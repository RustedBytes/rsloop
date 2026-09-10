//! The blocking reader worker and the control path around it.
//!
//! `run_stream_reader` is a plain thread reading into a pooled buffer and
//! enqueueing events on the core. After a successful read it spins on
//! non-blocking reads for `reader_spin_window()` before falling back to
//! `poll()`, which removes the sleep/wake pair from a request/response
//! round trip.
//!
//! Sockets are read on a runtime instead (see `reader_task`). Local readers
//! cancel immediately; coordination-thread readers use an acknowledged command
//! so `start_tls` can take exclusive ownership of the socket. Close and abort
//! use the `_nowait` form to keep teardown off the critical path.

use std::io::{self, Read as _};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::time::Duration;

use super::io_targets::ReaderTarget;
use super::poll::wait_socket_ready;
use super::tls_session::{
    SharedTlsIoState, TlsReadOutcome, drain_buffered_tls_plaintext, read_tls_records,
    tls_socket_wait_target,
};
use super::tuning::{
    BLOCKING_POLL_INTERVAL_MS, STREAM_READ_BUFFER_SIZE, TLS_WORKER_STACK_SIZE, reader_spin_window,
};
use super::worker::WorkerThread;
use super::{PendingReadEvent, StreamTransportCore};
use crate::engine::{LoopCommand, LoopIoCommand};
use crate::fd_ops;

pub(super) fn spawn_reader_worker(
    core: Arc<StreamTransportCore>,
    reader: ReaderTarget,
) -> io::Result<()> {
    let thread_core = Arc::clone(&core);
    let worker = WorkerThread::spawn("rsloop-stream-reader", move |stop| {
        run_stream_reader(thread_core, reader, stop)
    })?;
    core.register_worker(worker);
    Ok(())
}

