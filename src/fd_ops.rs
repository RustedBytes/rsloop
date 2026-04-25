use std::io;
#[cfg(not(windows))]
use std::thread;

#[cfg(not(windows))]
use futures::channel::oneshot;
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use self::windows::{
    duplicate_handle, duplicate_handle_from_fd, duplicate_tcp_stream, poll_fd, raw_fd_to_handle,
};

pub type RawFd = i64;

const FD_POLL_INTERVAL_MS: i32 = 50;

pub fn fileobj_to_fd(_py: Python<'_>, fileobj: &Bound<'_, PyAny>) -> PyResult<RawFd> {
    if let Ok(fd) = fileobj.extract::<RawFd>() {
        return Ok(fd);
    }

    fileobj.call_method0("fileno")?.extract::<RawFd>()
}

pub fn fileobj_keepalive(fileobj: &Bound<'_, PyAny>) -> Py<PyAny> {
    fileobj.clone().unbind().into_any()
}

pub fn dup_raw_fd(fd: RawFd) -> io::Result<RawFd> {
    #[cfg(unix)]
    {
        let fd = raw_fd_to_c_int(fd)?;
        // SAFETY: `fd` was range-checked as a C file descriptor. `dup` returns a new descriptor
        // or `-1` with errno set and does not retain Rust references.
        let duped = unsafe { libc::dup(fd) };
        if duped < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(duped as RawFd)
    }

    #[cfg(windows)]
    {
        windows::dup_raw_fd(fd)
    }
}

#[cfg(unix)]
pub fn poll_fd(fd: RawFd, read: bool, write: bool, timeout_ms: i32) -> io::Result<(bool, bool)> {
    if !read && !write {
        return Ok((false, false));
    }

    let fd = raw_fd_to_c_int(fd)?;
    let mut events = 0;
    if read {
        events |= libc::POLLIN;
    }
    if write {
        events |= libc::POLLOUT;
    }

    let mut pollfd = libc::pollfd {
        fd,
        events,
        revents: 0,
    };

    loop {
        // SAFETY: `pollfd` points to one initialized `libc::pollfd` and the count is `1`; `poll`
        // only mutates the `revents` field and reports errors through errno.
        let ready = unsafe { libc::poll(&mut pollfd, 1, timeout_ms) };
        if ready >= 0 {
            break;
        }

        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return Err(err);
    }

    let revents = pollfd.revents as i32;
    let error_bits = (libc::POLLERR | libc::POLLHUP | libc::POLLNVAL) as i32;
    Ok((
        read && (revents & ((libc::POLLIN as i32) | error_bits)) != 0,
        write && (revents & ((libc::POLLOUT as i32) | error_bits)) != 0,
    ))
}

pub async fn wait_readable(fd: RawFd) -> PyResult<()> {
    #[cfg(windows)]
    {
        if let Ok(stream) = duplicate_tcp_stream(fd) {
            if stream.peer_addr().is_ok() {
                let (tx, rx) = futures::channel::oneshot::channel();
                let task = crate::windows_vibeio::spawn(move || async move {
                    let result = async {
                        let stream = vibeio::net::PollTcpStream::from_std(stream)?;
                        let mut buf = [0_u8; 1];
                        stream.peek(&mut buf).await.map(|_| ())
                    }
                    .await;
                    let _ = tx.send(result);
                });

                if let Ok(task) = task {
                    let result = rx
                        .await
                        .map_err(|_| PyRuntimeError::new_err("vibeio wait dropped"))?
                        .map_err(|err| PyRuntimeError::new_err(err.to_string()));
                    crate::windows_vibeio::cancel(task);
                    return result;
                }
            }
        }
    }

    wait_for_interest(fd, true, false).await
}

pub async fn wait_writable(fd: RawFd) -> PyResult<()> {
    wait_for_interest(fd, false, true).await
}

async fn wait_for_interest(fd: RawFd, read: bool, write: bool) -> PyResult<()> {
    #[cfg(windows)]
    {
        return crate::blocking::run(format!("rsloop-fd-wait-{fd}"), move || loop {
            match poll_fd(fd, read, write, FD_POLL_INTERVAL_MS)? {
                (read_ready, write_ready) if (!read || read_ready) && (!write || write_ready) => {
                    return Ok::<(), io::Error>(());
                }
                _ => {}
            }
        })
        .await
        .map_err(PyRuntimeError::new_err)?
        .map_err(|err| PyRuntimeError::new_err(err.to_string()));
    }

    #[cfg(not(windows))]
    {
        let (tx, rx) = oneshot::channel();
        thread::Builder::new()
            .name(format!("rsloop-fd-wait-{fd}"))
            .spawn(move || {
                let result = loop {
                    match poll_fd(fd, read, write, FD_POLL_INTERVAL_MS) {
                        Ok((true, _)) if read => break Ok(()),
                        Ok((_, true)) if write => break Ok(()),
                        Ok(_) => continue,
                        Err(err) => break Err(err),
                    }
                };
                let _ = tx.send(result);
            })
            .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;

        rx.await
            .map_err(|_| PyRuntimeError::new_err("fd wait worker dropped"))?
            .map_err(|err| PyRuntimeError::new_err(err.to_string()))
    }
}

pub fn is_retryable_socket_error(py: Python<'_>, err: &PyErr) -> PyResult<bool> {
    let builtins = py.import("builtins")?;
    let blocking = builtins.getattr("BlockingIOError")?;
    let interrupted = builtins.getattr("InterruptedError")?;
    Ok(err.is_instance(py, &blocking) || err.is_instance(py, &interrupted))
}

fn raw_fd_to_c_int(fd: RawFd) -> io::Result<libc::c_int> {
    fd.try_into()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "descriptor out of range"))
}
