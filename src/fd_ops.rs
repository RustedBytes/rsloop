use std::io;
#[cfg(windows)]
use std::collections::HashMap;
#[cfg(not(windows))]
use std::thread;
#[cfg(windows)]
use std::sync::{Arc, Mutex, OnceLock};

#[cfg(not(windows))]
use futures::channel::oneshot;
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
#[cfg(windows)]
use socket2::Socket;
#[cfg(windows)]
use std::mem;
#[cfg(windows)]
use std::os::windows::io::{AsRawSocket, FromRawSocket, IntoRawSocket};
#[cfg(windows)]
use windows_sys::Win32::Foundation::{
    CloseHandle, DuplicateHandle, DUPLICATE_SAME_ACCESS, ERROR_IO_PENDING, ERROR_NOT_FOUND, HANDLE,
    INVALID_HANDLE_VALUE, WAIT_TIMEOUT,
};
#[cfg(windows)]
use windows_sys::Win32::Networking::WinSock::{
    getsockopt, WSACloseEvent, WSACreateEvent, WSAEnumNetworkEvents, WSAEventSelect,
    WSAGetLastError, WSARecv, WSASend, WSAWaitForMultipleEvents, FD_ACCEPT, FD_CLOSE, FD_CONNECT,
    FD_READ, FD_WRITE, SOCKET, SOCKET_ERROR, SOL_SOCKET, SO_ACCEPTCONN, WSABUF, WSAEVENT,
    WSANETWORKEVENTS, WSA_INVALID_EVENT, WSA_WAIT_EVENT_0, WSA_WAIT_FAILED, WSA_WAIT_TIMEOUT,
};
#[cfg(windows)]
use windows_sys::Win32::System::Threading::GetCurrentProcess;
#[cfg(windows)]
use windows_sys::Win32::System::IO::{
    CancelIoEx, CreateIoCompletionPort, GetQueuedCompletionStatus, OVERLAPPED,
};

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

    let socket_key = raw_fd_to_socket(fd)?;
    let duped = duplicate_socket(fd)?;
    let socket = unsafe { Socket::from_raw_socket(raw_fd_to_socket(duped)? as _) };
    let raw_socket = socket.as_raw_socket() as SOCKET;
    let is_listener = socket_is_listener(raw_socket)?;
    let is_connected = socket.peer_addr().is_ok();

    if read && !write && !is_listener && is_connected {
        return Ok((
            iocp_wait_socket(socket_key, raw_socket, IocpInterest::Read, timeout_ms)?,
            false,
        ));
    }

    if write && !read && is_connected {
        return Ok((
            false,
            iocp_wait_socket(socket_key, raw_socket, IocpInterest::Write, timeout_ms)?,
        ));
    }

    let event_mask = requested_network_events(read, write, is_listener);
    wait_socket_event(raw_socket, event_mask, read, write, is_listener, timeout_ms)
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
            let (read_ready, write_ready) = poll_fd(fd, read, write, -1)?;
            if (read && read_ready) || (write && write_ready) {
                return Ok(());
            }
            Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "socket interest wait completed without readiness",
            ))
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
        return Err(last_socket_error());
    }
    Ok(accept_conn != 0)
}

#[cfg(windows)]
fn requested_network_events(read: bool, write: bool, is_listener: bool) -> i32 {
    let mut events = 0;
    if read {
        events |= if is_listener {
            FD_ACCEPT as i32
        } else {
            (FD_READ | FD_CLOSE) as i32
        };
    }
    if write {
        events |= (FD_WRITE | FD_CONNECT | FD_CLOSE) as i32;
    }
    events
}