pub(super) fn spawn_tls_reader_worker(
    core: Arc<StreamTransportCore>,
    tls_state: SharedTlsIoState,
) -> io::Result<()> {
    let thread_core = Arc::clone(&core);
    let worker = WorkerThread::spawn_with_stack(
        "rsloop-tls-reader",
        Some(TLS_WORKER_STACK_SIZE),
        move |stop| run_tls_reader(thread_core, tls_state, stop),
    )?;
    core.register_worker(worker);
    Ok(())
}
pub(super) fn run_stream_reader(
    core: Arc<StreamTransportCore>,
    mut reader: ReaderTarget,
    stop: Arc<AtomicBool>,
) {
    let Some(mut buf) =
        core.acquire_read_buffer_blocking(STREAM_READ_BUFFER_SIZE, Some(stop.as_ref()))
    else {
        return;
    };
    let spin_window = reader_spin_window();

    loop {
        if stop.load(Ordering::Acquire) {
            return;
        }
        if core.is_closing() {
            core.enqueue_pending_read_event(PendingReadEvent::ConnectionLost(None));
            return;
        }

        core.wait_until_readable();
        if core.is_closing() {
            core.enqueue_pending_read_event(PendingReadEvent::ConnectionLost(None));
            return;
        }

        if reader.pollable() {
            match fd_ops::poll_fd(reader.fd(), true, false, BLOCKING_POLL_INTERVAL_MS) {
                Ok((false, _)) => continue,
                Ok((true, _)) => {}
                Err(err) => {
                    core.enqueue_pending_read_event(PendingReadEvent::ConnectionLost(Some(
                        err.to_string(),
                    )));
                    return;
                }
            }
        }

        buf.resize(buf.capacity(), 0);
        match reader.read(&mut buf) {
            Ok(0) => {
                core.enqueue_pending_read_event(PendingReadEvent::Eof);
                return;
            }
            Ok(n) => {
                buf.truncate(n);
                core.enqueue_pending_read_event(PendingReadEvent::Data(buf));
                let Some(next) =
                    core.acquire_read_buffer_blocking(STREAM_READ_BUFFER_SIZE, Some(stop.as_ref()))
                else {
                    return;
                };
                buf = next;
                if !spin_window.is_zero()
                    && !spin_read_stream(&core, &mut reader, &stop, &mut buf, spin_window)
                {
                    return;
                }
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => continue,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => {
                core.enqueue_pending_read_event(PendingReadEvent::ConnectionLost(Some(
                    err.to_string(),
                )));
                return;
            }
        }
    }
}

/// Retry non-blocking reads for a bounded window after a successful read.
/// Returns `false` when the connection terminated (event already enqueued)
/// and the reader loop must exit.
pub(super) fn spin_read_stream(
    core: &Arc<StreamTransportCore>,
    reader: &mut ReaderTarget,
    stop: &Arc<AtomicBool>,
    buf: &mut Vec<u8>,
    spin_window: Duration,
) -> bool {
    let mut deadline = std::time::Instant::now() + spin_window;
    loop {
        // Keep the hot path to atomics only: the closing/paused states are
        // re-checked by the outer loop within `spin_window` at the latest,
        // and after every successful read below.
        if stop.load(Ordering::Acquire) {
            return true;
        }

        buf.resize(buf.capacity(), 0);
        match reader.read(buf) {
            Ok(0) => {
                core.enqueue_pending_read_event(PendingReadEvent::Eof);
                return false;
            }
            Ok(n) => {
                buf.truncate(n);
                let data = std::mem::take(buf);
                core.enqueue_pending_read_event(PendingReadEvent::Data(data));
                let Some(next) =
                    core.acquire_read_buffer_blocking(STREAM_READ_BUFFER_SIZE, Some(stop.as_ref()))
                else {
                    return false;
                };
                *buf = next;
                if core.is_closing() || !core.is_reading() {
                    return true;
                }
                deadline = std::time::Instant::now() + spin_window;
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                if std::time::Instant::now() >= deadline {
                    return true;
                }
                std::hint::spin_loop();
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => {
                core.enqueue_pending_read_event(PendingReadEvent::ConnectionLost(Some(
                    err.to_string(),
                )));
                return false;
            }
        }
    }
}

/// Starts the socket reader for a stream transport.
///
/// Active Unix loops route generic-protocol TCP readers to their loop-thread
/// runtime. Native fast streams, Unix-domain sockets, and Windows readers retain
/// the coordination-thread path. Both stop
/// helpers check loop-thread task ownership before sending a dispatcher command,
/// so `start_tls` drops a local reader before reclaiming its socket.
pub(super) fn spawn_socket_reader(
    fd: fd_ops::RawFd,
    core: Arc<StreamTransportCore>,
    reader: ReaderTarget,
) -> Result<(), crate::engine::LoopCoreError> {
    let loop_core = Arc::clone(&core.loop_core);
    loop_core.send_command(LoopCommand::Io(LoopIoCommand::StartSocketReader {
        fd,
        core,
        reader,
    }))
}

/// Stops a local reader immediately, or waits for coordination-thread cancellation.
pub(super) fn stop_socket_reader(core: &StreamTransportCore, fd: fd_ops::RawFd) -> io::Result<()> {
    if core.loop_core.stop_io_task(fd) {
        return Ok(());
    }
    let (done_tx, done_rx) = mpsc::channel();
    core.loop_core
        .send_command(LoopCommand::Io(LoopIoCommand::StopSocketReader {
            fd,
            done_tx,
        }))
        .map_err(io::Error::other)?;
    // start_tls duplicates the socket before reaching this point. Do not let
    // the TLS handshake use that duplicate until the runtime-thread reader has
    // been dropped, otherwise it can consume handshake bytes.
    done_rx
        .recv_timeout(Duration::from_secs(5))
        .map_err(|err| io::Error::new(io::ErrorKind::TimedOut, err))
}

/// Requests reader cancellation without synchronously crossing to the runtime
/// thread. Normal close/abort already mark the transport closing and shut down
/// the socket, so waiting for the acknowledgement only adds teardown latency.
/// TLS upgrade continues to use `stop_socket_reader`, where exclusive access to
/// the underlying stream is required before handshake bytes can be consumed.
pub(super) fn stop_socket_reader_nowait(
    core: &StreamTransportCore,
    fd: fd_ops::RawFd,
) -> io::Result<()> {
    if core.loop_core.stop_io_task(fd) {
        return Ok(());
    }
    let (done_tx, _done_rx) = mpsc::channel();
    core.loop_core
        .send_command(LoopCommand::Io(LoopIoCommand::StopSocketReader {
            fd,
            done_tx,
        }))
        .map_err(io::Error::other)
}
pub(crate) fn run_socket_reader_blocking(
    core: Arc<StreamTransportCore>,
    reader: ReaderTarget,
    stop: Arc<AtomicBool>,
) {
    run_stream_reader(core, reader, stop)
}

pub(super) fn run_tls_reader(
    core: Arc<StreamTransportCore>,
    tls_state: SharedTlsIoState,
    stop: Arc<AtomicBool>,
) {
    let Some(mut plaintext) =
        core.acquire_read_buffer_blocking(STREAM_READ_BUFFER_SIZE, Some(stop.as_ref()))
    else {
        return;
    };

    loop {
        if stop.load(Ordering::Acquire) {
            return;
        }
        if core.is_closing() {
            core.enqueue_pending_read_event(PendingReadEvent::ConnectionLost(None));
            return;
        }

        core.wait_until_readable();
        if core.is_closing() {
            core.enqueue_pending_read_event(PendingReadEvent::ConnectionLost(None));
            return;
        }

        match drain_buffered_tls_plaintext(&core, &tls_state, &mut plaintext) {
            TlsReadOutcome::Continue => continue,
            TlsReadOutcome::Eof => {}
            TlsReadOutcome::ConnectionLost(err) => {
                core.enqueue_pending_read_event(PendingReadEvent::ConnectionLost(Some(err)));
                return;
            }
        }

        let (fd, pollable) = tls_socket_wait_target(&tls_state);
        if let Err(err) = wait_socket_ready(fd, pollable, true, false) {
            core.enqueue_pending_read_event(PendingReadEvent::ConnectionLost(Some(
                err.to_string(),
            )));
            return;
        }

        match read_tls_records(&core, &tls_state, &mut plaintext) {
            TlsReadOutcome::Continue => continue,
            TlsReadOutcome::Eof => {
                core.enqueue_pending_read_event(PendingReadEvent::Eof);
                return;
            }
            TlsReadOutcome::ConnectionLost(err) => {
                core.enqueue_pending_read_event(PendingReadEvent::ConnectionLost(Some(err)));
                return;
            }
        }
    }
}
