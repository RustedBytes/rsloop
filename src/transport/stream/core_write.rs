//! The write path: direct writes, staging, and protocol backpressure.
//!
//! A write is attempted straight on the socket first, because most responses
//! fit in the kernel buffer and never need a writer thread. What does not fit
//! is queued to the writer worker, which is only started at that point.
//!
//! Ordinary protocol writes get staged for one loop turn
//! (`SMALL_WRITE_COALESCE_*`) so a header and body share a syscall, a batch of
//! ready connections can finish their callbacks before waking peer readers, and
//! the whole batch of writes wakes a peer reader once rather than per message
//! (see `builder` for which transports opt in). Buffered bytes are tracked
//! against the high/low water marks and translated into
//! `pause_writing`/`resume_writing` on the protocol.

use std::io::{self, Write as _};
use std::sync::Arc;
use std::sync::atomic::Ordering;

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;

use super::buffers::OwnedWriteBuffer;
use super::io_targets::{StreamKind, TaskedDirectWriter};
use super::stats::{
    TRANSPORT_DIRECT_WRITE_ATTEMPTS, TRANSPORT_STAGED_WRITES, transport_stats_enabled,
};
#[cfg(windows)]
use super::tuning::SERVER_POLL_READER_WRITE_THRESHOLD;
use super::tuning::{
    SMALL_WRITE_COALESCE_MAX_BYTES, SMALL_WRITE_COALESCE_MIN_BYTES, max_write_buffer_size,
};
#[cfg(unix)]
use super::unix_stream_from_owned_socket_fd;
use super::writer::is_transient_write_backpressure;
use super::{
    PendingReadEvent, StreamTransportCore, TransportSpawnContext, WriterCommand,
    stop_socket_reader, tcp_stream_from_owned_socket_fd,
};
use crate::engine::{LoopCommand, LoopTransportCommand};
use crate::fd_ops;