#[cfg(windows)]
fn wait_socket_event(
    socket: SOCKET,
    event_mask: i32,
    read: bool,
    write: bool,
    is_listener: bool,
    timeout_ms: i32,
) -> io::Result<(bool, bool)> {
    let event = OwnedWsaEvent::new()?;
    let selector = unsafe { WSAEventSelect(socket, event.raw(), event_mask) };
    if selector == SOCKET_ERROR {
        return Err(last_socket_error());
    }

    let handles = [event.raw() as HANDLE];
    let wait_result = unsafe {
        WSAWaitForMultipleEvents(
            handles.len() as u32,
            handles.as_ptr(),
            0,
            timeout_to_wait_ms(timeout_ms),
            0,
        )
    };
    if wait_result == WSA_WAIT_TIMEOUT {
        return Ok((false, false));
    }
    if wait_result == WSA_WAIT_FAILED {
        return Err(last_socket_error());
    }
    if wait_result != WSA_WAIT_EVENT_0 as u32 {
        return Err(io::Error::other(format!(
            "unexpected WSA wait result: {wait_result}"
        )));
    }

    let mut network_events = unsafe { std::mem::zeroed::<WSANETWORKEVENTS>() };
    let enum_result = unsafe { WSAEnumNetworkEvents(socket, event.raw(), &mut network_events) };
    if enum_result == SOCKET_ERROR {
        return Err(last_socket_error());
    }

    for event in [FD_ACCEPT, FD_CONNECT, FD_READ, FD_WRITE, FD_CLOSE] {
        let event_mask = event as i32;
        if (network_events.lNetworkEvents & event_mask) == 0 {
            continue;
        }

        let error = network_events.iErrorCode[event.ilog2() as usize];
        if error != 0 {
            return Err(io::Error::from_raw_os_error(error));
        }
    }

    let close_ready = (network_events.lNetworkEvents & FD_CLOSE as i32) != 0;
    let read_ready = read
        && ((network_events.lNetworkEvents
            & if is_listener {
                FD_ACCEPT as i32
            } else {
                FD_READ as i32
            })
            != 0
            || close_ready);
    let write_ready = write
        && ((network_events.lNetworkEvents & (FD_WRITE | FD_CONNECT) as i32) != 0 || close_ready);
    Ok((read_ready, write_ready))
}

#[cfg(windows)]
fn iocp_wait_socket(
    socket_key: SOCKET,
    socket: SOCKET,
    interest: IocpInterest,
    timeout_ms: i32,
) -> io::Result<bool> {
    let state = socket_wait_state(socket_key)?;
    let _gate = state.gate.lock().expect("poisoned socket wait gate");
    let associated = unsafe { CreateIoCompletionPort(socket as HANDLE, state.port.raw(), 1, 0) };
    if associated.is_null() {
        return Err(io::Error::last_os_error());
    }

    let mut overlapped = unsafe { std::mem::zeroed::<OVERLAPPED>() };
    let mut buffer = WSABUF {
        len: 0,
        buf: std::ptr::null_mut(),
    };
    let mut transferred = 0u32;
    let submitted = match interest {
        IocpInterest::Read => {
            let mut flags = 0u32;
            unsafe {
                WSARecv(
                    socket,
                    &mut buffer,
                    1,
                    &mut transferred,
                    &mut flags,
                    &mut overlapped,
                    None,
                )
            }
        }
        IocpInterest::Write => unsafe {
            WSASend(
                socket,
                &mut buffer,
                1,
                &mut transferred,
                0,
                &mut overlapped,
                None,
            )
        },
    };

    if submitted == 0 {
        return Ok(true);
    }

    let submit_error = unsafe { WSAGetLastError() };
    if submit_error != ERROR_IO_PENDING as i32 {
        return Err(io::Error::from_raw_os_error(submit_error));
    }

    let mut completion_key = 0usize;
    let mut completed = std::ptr::null_mut();
    let completed_ok = unsafe {
        GetQueuedCompletionStatus(
            state.port.raw(),
            &mut transferred,
            &mut completion_key,
            &mut completed,
            timeout_to_wait_ms(timeout_ms),
        )
    };
    if completed_ok != 0 {
        return Ok(true);
    }

    let wait_error = io::Error::last_os_error();
    if wait_error.raw_os_error() == Some(WAIT_TIMEOUT as i32) && completed.is_null() {
        cancel_pending_socket_io(socket, state.port.raw(), &mut overlapped)?;
        return Ok(false);
    }

    Err(wait_error)
}

