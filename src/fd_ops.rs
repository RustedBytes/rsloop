use std::io;

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use self::windows::{
    duplicate_handle, duplicate_handle_from_fd, duplicate_tcp_stream, poll_fd,
};

pub type RawFd = i64;

#[cfg(windows)]
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
    // POLLNVAL means the descriptor is not open (e.g. the socket was closed
    // out from under the watcher). Reporting it as "ready" would make
    // watchers fire forever on a dead fd, so surface it as an error instead;
    // a closed fd produces no readiness events on asyncio either.
    if revents & (libc::POLLNVAL as i32) != 0 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "file descriptor is not open",
        ));
    }
    let error_bits = (libc::POLLERR | libc::POLLHUP) as i32;
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
        return crate::blocking::run(format!("rsloop-fd-wait-{fd}"), move || {
            loop {
                match poll_fd(fd, read, write, FD_POLL_INTERVAL_MS)? {
                    (read_ready, write_ready)
                        if (!read || read_ready) && (!write || write_ready) =>
                    {
                        return Ok::<(), io::Error>(());
                    }
                    _ => {}
                }
            }
        })
        .await
        .map_err(PyRuntimeError::new_err)?
        .map_err(|err| PyRuntimeError::new_err(err.to_string()));
    }

    #[cfg(not(windows))]
    {
        use std::os::fd::BorrowedFd;

        // Register the descriptor with async-io's shared, process-wide reactor
        // thread instead of spawning a fresh OS thread per wait. The previous
        // thread-per-wait approach dominated connection-setup latency: a burst
        // of N concurrent connects spawned N `poll()` threads. async-io drives
        // the same epoll/kqueue reactor smol/async-std already run, so this adds
        // no extra threads and deregisters as soon as the wait resolves.
        let raw = raw_fd_to_c_int(fd).map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
        // SAFETY: the caller keeps `fd` open for the duration of this await
        // (the owning Python socket outlives the connect/recv/send operation).
        let borrowed = unsafe { BorrowedFd::borrow_raw(raw) };
        let async_fd = async_io::Async::new_nonblocking(borrowed)
            .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;

        let result = if write {
            futures::future::poll_fn(|cx| async_fd.poll_writable(cx)).await
        } else if read {
            futures::future::poll_fn(|cx| async_fd.poll_readable(cx)).await
        } else {
            Ok(())
        };
        result.map_err(|err| PyRuntimeError::new_err(err.to_string()))
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

/// A connect() attempt that is still completing in the background.
#[cfg(unix)]
pub fn is_connect_in_progress_errno(errno: i32) -> bool {
    errno == libc::EINPROGRESS || errno == libc::EALREADY || errno == libc::EWOULDBLOCK
}

/// The socket is already connected (a benign outcome for connect()).
#[cfg(unix)]
pub fn is_already_connected_errno(errno: i32) -> bool {
    errno == libc::EISCONN
}

/// Reads the pending `SO_ERROR` for a socket via a direct `getsockopt`, so the
/// connect-completion path resolves without acquiring the GIL.
#[cfg(unix)]
pub fn socket_so_error(fd: RawFd) -> io::Result<i32> {
    let fd = raw_fd_to_c_int(fd)?;
    let mut value: libc::c_int = 0;
    let mut len: libc::socklen_t = std::mem::size_of::<libc::c_int>()
        .try_into()
        .expect("socklen_t can represent c_int size");
    // SAFETY: `fd` is a socket descriptor and `value`/`len` describe a correctly
    // sized `c_int`/`socklen_t` that `getsockopt` fills in.
    let result = unsafe {
        libc::getsockopt(
            fd,
            libc::SOL_SOCKET,
            libc::SO_ERROR,
            (&mut value as *mut libc::c_int).cast(),
            &mut len,
        )
    };
    if result == 0 {
        Ok(value)
    } else {
        Err(io::Error::last_os_error())
    }
}