#[inline]
fn is_write_batch_candidate(len: usize) -> bool {
    len > SMALL_WRITE_COALESCE_MIN_BYTES && len <= SMALL_WRITE_COALESCE_MAX_BYTES
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum WriteBufferSignal {
    None,
    Pause,
    Resume,
}

fn enqueue_write_buffer(
    state: &mut super::StreamWriteBufferState,
    len: usize,
    cap: usize,
) -> Result<WriteBufferSignal, ()> {
    if len == 0 {
        return Ok(WriteBufferSignal::None);
    }
    if len > cap.saturating_sub(state.size) {
        return Err(());
    }

    state.size += len;
    if state.size > state.high_water && !state.protocol_paused {
        state.protocol_paused = true;
        return Ok(WriteBufferSignal::Pause);
    }
    Ok(WriteBufferSignal::None)
}

fn drain_write_buffer(state: &mut super::StreamWriteBufferState, len: usize) -> WriteBufferSignal {
    state.size = state.size.saturating_sub(len);
    if state.protocol_paused && state.size <= state.low_water {
        state.protocol_paused = false;
        WriteBufferSignal::Resume
    } else {
        WriteBufferSignal::None
    }
}

fn clear_write_buffer_state(
    state: &mut super::StreamWriteBufferState,
    resume_protocol: bool,
) -> WriteBufferSignal {
    let signal = if resume_protocol && state.protocol_paused {
        WriteBufferSignal::Resume
    } else {
        WriteBufferSignal::None
    };
    state.size = 0;
    state.protocol_paused = false;
    signal
}

pub(super) fn reconcile_write_buffer_limits(
    state: &mut super::StreamWriteBufferState,
    low_water: usize,
    high_water: usize,
) -> WriteBufferSignal {
    state.low_water = low_water;
    state.high_water = high_water;
    if state.size > high_water && !state.protocol_paused {
        state.protocol_paused = true;
        WriteBufferSignal::Pause
    } else if state.protocol_paused && state.size <= low_water {
        state.protocol_paused = false;
        WriteBufferSignal::Resume
    } else {
        WriteBufferSignal::None
    }
}

impl StreamTransportCore {
    #[inline]
    pub(super) fn write_backpressure_active(&self) -> bool {
        self.state
            .lock()
            .expect("poisoned transport state")
            .writer_registered
    }

    #[inline]
    pub(super) fn set_write_backpressure_active(&self, active: bool) {
        self.state
            .lock()
            .expect("poisoned transport state")
            .writer_registered = active;
    }

    pub(super) fn close_on_write_eof(&self) -> bool {
        self.state
            .lock()
            .expect("poisoned transport state")
            .close_on_write_eof
    }

    pub(super) fn try_direct_tasked_write(&self, data: &[u8]) -> io::Result<usize> {
        crate::profile_scope!("StreamTransportCore::try_direct_tasked_write");
        if transport_stats_enabled() {
            TRANSPORT_DIRECT_WRITE_ATTEMPTS.fetch_add(1, Ordering::Relaxed);
        }
        let Some(writer) = &self.direct_writer else {
            return Err(io::Error::other("not direct-tasked"));
        };
        let mut writer = writer.lock().expect("poisoned direct tasked writer");
        let Some(writer) = writer.as_mut() else {
            return Err(io::Error::new(
                io::ErrorKind::NotConnected,
                "direct writer is closed",
            ));
        };
        match writer {
            TaskedDirectWriter::Tcp(stream) => {
                #[cfg(windows)]
                // A paused reader does not consume Winsock's asynchronous reset
                // notification. Surface it before another small write can appear
                // to succeed from the local send buffer.
                if !self.is_reading()
                    && let Some(err) = stream.take_error()?
                {
                    return Err(err);
                }

                stream.as_ref().write(data)
            }
            #[cfg(unix)]
            TaskedDirectWriter::Unix(stream) => stream.write(data),
        }
    }

    pub(super) fn fail_write(self: &Arc<Self>, err: Option<io::Error>) {
        if self.is_closing() {
            return;
        }

        // Queue the write error before waking a paused reader. Otherwise the
        // reader can observe `closing` and race this event with a clean loss.
        self.enqueue_pending_read_event(PendingReadEvent::ConnectionLost(
            err.map(|err| err.to_string()),
        ));
        self.set_closing();
        self.set_write_backpressure_active(false);
        self.pending_direct_write
            .lock()
            .expect("poisoned pending direct write")
            .take();
        self.direct_write_scheduled.store(false, Ordering::Release);
        self.clear_write_buffer(false);
        let _ = self.writer_tx.send(WriterCommand::Stop);
    }

    pub(super) fn queue_write(self: &Arc<Self>, data: OwnedWriteBuffer) -> io::Result<()> {
        let should_pause = self.record_write_buffer_enqueued(data.remaining().len())?;
        self.ensure_writer_worker();
        if should_pause {
            self.notify_pause_writing();
        }
        if self.writer_tx.send(WriterCommand::Data(data)).is_err() {
            self.clear_write_buffer(false);
            self.fail_write(None);
        }
        Ok(())
    }

    pub(super) fn queue_recorded_write(self: &Arc<Self>, data: OwnedWriteBuffer) {
        self.ensure_writer_worker();
        if self.writer_tx.send(WriterCommand::Data(data)).is_err() {
            self.clear_write_buffer(false);
            self.fail_write(None);
        }
    }

    pub(super) fn stage_direct_write(self: &Arc<Self>, data: &[u8]) -> io::Result<()> {
        if data.is_empty() {
            return Ok(());
        }
        if transport_stats_enabled() {
            TRANSPORT_STAGED_WRITES.fetch_add(1, Ordering::Relaxed);
        }

        let should_pause = self.record_write_buffer_enqueued(data.len())?;
        let mut pending = self
            .pending_direct_write
            .lock()
            .expect("poisoned pending direct write");
        let buffer = pending.get_or_insert_with(|| self.new_pooled_write_buffer(data.len()));
        buffer.extend_from_slice(data);
        drop(pending);
        if should_pause {
            self.notify_pause_writing();
        }

        if !self.direct_write_scheduled.swap(true, Ordering::AcqRel)
            && self
                .loop_core
                .send_command(LoopCommand::Transport(LoopTransportCommand::StreamWrite(
                    Arc::clone(self),
                )))
                .is_err()
        {
            self.direct_write_scheduled.store(false, Ordering::Release);
            self.fail_write(None);
        }
        Ok(())
    }

    pub(crate) fn flush_pending_direct_write(self: &Arc<Self>) {
        crate::profile_scope!("StreamTransportCore::flush_pending_direct_write");
        #[cfg(windows)]
        if self.poll_reader_requested() && !self.poll_reader_ready.load(Ordering::Acquire) {
            return;
        }
        self.direct_write_scheduled.store(false, Ordering::Release);
        let Some(mut data) = self
            .pending_direct_write
            .lock()
            .expect("poisoned pending direct write")
            .take()
        else {
            return;
        };
        if self.is_closing() {
            self.record_write_buffer_drained(data.len());
            return;
        }

        match self.try_direct_tasked_write(data.remaining()) {
            Ok(written) if written == data.len() => {
                self.record_write_buffer_drained(written);
            }
            Ok(written) => {
                self.record_write_buffer_drained(written);
                data.advance(written);
                self.set_write_backpressure_active(true);
                self.queue_recorded_write(data);
            }
            Err(err)
                if err.kind() == io::ErrorKind::Interrupted
                    || is_transient_write_backpressure(&err) =>
            {
                self.set_write_backpressure_active(true);
                self.queue_recorded_write(data);
            }
            Err(err) => self.fail_write(Some(err)),
        }
    }

    pub(super) fn discard_pending_direct_write(self: &Arc<Self>) {
        self.direct_write_scheduled.store(false, Ordering::Release);
        let discarded = {
            let mut pending = self
                .pending_direct_write
                .lock()
                .expect("poisoned pending direct write");
            pending.take().map_or(0, |buffer| buffer.len())
        };
        self.record_write_buffer_drained(discarded);
    }

    pub(super) fn try_write_bytes(self: &Arc<Self>, data: &[u8]) -> io::Result<()> {
        crate::profile_scope!("StreamTransportCore::try_write_bytes");
        #[cfg(windows)]
        if self.direct_writer.is_some()
            && (!self.server_side || data.len() >= SERVER_POLL_READER_WRITE_THRESHOLD)
        {
            // Completion reads require a blocking Winsock handle. Never write
            // synchronously through that shared handle on the loop thread: a
            // full send buffer would park the entire event loop. Ensure the
            // reader is in readiness mode first, which also makes the socket
            // nonblocking, and stage writes while any transition completes.
            self.request_poll_reader();
            if !self.poll_reader_ready.load(Ordering::Acquire) {
                return self.stage_direct_write(data);
            }
        }

        if self.direct_writer.is_some() && !self.write_backpressure_active() {
            if self.direct_write_scheduled.load(Ordering::Acquire)
                || (self.coalesce_small_writes && is_write_batch_candidate(data.len()))
            {
                return self.stage_direct_write(data);
            }
            match self.try_direct_tasked_write(data) {
                Ok(written) if written == data.len() => return Ok(()),
                Ok(written) => {
                    let mut pending =
                        OwnedWriteBuffer::from_pooled_slice(data, &self.write_buffer_pool);
                    pending.advance(written);
                    self.set_write_backpressure_active(true);
                    return self.queue_write(pending);
                }
                Err(err)
                    if err.kind() == io::ErrorKind::Interrupted
                        || is_transient_write_backpressure(&err) =>
                {
                    self.set_write_backpressure_active(true);
                    return self.queue_write(OwnedWriteBuffer::from_pooled_slice(
                        data,
                        &self.write_buffer_pool,
                    ));
                }
                Err(err) => {
                    self.fail_write(Some(err));
                    return Ok(());
                }
            }
        }

        self.queue_write(OwnedWriteBuffer::from_pooled_slice(
            data,
            &self.write_buffer_pool,
        ))
    }

    pub(super) fn new_pooled_write_buffer(&self, capacity: usize) -> OwnedWriteBuffer {
        OwnedWriteBuffer::with_pooled_capacity(capacity, &self.write_buffer_pool)
    }

    pub(super) fn try_write_buffer(self: &Arc<Self>, mut data: OwnedWriteBuffer) -> io::Result<()> {
        if data.is_empty() {
            return Ok(());
        }

        #[cfg(windows)]
        if self.direct_writer.is_some()
            && (!self.server_side || data.remaining().len() >= SERVER_POLL_READER_WRITE_THRESHOLD)
        {
            self.request_poll_reader();
            if !self.poll_reader_ready.load(Ordering::Acquire) {
                return self.stage_direct_write(data.remaining());
            }
        }

        if self.direct_writer.is_some() && !self.write_backpressure_active() {
            if self.direct_write_scheduled.load(Ordering::Acquire)
                || (self.coalesce_small_writes && is_write_batch_candidate(data.remaining().len()))
            {
                return self.stage_direct_write(data.remaining());
            }
            match self.try_direct_tasked_write(data.remaining()) {
                Ok(written) if written == data.remaining().len() => return Ok(()),
                Ok(written) => {
                    data.advance(written);
                    self.set_write_backpressure_active(true);
                    return self.queue_write(data);
                }
                Err(err)
                    if err.kind() == io::ErrorKind::Interrupted
                        || is_transient_write_backpressure(&err) =>
                {
                    self.set_write_backpressure_active(true);
                    return self.queue_write(data);
                }
                Err(err) => {
                    self.fail_write(Some(err));
                    return Ok(());
                }
            }
        }

        self.queue_write(data)
    }

    pub async fn wait_readable(self: &Arc<Self>) -> io::Result<()> {
        Err(io::Error::other(
            "transport readiness is not used in std transport mode",
        ))
    }

    pub async fn wait_writable(self: &Arc<Self>) -> io::Result<()> {
        Err(io::Error::other(
            "transport readiness is not used in std transport mode",
        ))
    }

    pub fn handle_read_ready_with_py(self: &Arc<Self>, _py: Python<'_>) {}

    pub fn handle_write_ready_with_py(self: &Arc<Self>, _py: Python<'_>) {}

    pub(super) fn upgrade_stream(
        self: &Arc<Self>,
        py: Python<'_>,
    ) -> PyResult<(TransportSpawnContext, StreamKind)> {
        self.flush_pending_direct_write();
        let protocol = self.get_protocol(py);
        let context = self
            .state
            .lock()
            .expect("poisoned transport state")
            .context
            .clone_ref(py);
        let context_needs_run = self
            .state
            .lock()
            .expect("poisoned transport state")
            .context_needs_run;
        let socket = self
            .get_extra(py, "socket")
            .ok_or_else(|| PyRuntimeError::new_err("transport does not expose a socket"))?;
        let fd = fd_ops::dup_raw_fd(socket.bind(py).call_method0("fileno")?.extract()?)
            .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;

        #[cfg(windows)]
        if self.runtime_socket_fd().is_some() {
            self.request_poll_reader();
            self.wait_for_poll_reader()
                .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
        }

        self.detach_underlying_stream(py);
        let _ = self.writer_tx.send(WriterCommand::Stop);
        if let Some(fd) = self.runtime_socket_fd() {
            stop_socket_reader(self, fd).map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
        }
        self.abort_workers();
        self.release_direct_writer();

        #[allow(unused_variables)]
        let family = socket.bind(py).getattr("family")?.extract::<i32>()?;
        #[cfg(unix)]
        let stream = if family == libc::AF_UNIX {
            StreamKind::Unix(unix_stream_from_owned_socket_fd(fd)?)
        } else {
            StreamKind::Tcp(tcp_stream_from_owned_socket_fd(fd)?)
        };
        #[cfg(not(unix))]
        let stream = StreamKind::Tcp(tcp_stream_from_owned_socket_fd(fd)?);

        Ok((
            TransportSpawnContext::new(
                py,
                Arc::clone(&self.loop_core),
                &self.loop_obj,
                protocol,
                &context,
                context_needs_run,
            ),
            stream,
        ))
    }

    pub(super) fn pause_writing_with_py(&self, py: Python<'_>) -> PyResult<()> {
        let (callback, context, context_needs_run) = {
            let state = self.state.lock().expect("poisoned transport state");
            (
                state.callbacks.pause_writing.clone_ref(py),
                state.context.clone_ref(py),
                state.context_needs_run,
            )
        };

        if let Err(err) = self.call_protocol_method0(py, &callback, &context, context_needs_run) {
            self.report_error_with_py(py, err, "protocol.pause_writing() failed")?;
        }
        Ok(())
    }

    pub(super) fn resume_writing_with_py(&self, py: Python<'_>) -> PyResult<()> {
        let (callback, context, context_needs_run) = {
            let state = self.state.lock().expect("poisoned transport state");
            (
                state.callbacks.resume_writing.clone_ref(py),
                state.context.clone_ref(py),
                state.context_needs_run,
            )
        };

        if let Err(err) = self.call_protocol_method0(py, &callback, &context, context_needs_run) {
            self.report_error_with_py(py, err, "protocol.resume_writing() failed")?;
        }
        Ok(())
    }

    pub(super) fn notify_pause_writing(self: &Arc<Self>) {
        if self.loop_core.on_runtime_thread() {
            let _ = self.call_in_loop_context(|py| self.pause_writing_with_py(py));
            return;
        }

        self.enqueue_pending_read_event(PendingReadEvent::PauseWriting);
    }

    pub(super) fn notify_resume_writing(self: &Arc<Self>) {
        if self.loop_core.on_runtime_thread() {
            let _ = self.call_in_loop_context(|py| self.resume_writing_with_py(py));
            return;
        }

        self.enqueue_pending_read_event(PendingReadEvent::ResumeWriting);
    }

    pub(super) fn record_write_buffer_enqueued(&self, len: usize) -> io::Result<bool> {
        if len == 0 {
            return Ok(false);
        }

        let cap = max_write_buffer_size();
        let mut state = self.state.lock().expect("poisoned transport state");
        match enqueue_write_buffer(&mut state.write_buffer, len, cap) {
            Ok(signal) => Ok(signal == WriteBufferSignal::Pause),
            Err(()) => Err(io::Error::new(
                io::ErrorKind::OutOfMemory,
                format!("transport write buffer exceeds {cap} bytes"),
            )),
        }
    }

    pub(super) fn record_write_buffer_drained(self: &Arc<Self>, len: usize) {
        if len == 0 {
            return;
        }

        let should_resume = {
            let mut state = self.state.lock().expect("poisoned transport state");
            drain_write_buffer(&mut state.write_buffer, len) == WriteBufferSignal::Resume
        };

        if should_resume {
            self.notify_resume_writing();
        }
    }

    pub(super) fn clear_write_buffer(self: &Arc<Self>, resume_protocol: bool) {
        let should_resume = {
            let mut state = self.state.lock().expect("poisoned transport state");
            clear_write_buffer_state(&mut state.write_buffer, resume_protocol)
                == WriteBufferSignal::Resume
        };

        if should_resume {
            self.notify_resume_writing();
        }
    }
}

#[cfg(kani)]
mod verification {
    use super::{
        WriteBufferSignal, clear_write_buffer_state, drain_write_buffer, enqueue_write_buffer,
        reconcile_write_buffer_limits,
    };
    use crate::transport::stream::StreamWriteBufferState;

    const MODEL_CAP: usize = 32;
    const MODEL_OPERATIONS: usize = 6;

    fn assert_valid(state: &StreamWriteBufferState) {
        assert!(state.low_water <= state.high_water);
        assert!(state.size <= MODEL_CAP);
        if state.protocol_paused {
            assert!(state.size > state.low_water);
        } else {
            assert!(state.size <= state.high_water);
        }
    }

    #[kani::proof]
    #[kani::unwind(8)]
    fn extended_write_accounting_preserves_lifecycle_invariants() {
        let low_water = usize::from(kani::any::<u8>() % 16);
        let high_water = low_water + usize::from(kani::any::<u8>()) % (MODEL_CAP - low_water + 1);
        let size = usize::from(kani::any::<u8>()) % (MODEL_CAP + 1);
        let protocol_paused: bool = kani::any();
        kani::assume(
            (protocol_paused && size > low_water) || (!protocol_paused && size <= high_water),
        );

        let mut state = StreamWriteBufferState {
            size,
            high_water,
            low_water,
            protocol_paused,
        };
        let operations: [u8; MODEL_OPERATIONS] = kani::any();
        let amounts: [u8; MODEL_OPERATIONS] = kani::any();
        let secondary: [u8; MODEL_OPERATIONS] = kani::any();

        for index in 0..MODEL_OPERATIONS {
            assert_valid(&state);
            let before = state;
            match operations[index] % 5 {
                0 => {
                    let amount = usize::from(amounts[index] % 17);
                    match enqueue_write_buffer(&mut state, amount, MODEL_CAP) {
                        Ok(signal) => {
                            assert_eq!(state.size, before.size + amount);
                            assert_eq!(
                                signal == WriteBufferSignal::Pause,
                                amount > 0
                                    && before.size + amount > before.high_water
                                    && !before.protocol_paused
                            );
                            kani::cover!(signal == WriteBufferSignal::Pause);
                        }
                        Err(()) => {
                            assert_eq!(state, before);
                            assert!(amount > MODEL_CAP - before.size);
                            kani::cover!(true);
                        }
                    }
                }
                1 => {
                    let amount = usize::from(amounts[index]);
                    let signal = drain_write_buffer(&mut state, amount);
                    let expected_size = before.size.saturating_sub(amount);
                    assert_eq!(state.size, expected_size);
                    assert_eq!(
                        signal == WriteBufferSignal::Resume,
                        before.protocol_paused && expected_size <= before.low_water
                    );
                    kani::cover!(signal == WriteBufferSignal::Resume);
                }
                2 => {
                    let resume_protocol = secondary[index] & 1 == 1;
                    let signal = clear_write_buffer_state(&mut state, resume_protocol);
                    assert_eq!(state.size, 0);
                    assert!(!state.protocol_paused);
                    assert_eq!(
                        signal == WriteBufferSignal::Resume,
                        resume_protocol && before.protocol_paused
                    );
                }
                3 => {
                    let low = usize::from(amounts[index] % 16);
                    let high = low + usize::from(secondary[index]) % (MODEL_CAP - low + 1);
                    let signal = reconcile_write_buffer_limits(&mut state, low, high);
                    let expected_pause = before.size > high && !before.protocol_paused;
                    let expected_resume =
                        !expected_pause && before.protocol_paused && before.size <= low;
                    assert_eq!(signal == WriteBufferSignal::Pause, expected_pause);
                    assert_eq!(signal == WriteBufferSignal::Resume, expected_resume);
                    kani::cover!(signal == WriteBufferSignal::Pause);
                    kani::cover!(signal == WriteBufferSignal::Resume);
                }
                _ => {
                    // Connection loss clears accounting without invoking resume_writing.
                    let signal = clear_write_buffer_state(&mut state, false);
                    assert_eq!(signal, WriteBufferSignal::None);
                    assert_eq!(state.size, 0);
                    assert!(!state.protocol_paused);
                }
            }
            assert_valid(&state);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use pyo3::prelude::*;

    use super::{OwnedWriteBuffer, is_write_batch_candidate};
    use crate::transport::stream::test_support::{build_test_core, shutdown_test_core};
    use crate::transport::stream::tuning::{
        SMALL_WRITE_COALESCE_MAX_BYTES, SMALL_WRITE_COALESCE_MIN_BYTES, STREAM_READ_BUFFER_SIZE,
        max_write_buffer_size,
    };

    #[test]
    fn write_batch_range_tracks_the_normal_read_block() {
        assert_eq!(SMALL_WRITE_COALESCE_MAX_BYTES, STREAM_READ_BUFFER_SIZE);
        assert!(!is_write_batch_candidate(SMALL_WRITE_COALESCE_MIN_BYTES));
        assert!(is_write_batch_candidate(SMALL_WRITE_COALESCE_MIN_BYTES + 1));
        assert!(is_write_batch_candidate(SMALL_WRITE_COALESCE_MAX_BYTES));
        assert!(!is_write_batch_candidate(
            SMALL_WRITE_COALESCE_MAX_BYTES + 1
        ));
    }

    #[test]
    fn write_buffer_pauses_and_resumes_only_at_watermark_crossings() {
        crate::initialize_python_for_tests();
        Python::attach(|py| {
            let (core, writer_rx, loop_core, protocol) = build_test_core(py);
            core.set_write_buffer_limits(Some(10), Some(4))
                .expect("valid write buffer limits");

            core.queue_write(OwnedWriteBuffer::from_slice(&[0; 10]))
                .expect("write at high water");
            assert!(protocol.borrow(py).events.is_empty());

            core.queue_write(OwnedWriteBuffer::from_slice(&[0; 1]))
                .expect("write above high water");
            assert_eq!(protocol.borrow(py).events, ["pause"]);

            core.queue_write(OwnedWriteBuffer::from_slice(&[0; 5]))
                .expect("write while paused");
            assert_eq!(protocol.borrow(py).events, ["pause"]);

            core.record_write_buffer_drained(11);
            assert_eq!(core.get_write_buffer_size(), 5);
            assert_eq!(protocol.borrow(py).events, ["pause"]);

            core.record_write_buffer_drained(1);
            assert_eq!(core.get_write_buffer_size(), 4);
            assert_eq!(protocol.borrow(py).events, ["pause", "resume"]);

            core.record_write_buffer_drained(4);
            assert_eq!(core.get_write_buffer_size(), 0);
            assert_eq!(protocol.borrow(py).events, ["pause", "resume"]);

            shutdown_test_core(core, writer_rx, loop_core);
        });
    }

    #[test]
    fn changing_watermarks_reconciles_the_current_buffer_state() {
        crate::initialize_python_for_tests();
        Python::attach(|py| {
            let (core, writer_rx, loop_core, protocol) = build_test_core(py);
            core.set_write_buffer_limits(Some(10), Some(4))
                .expect("valid initial limits");
            core.queue_write(OwnedWriteBuffer::from_slice(&[0; 8]))
                .expect("buffer write");

            core.set_write_buffer_limits(Some(7), Some(3))
                .expect("lower limits");
            assert_eq!(core.get_write_buffer_limits(), (3, 7));
            assert_eq!(protocol.borrow(py).events, ["pause"]);

            core.set_write_buffer_limits(Some(20), Some(10))
                .expect("raise limits");
            assert_eq!(core.get_write_buffer_limits(), (10, 20));
            assert_eq!(protocol.borrow(py).events, ["pause", "resume"]);

            core.set_write_buffer_limits(Some(20), Some(10))
                .expect("unchanged limits");
            assert_eq!(protocol.borrow(py).events, ["pause", "resume"]);

            shutdown_test_core(core, writer_rx, loop_core);
        });
    }

    #[test]
    fn invalid_limits_and_connection_loss_leave_write_state_consistent() {
        crate::initialize_python_for_tests();
        Python::attach(|py| {
            let (core, writer_rx, loop_core, protocol) = build_test_core(py);
            core.set_write_buffer_limits(Some(10), Some(4))
                .expect("valid write buffer limits");

            let error = core
                .set_write_buffer_limits(Some(3), Some(4))
                .expect_err("low water above high water should fail");
            assert!(error.is_instance_of::<pyo3::exceptions::PyValueError>(py));
            assert_eq!(core.get_write_buffer_limits(), (4, 10));

            core.queue_write(OwnedWriteBuffer::from_slice(&[0; 11]))
                .expect("write above high water");
            assert_eq!(protocol.borrow(py).events, ["pause"]);
            assert_eq!(core.get_write_buffer_size(), 11);

            core.connection_lost_with_py(py, None)
                .expect("connection loss callback");
            assert_eq!(core.get_write_buffer_size(), 0);
            assert_eq!(protocol.borrow(py).events, ["pause", "lost"]);
            assert!(core.is_closing());
            assert!(
                !core
                    .state
                    .lock()
                    .expect("transport state")
                    .write_buffer
                    .protocol_paused
            );

            core.record_write_buffer_drained(11);
            assert_eq!(protocol.borrow(py).events, ["pause", "lost"]);

            shutdown_test_core(core, writer_rx, loop_core);
        });
    }

    #[test]
    fn panicking_protocol_callback_leaves_transport_locks_unpoisoned() {
        crate::initialize_python_for_tests();
        Python::attach(|py| {
            let (core, writer_rx, loop_core, protocol) = build_test_core(py);
            protocol.borrow_mut(py).panic_next_pause = true;
            core.set_write_buffer_limits(Some(10), Some(4))
                .expect("valid write buffer limits");

            let callback_panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                core.queue_write(OwnedWriteBuffer::from_slice(&[0; 11]))
                    .expect("write crossing high water");
            }));

            assert!(callback_panic.is_err());
            assert!(!core.state.is_poisoned());
            assert!(!loop_core.state.is_poisoned());
            assert_eq!(core.get_write_buffer_size(), 11);
            assert_eq!(protocol.borrow(py).pause_attempts, 1);
            assert!(protocol.borrow(py).events.is_empty());

            core.record_write_buffer_drained(11);
            core.queue_write(OwnedWriteBuffer::from_slice(&[0; 11]))
                .expect("write after callback panic");

            assert!(!core.state.is_poisoned());
            assert_eq!(protocol.borrow(py).pause_attempts, 2);
            assert_eq!(protocol.borrow(py).events, ["resume", "pause"]);

            core.clear_write_buffer(false);
            shutdown_test_core(core, writer_rx, loop_core);
        });
    }

    #[test]
    fn write_buffer_cap_rejects_growth_without_corrupting_accounting() {
        crate::initialize_python_for_tests();
        Python::attach(|py| {
            let (core, writer_rx, loop_core, _protocol) = build_test_core(py);
            let maximum = max_write_buffer_size();

            assert!(core.record_write_buffer_enqueued(0).is_ok());
            assert!(
                core.record_write_buffer_enqueued(maximum)
                    .expect("write at exact cap")
            );
            let err = core
                .record_write_buffer_enqueued(1)
                .expect_err("write above cap should fail");
            assert_eq!(err.kind(), io::ErrorKind::OutOfMemory);
            assert_eq!(core.get_write_buffer_size(), maximum);

            core.clear_write_buffer(false);
            shutdown_test_core(core, writer_rx, loop_core);
        });
    }

    #[test]
    fn closed_writer_channel_clears_accounting_and_closes_transport() {
        crate::initialize_python_for_tests();
        Python::attach(|py| {
            let (core, writer_rx, loop_core, _protocol) = build_test_core(py);
            drop(writer_rx);

            core.queue_write(OwnedWriteBuffer::from_slice(b"undeliverable"))
                .expect("channel failure is converted into transport loss");

            assert!(core.is_closing());
            assert_eq!(core.get_write_buffer_size(), 0);
            assert_eq!(
                core.pending_read_events
                    .lock()
                    .expect("pending read events")
                    .len(),
                1
            );

            loop_core.clear_runtime_thread();
            drop(core);
            loop_core.close().expect("close test loop");
        });
    }
}
