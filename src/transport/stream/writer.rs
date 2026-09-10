//! The writer workers, plaintext and TLS.
//!
//! A writer worker only exists once a write could not complete inline. It then
//! owns the write side of the connection and drains `WriterCommand`s from the
//! channel, batching everything already queued into consecutive `write` calls
//! before it reports the buffer drained — which is what releases protocol
//! backpressure.
//!
//! The TLS writer additionally owns the close-notify shutdown, bounded by the
//! session's shutdown timeout so a peer that stops reading cannot hold the
//! worker open.

use std::io::{self, Write as _};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use pyo3::exceptions::{PyRuntimeError, PyTimeoutError};

use super::buffers::OwnedWriteBuffer;
use super::io_targets::{WriterTarget, is_no_buffer_space_code};
use super::tls_session::{
    SharedTlsIoState, abort_tls_writer, close_tls_writer, flush_tls_io_locked,
};
use super::tuning::{BLOCKING_POLL_INTERVAL_MS, TLS_WORKER_STACK_SIZE};
use super::worker::WorkerThread;
use super::write_queue::{TryRecvError, WriterReceiver};
use super::{StreamTransportCore, WriterCommand};
use crate::fd_ops;

pub(super) fn spawn_writer_worker(
    core: Arc<StreamTransportCore>,
    writer: WriterTarget,
    writer_rx: WriterReceiver,
) -> io::Result<()> {
    let thread_core = Arc::clone(&core);
    let worker = WorkerThread::spawn("rsloop-stream-writer", move |stop| {
        run_stream_writer(thread_core, writer, writer_rx, stop)
    })?;
    core.register_worker(worker);
    Ok(())
}

pub(super) fn spawn_tls_writer_worker(
    core: Arc<StreamTransportCore>,
    tls_state: SharedTlsIoState,
    writer_rx: WriterReceiver,
) -> io::Result<()> {
    let thread_core = Arc::clone(&core);
    let worker = WorkerThread::spawn_with_stack(
        "rsloop-tls-writer",
        Some(TLS_WORKER_STACK_SIZE),
        move |stop| run_tls_writer(thread_core, tls_state, writer_rx, stop),
    )?;
    core.register_worker(worker);
    Ok(())
}

pub(super) fn run_stream_writer(
    core: Arc<StreamTransportCore>,
    mut writer: WriterTarget,
    writer_rx: WriterReceiver,
    stop: Arc<AtomicBool>,
) {
    let mut pending_command = None;

    loop {
        if stop.load(Ordering::Acquire) {
            return;
        }
        let command = match pending_command.take() {
            Some(command) => command,
            None => match writer_rx.recv() {
                Ok(command) => command,
                Err(()) => break,
            },
        };

        if handle_stream_writer_command(
            &core,
            &mut writer,
            &writer_rx,
            command,
            &mut pending_command,
        ) {
            continue;
        }
        return;
    }

    core.report_connection_lost_result(core.connection_lost(None));
}

pub(super) fn handle_stream_writer_command(
    core: &Arc<StreamTransportCore>,
    writer: &mut WriterTarget,
    writer_rx: &WriterReceiver,
    command: WriterCommand,
    pending_command: &mut Option<WriterCommand>,
) -> bool {
    match command {
        WriterCommand::Data(data) => {
            write_stream_data_batch(core, writer, writer_rx, data, pending_command)
        }
        WriterCommand::WriteEof => handle_stream_write_eof(core, writer),
        WriterCommand::Close => {
            report_writer_close_result(core, writer.shutdown_write());
            false
        }
        WriterCommand::Abort => {
            report_writer_close_result(core, writer.shutdown_close());
            false
        }
        WriterCommand::Stop => false,
    }
}

