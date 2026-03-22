use std::io;
use std::thread;

use futures::channel::oneshot;
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
#[cfg(windows)]
use windows_sys::Win32::Networking::WinSock::{select, FD_SET, SOCKET, SOCKET_ERROR, TIMEVAL};

pub type RawFd = i64;

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
    let fd = raw_fd_to_c_int(fd)?;
    let duped = unsafe { libc::dup(fd) };
    if duped < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(duped as RawFd)
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

#[cfg(windows)]
pub fn poll_fd(fd: RawFd, read: bool, write: bool, timeout_ms: i32) -> io::Result<(bool, bool)> {
    if !read && !write {
        return Ok((false, false));
    }

    let socket = raw_fd_to_socket(fd)?;
    let mut timeout = TIMEVAL {
        tv_sec: (timeout_ms / 1000),
        tv_usec: ((timeout_ms % 1000) * 1000),
    };
    let mut readfds = new_fd_set(socket, read);
    let mut writefds = new_fd_set(socket, write);

    let ready = unsafe {
        select(
            0,
            if read {
                &mut readfds
            } else {
                std::ptr::null_mut()
            },
            if write {
                &mut writefds
            } else {
                std::ptr::null_mut()
            },
            std::ptr::null_mut(),
            &mut timeout,
        )
    };
    if ready == SOCKET_ERROR {
        return Err(io::Error::last_os_error());
    }

    Ok((read && readfds.fd_count > 0, write && writefds.fd_count > 0))
}

pub async fn wait_readable(fd: RawFd) -> PyResult<()> {
    wait_for_interest(fd, true, false).await
}

pub async fn wait_writable(fd: RawFd) -> PyResult<()> {
    wait_for_interest(fd, false, true).await
}

async fn wait_for_interest(fd: RawFd, read: bool, write: bool) -> PyResult<()> {
    let (tx, rx) = oneshot::channel();
    thread::Builder::new()
        .name(format!("rsloop-fd-wait-{fd}"))
        .spawn(move || {
            let result = loop {
                match poll_fd(fd, read, write, 50) {
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

#[cfg(windows)]
fn raw_fd_to_socket(fd: RawFd) -> io::Result<SOCKET> {
    fd.try_into()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "socket handle out of range"))
}

#[cfg(windows)]
fn new_fd_set(socket: SOCKET, enabled: bool) -> FD_SET {
    let mut set = unsafe { std::mem::zeroed::<FD_SET>() };
    if enabled {
        set.fd_count = 1;
        set.fd_array[0] = socket;
    }
    set
}
