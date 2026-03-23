use std::io;
#[cfg(not(windows))]
use std::thread;
#[cfg(windows)]
use std::time::Duration;

#[cfg(windows)]
use compio::net::PollFd;
#[cfg(windows)]
use compio::runtime::Runtime as CompioRuntime;
#[cfg(windows)]
use compio::time::timeout as compio_timeout;
#[cfg(not(windows))]
use futures::channel::oneshot;
#[cfg(windows)]
use futures::future::{select, Either};
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
use windows_sys::Win32::Networking::WinSock::{
    getsockopt, SOCKET, SOCKET_ERROR, SOL_SOCKET, SO_ACCEPTCONN,
};
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
    let is_listener = socket_is_listener(socket)?;
    let timeout = timeout_duration(timeout_ms);

    CompioRuntime::new()?.block_on(async move {
        match (read, write) {
            (true, false) => await_read_interest_with_timeout(fd, is_listener, timeout).await,
            (false, true) => {
                let ready = wait_for_write_ready(fd);
                await_interest_with_timeout(timeout, ready, (false, true)).await
            }
            (true, true) => await_dual_interest_with_timeout(fd, is_listener, timeout).await,
            (false, false) => Ok((false, false)),
        }
    })
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
        return crate::blocking::run(format!("rsloop-fd-wait-{fd}"), move || loop {
            match poll_fd(fd, read, write, 50)? {
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
fn socket_is_listener(socket: SOCKET) -> io::Result<bool> {
    let mut accept_conn = 0i32;
    let mut len = std::mem::size_of::<i32>() as i32;
    let result = unsafe {
        getsockopt(
            socket,
            SOL_SOCKET as i32,
            SO_ACCEPTCONN as i32,
            (&mut accept_conn as *mut i32).cast(),
            &mut len,
        )
    };
    if result == SOCKET_ERROR {
        return Err(io::Error::last_os_error());
    }
    Ok(accept_conn != 0)
}

#[cfg(windows)]
fn timeout_duration(timeout_ms: i32) -> Option<Duration> {
    if timeout_ms < 0 {
        None
    } else {
        Some(Duration::from_millis(timeout_ms as u64))
    }
}

#[cfg(windows)]
async fn wait_for_read_ready(fd: RawFd, is_listener: bool) -> io::Result<()> {
    let poll_fd = poll_fd_from_raw(fd)?;
    if is_listener {
        poll_fd.accept_ready().await
    } else {
        poll_fd.read_ready().await
    }
}

#[cfg(windows)]
async fn wait_for_write_ready(fd: RawFd) -> io::Result<()> {
    let poll_fd = poll_fd_from_raw(fd)?;
    poll_fd.write_ready().await
}

#[cfg(windows)]
async fn await_read_interest_with_timeout(
    fd: RawFd,
    is_listener: bool,
    timeout: Option<Duration>,
) -> io::Result<(bool, bool)> {
    let future = wait_for_read_ready(fd, is_listener);
    match timeout {
        Some(timeout) => match compio_timeout(timeout, future).await {
            Ok(result) => result.map(|_| (true, false)),
            Err(_) => Ok((probe_read_ready(fd, is_listener)?, false)),
        },
        None => future.await.map(|_| (true, false)),
    }
}

#[cfg(windows)]
async fn await_interest_with_timeout<F>(
    timeout: Option<Duration>,
    future: F,
    ready: (bool, bool),
) -> io::Result<(bool, bool)>
where
    F: std::future::Future<Output = io::Result<()>>,
{
    match timeout {
        Some(timeout) => match compio_timeout(timeout, future).await {
            Ok(result) => result.map(|_| ready),
            Err(_) => Ok((false, false)),
        },
        None => future.await.map(|_| ready),
    }
}

#[cfg(windows)]
async fn await_dual_interest_with_timeout(
    fd: RawFd,
    is_listener: bool,
    timeout: Option<Duration>,
) -> io::Result<(bool, bool)> {
    let read_future = Box::pin(wait_for_read_ready(fd, is_listener));
    let write_future = Box::pin(wait_for_write_ready(fd));
    let future = async move {
        match select(read_future, write_future).await {
            Either::Left((result, _)) => result.map(|_| (true, false)),
            Either::Right((result, _)) => result.map(|_| (false, true)),
        }
    };

    match timeout {
        Some(timeout) => match compio_timeout(timeout, future).await {
            Ok(result) => result,
            Err(_) => Ok((probe_read_ready(fd, is_listener)?, false)),
        },
        None => future.await,
    }
}

#[cfg(windows)]
fn probe_read_ready(fd: RawFd, is_listener: bool) -> io::Result<bool> {
    if is_listener {
        return Ok(false);
    }

    let dup = duplicate_socket(fd)?;
    let dup: RawSocket = dup
        .try_into()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "socket handle out of range"))?;
    let socket = unsafe { Socket::from_raw_socket(dup) };
    let mut buf = [std::mem::MaybeUninit::<u8>::uninit(); 1];
    match socket.peek(&mut buf) {
        Ok(_) => Ok(true),
        Err(err)
            if err.kind() == io::ErrorKind::WouldBlock
                || err.kind() == io::ErrorKind::Interrupted =>
        {
            Ok(false)
        }
        Err(_) => Ok(true),
    }
}
