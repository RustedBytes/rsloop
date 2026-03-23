use std::io;
#[cfg(not(windows))]
use std::thread;

#[cfg(windows)]
use compio::net::PollFd;
#[cfg(windows)]
use compio::runtime::Runtime as CompioRuntime;
#[cfg(not(windows))]
use futures::channel::oneshot;
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
#[cfg(windows)]
use socket2::Socket;
#[cfg(windows)]
use std::mem;
#[cfg(windows)]
use std::os::windows::io::{FromRawSocket, IntoRawSocket, RawSocket};
#[cfg(windows)]
use windows_sys::Win32::Foundation::{
    DuplicateHandle, DUPLICATE_SAME_ACCESS, HANDLE, INVALID_HANDLE_VALUE,
};
#[cfg(windows)]
use windows_sys::Win32::Networking::WinSock::{select, FD_SET, SOCKET, SOCKET_ERROR, TIMEVAL};
#[cfg(windows)]
use windows_sys::Win32::System::Threading::GetCurrentProcess;

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
    #[cfg(unix)]
    {
        let fd = raw_fd_to_c_int(fd)?;
        let duped = unsafe { libc::dup(fd) };
        if duped < 0 {
            return Err(io::Error::last_os_error());
        }
        return Ok(duped as RawFd);
    }

    #[cfg(windows)]
    {
        match duplicate_socket(fd) {
            Ok(duped) => return Ok(duped),
            Err(socket_err) => {
                if let Ok(fd) = raw_fd_to_c_int(fd) {
                    let duped = unsafe { libc::dup(fd) };
                    if duped >= 0 {
                        return Ok(duped as RawFd);
                    }
                }
                return Err(socket_err);
            }
        }
    }
}

#[cfg(windows)]
fn duplicate_socket(fd: RawFd) -> io::Result<RawFd> {
    let socket = raw_fd_to_socket(fd)?;
    let socket = unsafe { Socket::from_raw_socket(socket as _) };
    let duplicate = socket.try_clone()?;
    let raw = duplicate.into_raw_socket();
    mem::forget(socket);
    Ok(raw as RawFd)
}

#[cfg(windows)]
pub fn raw_fd_to_handle(fd: RawFd) -> io::Result<HANDLE> {
    let fd = raw_fd_to_c_int(fd)?;
    let handle = unsafe { libc::get_osfhandle(fd) };
    if handle == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(handle as HANDLE)
}

#[cfg(windows)]
pub fn duplicate_handle(handle: HANDLE) -> io::Result<HANDLE> {
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid Windows handle",
        ));
    }

    let current_process = unsafe { GetCurrentProcess() };
    let mut duplicated = 0 as HANDLE;
    let ok = unsafe {
        DuplicateHandle(
            current_process,
            handle,
            current_process,
            &mut duplicated,
            0,
            0,
            DUPLICATE_SAME_ACCESS,
        )
    };
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(duplicated)
}

#[cfg(windows)]
pub fn duplicate_handle_from_fd(fd: RawFd) -> io::Result<HANDLE> {
    duplicate_handle(raw_fd_to_handle(fd)?)
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
    #[cfg(windows)]
    {
        return crate::blocking::run(format!("rsloop-fd-wait-{fd}"), move || {
            let runtime = CompioRuntime::new()?;
            runtime.block_on(async move {
                let poll_fd = poll_fd_from_raw(fd)?;
                match (read, write) {
                    (true, false) => poll_fd.read_ready().await,
                    (false, true) => poll_fd.write_ready().await,
                    (true, true) => Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "combined socket interest waits are not supported",
                    )),
                    (false, false) => Ok(()),
                }
            })
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
fn poll_fd_from_raw(fd: RawFd) -> io::Result<PollFd<Socket>> {
    let dup = duplicate_socket(fd)?;
    let dup: RawSocket = dup
        .try_into()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "socket handle out of range"))?;
    PollFd::new(unsafe { Socket::from_raw_socket(dup) })
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
