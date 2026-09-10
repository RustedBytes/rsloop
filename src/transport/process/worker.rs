//! The blocking threads that watch the child and its output pipes.
//!
//! One reader thread per output pipe blocks on `read` and enqueues what it gets;
//! a single waiter thread owns the `Child`, polls `try_wait`, and is the only
//! place signals are delivered — commands from Python arrive over the control
//! channel rather than touching the child from the loop thread.
//!
//! The waiter's poll interval doubles as the control-channel receive timeout, so
//! a `kill()` is acted on promptly without a second wakeup source. On exit it
//! closes stdin's pipe bookkeeping first, matching the order asyncio reports.

use std::io::Read;
#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;
use std::process::Child;
use std::sync::Arc;
use std::sync::mpsc::{Receiver, RecvTimeoutError};
use std::time::Duration;

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;

use super::params::BoxedProcessReader;
use super::{ProcessCommand, ProcessTransportCore};

const PROCESS_READER_BUFFER_SIZE: usize = 65_536;
const PROCESS_WAIT_POLL_INTERVAL: Duration = Duration::from_millis(20);

pub(super) fn report_process_result(
    core: &Arc<ProcessTransportCore>,
    result: PyResult<()>,
    message: &str,
) {
    if let Err(err) = result {
        core.report_error(err, message);
    }
}

#[inline]
pub(super) fn report_process_io_error(
    core: &Arc<ProcessTransportCore>,
    err: std::io::Error,
    message: &str,
) {
    core.report_error(PyRuntimeError::new_err(err.to_string()), message);
}

#[cfg(unix)]
pub(super) fn send_process_signal(child: &Child, signal: i32) -> std::io::Result<()> {
    let pid = i32::try_from(child.id()).map_err(|_| {
        std::io::Error::new(std::io::ErrorKind::InvalidInput, "child PID out of range")
    })?;
    // SAFETY: `libc::kill` is called with the child PID returned by `std::process::Child`
    // and a signal value supplied by the caller/Python API. It does not retain pointers.
    let result = unsafe { libc::kill(pid, signal) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(unix)]
fn process_exit_code_parts(code: Option<i32>, signal: Option<i32>) -> i32 {
    code.or_else(|| signal.and_then(i32::checked_neg))
        .unwrap_or(-1)
}

pub(super) fn process_exit_code(status: std::process::ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        process_exit_code_parts(status.code(), status.signal())
    }
    #[cfg(windows)]
    {
        status.code().unwrap_or(-1)
    }
}

pub(super) fn handle_process_exit(core: &Arc<ProcessTransportCore>, code: i32) {
    if core
        .state
        .lock()
        .expect("poisoned process state")
        .open_pipes
        .contains(&0)
    {
        report_process_result(
            core,
            core.pipe_connection_lost(0, None),
            "subprocess pipe_connection_lost failed",
        );
    }
    report_process_result(
        core,
        core.process_exited(code),
        "subprocess process_exited failed",
    );
}

pub(super) fn kill_process_child(
    core: &Arc<ProcessTransportCore>,
    child: &mut Child,
    message: &str,
) {
    if let Err(err) = child.kill() {
        report_process_io_error(core, err, message);
    }
}

pub(super) fn handle_process_command(
    core: &Arc<ProcessTransportCore>,
    child: &mut Child,
    command: ProcessCommand,
) {
    match command {
        ProcessCommand::Close | ProcessCommand::Kill => {
            kill_process_child(core, child, "subprocess kill failed");
        }
        #[cfg(unix)]
        ProcessCommand::Terminate => {
            if let Err(err) = send_process_signal(child, libc::SIGTERM) {
                report_process_io_error(core, err, "subprocess terminate failed");
            }
        }
        #[cfg(unix)]
        ProcessCommand::SendSignal(sig) => {
            if let Err(err) = send_process_signal(child, sig) {
                report_process_io_error(core, err, "subprocess send_signal failed");
            }
        }
        #[cfg(windows)]
        ProcessCommand::Terminate => {
            kill_process_child(core, child, "subprocess kill failed");
        }
        #[cfg(windows)]
        ProcessCommand::SendSignal(sig) => {
            let _ = sig;
            kill_process_child(core, child, "subprocess kill failed");
        }
    }
}

pub(super) fn run_process_reader(
    core: Arc<ProcessTransportCore>,
    fd: i32,
    mut reader: BoxedProcessReader,
) {
    let mut buf = vec![0_u8; PROCESS_READER_BUFFER_SIZE].into_boxed_slice();
    loop {
        match reader.read(&mut buf) {
            Ok(0) => {
                report_process_result(
                    &core,
                    core.pipe_connection_lost(fd, None),
                    "subprocess pipe_connection_lost failed",
                );
                return;
            }
            Ok(n) => {
                if let Err(err) = core.pipe_data_received(fd, &buf[..n]) {
                    core.report_error(err, "subprocess pipe_data_received failed");
                    report_process_result(
                        &core,
                        core.pipe_connection_lost(fd, None),
                        "subprocess pipe_connection_lost failed",
                    );
                    return;
                }
            }
            Err(err) => {
                report_process_result(
                    &core,
                    core.pipe_connection_lost(fd, Some(PyRuntimeError::new_err(err.to_string()))),
                    "subprocess pipe_connection_lost failed",
                );
                return;
            }
        }
    }
}

pub(super) fn run_process_waiter(
    core: Arc<ProcessTransportCore>,
    mut child: Child,
    control_rx: Receiver<ProcessCommand>,
) {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                handle_process_exit(&core, process_exit_code(status));
                return;
            }
            Ok(None) => {}
            Err(err) => {
                report_process_io_error(&core, err, "subprocess wait failed");
                report_process_result(
                    &core,
                    core.connection_lost(None),
                    "subprocess connection_lost failed",
                );
                return;
            }
        }

        match control_rx.recv_timeout(PROCESS_WAIT_POLL_INTERVAL) {
            Ok(command) => handle_process_command(&core, &mut child, command),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}

#[cfg(all(kani, unix))]
mod verification {
    use super::process_exit_code_parts;

    #[kani::proof]
    fn merge_process_exit_code_prefers_status_then_negated_signal() {
        let code: Option<i32> = kani::any();
        let signal: Option<i32> = kani::any();
        let result = process_exit_code_parts(code, signal);

        if let Some(code) = code {
            assert_eq!(result, code);
        } else if let Some(signal) = signal {
            if let Some(negated) = signal.checked_neg() {
                assert_eq!(result, negated);
            } else {
                assert_eq!(result, -1);
            }
        } else {
            assert_eq!(result, -1);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::process_exit_code;

    #[cfg(unix)]
    #[test]
    fn process_exit_code_preserves_exit_status_and_negates_signals() {
        use std::os::unix::process::ExitStatusExt;

        assert_eq!(process_exit_code(ExitStatusExt::from_raw(23 << 8)), 23);
        assert_eq!(
            process_exit_code(ExitStatusExt::from_raw(libc::SIGTERM)),
            -libc::SIGTERM
        );
    }

    #[cfg(windows)]
    #[test]
    fn process_exit_code_preserves_exit_status() {
        use std::os::windows::process::ExitStatusExt;

        assert_eq!(process_exit_code(ExitStatusExt::from_raw(23)), 23);
    }
}