#[cfg(windows)]
fn cancel_pending_socket_io(
    socket: SOCKET,
    port: HANDLE,
    overlapped: &mut OVERLAPPED,
) -> io::Result<()> {
    let cancelled = unsafe { CancelIoEx(socket as HANDLE, overlapped) };
    if cancelled == 0 {
        let err = io::Error::last_os_error();
        if err.raw_os_error() != Some(ERROR_NOT_FOUND as i32) {
            return Err(err);
        }
    }

    let mut transferred = 0u32;
    let mut completion_key = 0usize;
    let mut completed = std::ptr::null_mut();
    let _ = unsafe {
        GetQueuedCompletionStatus(
            port,
            &mut transferred,
            &mut completion_key,
            &mut completed,
            u32::MAX,
        )
    };
    Ok(())
}

#[cfg(windows)]
fn timeout_to_wait_ms(timeout_ms: i32) -> u32 {
    if timeout_ms < 0 {
        u32::MAX
    } else {
        timeout_ms as u32
    }
}

#[cfg(windows)]
fn last_socket_error() -> io::Error {
    io::Error::from_raw_os_error(unsafe { WSAGetLastError() })
}

#[cfg(windows)]
enum IocpInterest {
    Read,
    Write,
}

#[cfg(windows)]
struct SocketWaitState {
    port: OwnedHandle,
    gate: Mutex<()>,
}

#[cfg(windows)]
fn socket_wait_state(socket: SOCKET) -> io::Result<Arc<SocketWaitState>> {
    static STATES: OnceLock<Mutex<HashMap<SOCKET, Arc<SocketWaitState>>>> = OnceLock::new();

    let states = STATES.get_or_init(|| Mutex::new(HashMap::new()));
    let mut states = states.lock().expect("poisoned socket wait state map");
    if let Some(state) = states.get(&socket) {
        return Ok(Arc::clone(state));
    }

    let state = Arc::new(SocketWaitState {
        port: OwnedHandle::new_completion_port()?,
        gate: Mutex::new(()),
    });
    states.insert(socket, Arc::clone(&state));
    Ok(state)
}

#[cfg(windows)]
struct OwnedHandle(HANDLE);

#[cfg(windows)]
unsafe impl Send for OwnedHandle {}

#[cfg(windows)]
unsafe impl Sync for OwnedHandle {}

#[cfg(windows)]
impl OwnedHandle {
    fn new_completion_port() -> io::Result<Self> {
        let handle =
            unsafe { CreateIoCompletionPort(INVALID_HANDLE_VALUE, std::ptr::null_mut(), 0, 1) };
        if handle.is_null() {
            return Err(io::Error::last_os_error());
        }
        Ok(Self(handle))
    }

    fn raw(&self) -> HANDLE {
        self.0
    }
}

#[cfg(windows)]
impl Drop for OwnedHandle {
    fn drop(&mut self) {
        let _ = unsafe { CloseHandle(self.0) };
    }
}

#[cfg(windows)]
struct OwnedWsaEvent(WSAEVENT);

#[cfg(windows)]
impl OwnedWsaEvent {
    fn new() -> io::Result<Self> {
        let event = unsafe { WSACreateEvent() };
        if event == WSA_INVALID_EVENT {
            return Err(last_socket_error());
        }
        Ok(Self(event))
    }

    fn raw(&self) -> WSAEVENT {
        self.0
    }
}

#[cfg(windows)]
impl Drop for OwnedWsaEvent {
    fn drop(&mut self) {
        let _ = unsafe { WSACloseEvent(self.0) };
    }
}
