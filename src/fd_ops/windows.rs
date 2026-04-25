use std::io;
use std::mem;
use std::net::TcpStream as StdTcpStream;
use std::os::windows::io::{FromRawSocket, IntoRawSocket};

use socket2::Socket;
use windows_sys::Win32::Foundation::{
    DuplicateHandle, DUPLICATE_SAME_ACCESS, HANDLE, INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Networking::WinSock::{
    select as winsock_select, FD_SET, FD_SETSIZE, SOCKET, SOCKET_ERROR, TIMEVAL,
};
use windows_sys::Win32::System::Threading::GetCurrentProcess;

use super::{raw_fd_to_c_int, RawFd};

pub(super) fn dup_raw_fd(fd: RawFd) -> io::Result<RawFd> {
    match duplicate_socket(fd) {
        Ok(duped) => Ok(duped),
        Err(socket_err) => {
            if let Ok(fd) = raw_fd_to_c_int(fd) {
                // SAFETY: `fd` was range-checked as a C runtime descriptor. Errors are
                // reported via a negative return and errno.
                let duped = unsafe { libc::dup(fd) };
                if duped >= 0 {
                    return Ok(duped as RawFd);
                }
            }
            Err(socket_err)
        }
    }
}

pub(super) fn duplicate_socket(fd: RawFd) -> io::Result<RawFd> {
    let socket = raw_fd_to_socket(fd)?;
    let socket = socket_from_raw(socket);
    let duplicate = socket.try_clone()?;
    let raw = duplicate.into_raw_socket();
    mem::forget(socket);
    Ok(raw as RawFd)
}

fn socket_from_raw(socket: SOCKET) -> Socket {
    // SAFETY: The caller provides a raw socket handle that should be temporarily owned by
    // `Socket`; callers must prevent unintended closure when they only borrow the source handle.
    unsafe { Socket::from_raw_socket(socket as _) }
}

fn duplicate_handle_raw(
    process: HANDLE,
    handle: HANDLE,
    duplicated: &mut HANDLE,
    options: u32,
) -> i32 {
    let target = process;
    let access = 0;
    let inherit = 0;
    let call = DuplicateHandle;
    // SAFETY: `handle` must be valid for `process`, and `duplicated` must be a valid
    // out-parameter. The wrapper forwards directly to Windows `DuplicateHandle`.
    unsafe {
        call(
            process, handle, target, duplicated, access, inherit, options,
        )
    }
}

pub fn raw_fd_to_handle(fd: RawFd) -> io::Result<HANDLE> {
    let fd = raw_fd_to_c_int(fd)?;
    // SAFETY: `_get_osfhandle` only reads the C runtime fd table for this validated fd and returns
    // `-1` on failure.
    let handle = unsafe { libc::get_osfhandle(fd) };
    if handle == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(handle as HANDLE)
}

pub fn duplicate_handle(handle: HANDLE) -> io::Result<HANDLE> {
    if handle.is_null() || handle == INVALID_HANDLE_VALUE {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid Windows handle",
        ));
    }

    let mut duplicated = 0 as HANDLE;
    // SAFETY: `GetCurrentProcess` returns the always-valid pseudo-handle for the current
    // process and does not require any cleanup by the caller.
    let process = unsafe { GetCurrentProcess() };
    let ok = duplicate_handle_raw(process, handle, &mut duplicated, DUPLICATE_SAME_ACCESS);
    if ok == 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(duplicated)
}

pub fn duplicate_handle_from_fd(fd: RawFd) -> io::Result<HANDLE> {
    duplicate_handle(raw_fd_to_handle(fd)?)
}

pub fn poll_fd(fd: RawFd, read: bool, write: bool, timeout_ms: i32) -> io::Result<(bool, bool)> {
    if !read && !write {
        return Ok((false, false));
    }

    let socket = raw_fd_to_socket(fd)?;
    let mut timeout = TIMEVAL {
        tv_sec: timeout_ms / 1000,
        tv_usec: (timeout_ms % 1000) * 1000,
    };
    let mut readfds = new_fd_set(socket, read);
    let mut writefds = new_fd_set(socket, write);

    let readfds_ptr = if read {
        &mut readfds
    } else {
        std::ptr::null_mut()
    };
    let writefds_ptr = if write {
        &mut writefds
    } else {
        std::ptr::null_mut()
    };
    let exceptfds = std::ptr::null_mut();
    // SAFETY: The fd sets and timeout live for the duration of the call. Null pointers are passed
    // for disabled interests as required by winsock `select`.
    let ready = unsafe { winsock_select(0, readfds_ptr, writefds_ptr, exceptfds, &mut timeout) };
    if ready == SOCKET_ERROR {
        return Err(io::Error::last_os_error());
    }

    Ok((read && readfds.fd_count > 0, write && writefds.fd_count > 0))
}

fn raw_fd_to_socket(fd: RawFd) -> io::Result<SOCKET> {
    fd.try_into()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "socket handle out of range"))
}

fn new_fd_set(socket: SOCKET, enabled: bool) -> FD_SET {
    let mut set = FD_SET {
        fd_count: 0,
        fd_array: [0; FD_SETSIZE as usize],
    };
    if enabled {
        set.fd_count = 1;
        set.fd_array[0] = socket;
    }
    set
}

pub fn duplicate_tcp_stream(fd: RawFd) -> io::Result<StdTcpStream> {
    let dup = duplicate_socket(fd)?;
    let socket = socket_from_raw(raw_fd_to_socket(dup)?);
    socket.set_nonblocking(true)?;
    Ok(socket.into())
}