pub(super) fn write_stream_data_batch(
    core: &Arc<StreamTransportCore>,
    writer: &mut WriterTarget,
    writer_rx: &WriterReceiver,
    mut data: OwnedWriteBuffer,
    pending_command: &mut Option<WriterCommand>,
) -> bool {
    if !write_one_stream_buffer(core, writer, &mut data) {
        return false;
    }

    loop {
        match writer_rx.try_recv() {
            Ok(WriterCommand::Data(mut next)) => {
                if !write_one_stream_buffer(core, writer, &mut next) {
                    return false;
                }
            }
            Ok(command) => {
                *pending_command = Some(command);
                break;
            }
            Err(TryRecvError::Empty) => {
                core.set_write_backpressure_active(false);
                break;
            }
            Err(TryRecvError::Disconnected) => {
                core.set_write_backpressure_active(false);
                core.report_connection_lost_result(core.connection_lost(None));
                return false;
            }
        }
    }
    if pending_command.is_none() {
        core.set_write_backpressure_active(false);
    }
    true
}

pub(super) fn write_one_stream_buffer(
    core: &Arc<StreamTransportCore>,
    writer: &mut WriterTarget,
    data: &mut OwnedWriteBuffer,
) -> bool {
    let buffered_len = data.remaining().len();
    if let Err(err) = write_all_owned(writer, data) {
        report_writer_io_error(core, err);
        return false;
    }
    core.record_write_buffer_drained(buffered_len);
    true
}

pub(super) fn handle_stream_write_eof(
    core: &Arc<StreamTransportCore>,
    writer: &mut WriterTarget,
) -> bool {
    if let Err(err) = writer.shutdown_write() {
        report_writer_io_error(core, err);
        return false;
    }
    if core.close_on_write_eof() {
        core.report_connection_lost_result(core.connection_lost(None));
        return false;
    }
    true
}

pub(super) fn report_writer_io_error(core: &Arc<StreamTransportCore>, err: io::Error) {
    core.report_connection_lost_result(
        core.connection_lost(Some(PyRuntimeError::new_err(err.to_string()))),
    );
}

pub(super) fn report_writer_close_result(core: &Arc<StreamTransportCore>, result: io::Result<()>) {
    core.report_connection_lost_result(
        core.connection_lost(
            result
                .err()
                .map(|err| PyRuntimeError::new_err(err.to_string())),
        ),
    );
}

pub(super) fn run_tls_writer(
    core: Arc<StreamTransportCore>,
    tls_state: SharedTlsIoState,
    writer_rx: WriterReceiver,
    stop: Arc<AtomicBool>,
) {
    let mut pending_command = None;

    loop {
        if stop.load(Ordering::Acquire) {
            return;
        }
        let command = match pending_command.take() {
            Some(command) => command,
            None => match writer_rx.recv() {
                Ok(command) => command,
                Err(()) => break,
            },
        };

        if handle_tls_writer_command(&core, &tls_state, &writer_rx, command, &mut pending_command) {
            continue;
        }
        return;
    }

    core.report_connection_lost_result(core.connection_lost(None));
}

pub(super) fn handle_tls_writer_command(
    core: &Arc<StreamTransportCore>,
    tls_state: &SharedTlsIoState,
    writer_rx: &WriterReceiver,
    command: WriterCommand,
    pending_command: &mut Option<WriterCommand>,
) -> bool {
    match command {
        WriterCommand::Data(data) => {
            write_tls_data_batch(core, tls_state, writer_rx, data, pending_command)
        }
        WriterCommand::WriteEof => true,
        WriterCommand::Close => {
            report_tls_close_result(core, close_tls_writer(tls_state));
            false
        }
        WriterCommand::Abort => {
            report_writer_close_result(core, abort_tls_writer(tls_state));
            false
        }
        WriterCommand::Stop => false,
    }
}

