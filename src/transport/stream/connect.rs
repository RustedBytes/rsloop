//! Completing a non-blocking `connect()` on the loop runtime.

use std::{
    io,
    net::TcpStream as StdTcpStream,
    os::{fd::FromRawFd, raw::c_int},
    sync::Arc,
};

use pyo3::prelude::*;

use crate::{
    engine::{LoopCommand, LoopCore},
    fd_ops,
    vibeio::net::PollTcpStream as VibePollTcpStream,
};

/// Waits for a connecting TCP socket to become writable on the vibeio reactor,
/// then hands the outcome back to the loop thread via `ConnectCompleted`. The
/// descriptor is duplicated so the vibeio stream owns (and closes) only the
/// dup; the connecting socket stays owned by its Python object. The dup shares
/// the same underlying socket, so its POLLOUT readiness reflects the connect
/// completing.
#[cfg_attr(feature = "hotpath-profile", hotpath::measure(future = true))]
pub(crate) async fn run_connect_watch_task(
    core: Arc<LoopCore>,
    fd: fd_ops::RawFd,
    future: Py<PyAny>,
) {
    use crate::vibeio::io::AsyncWritePoll;

    let wait_errno = 'wait: {
        let Ok(raw) = c_int::try_from(fd) else {
            break 'wait libc::EBADF;
        };
        // SAFETY: `raw` is a live socket descriptor owned by the Python socket.
        let dup = unsafe { libc::dup(raw) };
        if dup < 0 {
            break 'wait io::Error::last_os_error()
                .raw_os_error()
                .unwrap_or(libc::EBADF);
        }
        // SAFETY: `dup` is a freshly duplicated, owned socket descriptor.
        let std_stream = unsafe { StdTcpStream::from_raw_fd(dup) };
        match VibePollTcpStream::from_std(std_stream) {
            Ok(stream) => match stream.writable().await {
                Ok(()) => 0,
                Err(err) => err.raw_os_error().unwrap_or(0),
            },
            Err(err) => err.raw_os_error().unwrap_or(libc::EBADF),
        }
    };

    let _ = core.send_command(LoopCommand::ConnectCompleted {
        future,
        fd,
        wait_errno,
    });
}