pub(super) fn write_tls_data_batch(
    core: &Arc<StreamTransportCore>,
    tls_state: &SharedTlsIoState,
    writer_rx: &WriterReceiver,
    data: OwnedWriteBuffer,
    pending_command: &mut Option<WriterCommand>,
) -> bool {
    let mut buffered_len = 0;
    let mut state = tls_state.lock().expect("poisoned tls state");
    if let Err(err) = state.connection.writer_write_all(data.remaining()) {
        drop(state);
        report_writer_io_error(core, err);
        return false;
    }
    buffered_len += data.remaining().len();
    loop {
        match writer_rx.try_recv() {
            Ok(WriterCommand::Data(next)) => {
                if let Err(err) = state.connection.writer_write_all(next.remaining()) {
                    drop(state);
                    report_writer_io_error(core, err);
                    return false;
                }
                buffered_len += next.remaining().len();
            }
            Ok(command) => {
                *pending_command = Some(command);
                break;
            }
            Err(TryRecvError::Empty) => {
                core.set_write_backpressure_active(false);
                break;
            }
            Err(TryRecvError::Disconnected) => {
                core.set_write_backpressure_active(false);
                core.report_connection_lost_result(core.connection_lost(None));
                return false;
            }
        }
    }

    // Feed all immediately available plaintext into rustls before flushing
    // encrypted records. This turns common header/body write pairs into one
    // socket-flush pass and one TLS-state lock acquisition.
    if let Err(err) = flush_tls_io_locked(&mut state) {
        drop(state);
        report_writer_io_error(core, err);
        return false;
    }
    drop(state);
    core.record_write_buffer_drained(buffered_len);

    if pending_command.is_none() {
        core.set_write_backpressure_active(false);
    }
    true
}

pub(super) fn report_tls_close_result(core: &Arc<StreamTransportCore>, result: io::Result<()>) {
    match result {
        Ok(()) => core.report_connection_lost_result(core.connection_lost(None)),
        Err(err) if err.kind() == io::ErrorKind::TimedOut => core.report_connection_lost_result(
            core.connection_lost(Some(PyTimeoutError::new_err("SSL shutdown timed out"))),
        ),
        Err(err) => report_writer_io_error(core, err),
    }
}

pub(super) fn write_all_owned(
    writer: &mut WriterTarget,
    data: &mut OwnedWriteBuffer,
) -> io::Result<()> {
    while !data.is_empty() {
        match writer.write(data.remaining()) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "failed to write buffered transport data",
                ));
            }
            Ok(written) => data.advance(written),
            Err(err) if is_transient_write_backpressure(&err) => {
                // Some kernels report temporary mbuf exhaustion as ENOBUFS
                // instead of EWOULDBLOCK on nonblocking TCP sockets. Treat it
                // as backpressure; failing the transport here truncates data
                // that was already accepted by asyncio's write contract.
                if err.kind() != io::ErrorKind::WouldBlock {
                    thread::sleep(Duration::from_millis(1));
                }
                if let Some(fd) = writer.fd().filter(|_| writer.pollable()) {
                    loop {
                        match fd_ops::poll_fd(fd, false, true, BLOCKING_POLL_INTERVAL_MS) {
                            Ok((false, true)) => break,
                            Ok(_) => continue,
                            Err(err) => return Err(err),
                        }
                    }
                } else {
                    thread::sleep(Duration::from_millis(10));
                }
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    }

    Ok(())
}

#[inline]
pub(super) fn is_transient_write_backpressure(err: &io::Error) -> bool {
    if err.kind() == io::ErrorKind::WouldBlock {
        return true;
    }
    is_no_buffer_space_code(err.raw_os_error())
}

#[cfg(test)]
mod transient_write_tests {
    use std::io;

    use super::is_transient_write_backpressure;

    #[test]
    fn would_block_is_transient_write_backpressure() {
        assert!(is_transient_write_backpressure(&io::Error::from(
            io::ErrorKind::WouldBlock
        )));
    }

    #[cfg(unix)]
    #[test]
    fn no_buffer_space_is_transient_write_backpressure() {
        assert!(is_transient_write_backpressure(
            &io::Error::from_raw_os_error(libc::ENOBUFS)
        ));
    }
}
