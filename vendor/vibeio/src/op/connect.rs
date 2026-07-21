#[cfg(windows)]
use std::ffi::c_void;
use std::io;
#[cfg(unix)]
use std::mem;
#[cfg(unix)]
use std::mem::MaybeUninit;
use std::task::{Context, Poll};

use mio::Interest;
#[cfg(windows)]
use windows_sys::Win32::Networking::WinSock::{
    self as WinSock, AF_INET, AF_INET6, SOCKADDR, SOCKADDR_IN, SOCKADDR_IN6, SOCKADDR_STORAGE,
    SOCKET, SOCKET_ERROR, SOL_SOCKET, SO_UPDATE_CONNECT_CONTEXT, WSAEADDRINUSE, WSAEALREADY,
    WSAEINPROGRESS, WSAEINVAL, WSAENOTCONN, WSAEWOULDBLOCK, WSAID_CONNECTEX, WSA_IO_PENDING,
};
#[cfg(windows)]
use windows_sys::Win32::System::IO::OVERLAPPED;

use crate::driver::AnyDriver;
use crate::driver::CompletionIoResult;
use crate::fd_inner::InnerRawHandle;
#[cfg(windows)]
use crate::fd_inner::RawOsHandle;
use crate::op::Op;

#[cfg(unix)]
fn start_nonblocking_connect(
    fd: std::os::fd::RawFd,
    raw_addr: *const libc::sockaddr,
    raw_addr_len: libc::socklen_t,
) -> Result<(), io::Error> {
    let connect_result = unsafe { libc::connect(fd, raw_addr, raw_addr_len) };

    if connect_result == -1 {
        let err = io::Error::last_os_error();
        if !matches!(
            err.raw_os_error(),
            Some(libc::EINPROGRESS) | Some(libc::EWOULDBLOCK) | Some(libc::EALREADY)
        ) {
            return Err(err);
        }
    }

    Ok(())
}

#[cfg(windows)]
fn start_nonblocking_connect(
    socket: std::os::windows::io::RawSocket,
    raw_addr: *const SOCKADDR,
    raw_addr_len: i32,
) -> Result<(), io::Error> {
    let connect_result = unsafe { WinSock::connect(socket as SOCKET, raw_addr, raw_addr_len) };

    if connect_result == WinSock::SOCKET_ERROR {
        let err_code = unsafe { WinSock::WSAGetLastError() };
        if !matches!(err_code, WSAEINPROGRESS | WSAEWOULDBLOCK | WSAEALREADY) {
            return Err(io::Error::from_raw_os_error(err_code));
        }
    }

    Ok(())
}

#[cfg(windows)]
fn ensure_connectex_bound(socket: SOCKET, addr: *const SOCKADDR) -> Result<(), io::Error> {
    let family = unsafe { (*addr).sa_family as i32 };
    let bind_result = match family {
        x if x == AF_INET as i32 => {
            let local = SOCKADDR_IN {
                sin_family: AF_INET as u16,
                ..Default::default()
            };
            unsafe {
                WinSock::bind(
                    socket,
                    (&local as *const SOCKADDR_IN).cast::<SOCKADDR>(),
                    std::mem::size_of::<SOCKADDR_IN>() as i32,
                )
            }
        }
        x if x == AF_INET6 as i32 => {
            let local = SOCKADDR_IN6 {
                sin6_family: AF_INET6 as u16,
                ..Default::default()
            };
            unsafe {
                WinSock::bind(
                    socket,
                    (&local as *const SOCKADDR_IN6).cast::<SOCKADDR>(),
                    std::mem::size_of::<SOCKADDR_IN6>() as i32,
                )
            }
        }
        _ => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "unsupported socket family for ConnectEx",
            ))
        }
    };

    if bind_result == WinSock::SOCKET_ERROR {
        let err_code = unsafe { WinSock::WSAGetLastError() };
        if !matches!(err_code, WSAEINVAL | WSAEADDRINUSE) {
            return Err(io::Error::from_raw_os_error(err_code));
        }
    }

    Ok(())
}

#[cfg(windows)]
fn load_connect_ex(socket: SOCKET) -> Result<WinSock::LPFN_CONNECTEX, io::Error> {
    let mut bytes_returned: u32 = 0;
    let mut connect_ex: WinSock::LPFN_CONNECTEX = None;
    let mut guid = WSAID_CONNECTEX;

    let ioctl_result = unsafe {
        WinSock::WSAIoctl(
            socket,
            WinSock::SIO_GET_EXTENSION_FUNCTION_POINTER,
            (&mut guid as *mut _) as *mut c_void,
            std::mem::size_of_val(&guid) as u32,
            (&mut connect_ex as *mut _) as *mut c_void,
            std::mem::size_of_val(&connect_ex) as u32,
            &mut bytes_returned,
            std::ptr::null_mut(),
            None,
        )
    };

    if ioctl_result == WinSock::SOCKET_ERROR {
        let err_code = unsafe { WinSock::WSAGetLastError() };
        return Err(io::Error::from_raw_os_error(err_code));
    }

    if connect_ex.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "ConnectEx extension function is unavailable",
        ));
    }

    Ok(connect_ex)
}

#[cfg(windows)]
fn set_connect_context(socket: SOCKET) -> Result<(), io::Error> {
    let result = unsafe {
        WinSock::setsockopt(
            socket,
            SOL_SOCKET as i32,
            SO_UPDATE_CONNECT_CONTEXT as i32,
            std::ptr::null(),
            0,
        )
    };
    if result == WinSock::SOCKET_ERROR {
        let err_code = unsafe { WinSock::WSAGetLastError() };
        return Err(io::Error::from_raw_os_error(err_code));
    }
    Ok(())
}

pub struct ConnectOp<'a> {
    handle: &'a InnerRawHandle,
    #[cfg(unix)]
    addr: (*const libc::sockaddr, libc::socklen_t),
    #[cfg(windows)]
    addr: (*const SOCKADDR, i32),
    #[cfg(windows)]
    connect_ex: Option<WinSock::LPFN_CONNECTEX>,
    #[cfg(windows)]
    completion_bound: bool,
    completion_token: Option<usize>,
    #[cfg(any(unix, windows))]
    poll_connect_started: bool,
}

impl<'a> ConnectOp<'a> {
    #[cfg(unix)]
    #[inline]
    pub fn new(
        handle: &'a InnerRawHandle,
        addr: *const libc::sockaddr,
        addrlen: libc::socklen_t,
    ) -> Self {
        Self {
            handle,
            addr: (addr, addrlen),
            completion_token: None,
            #[cfg(windows)]
            connect_ex: None,
            #[cfg(windows)]
            completion_bound: false,
            poll_connect_started: false,
        }
    }

    #[cfg(windows)]
    #[inline]
    pub fn new(handle: &'a InnerRawHandle, addr: *const SOCKADDR, addrlen: i32) -> Self {
        Self {
            handle,
            addr: (addr, addrlen),
            completion_token: None,
            connect_ex: None,
            completion_bound: false,
            poll_connect_started: false,
        }
    }
}

impl Op for ConnectOp<'_> {
    type Output = ();

    #[cfg(any(unix, windows))]
    #[inline]
    fn poll_poll(
        &mut self,
        cx: &mut Context<'_>,
        driver: &AnyDriver,
    ) -> Poll<io::Result<Self::Output>> {
        if !self.poll_connect_started {
            #[cfg(unix)]
            let handle = self.handle.handle;
            #[cfg(windows)]
            let crate::fd_inner::RawOsHandle::Socket(handle) = self.handle.handle
            else {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid raw handle",
                )));
            };

            if let Err(err) = start_nonblocking_connect(handle, self.addr.0, self.addr.1) {
                return Poll::Ready(Err(err));
            };

            self.poll_connect_started = true;
        }

        #[cfg(unix)]
        {
            let mut socket_error: libc::c_int = 0;
            let mut socket_error_len = mem::size_of::<libc::c_int>() as libc::socklen_t;
            let getsockopt_result = unsafe {
                libc::getsockopt(
                    self.handle.handle,
                    libc::SOL_SOCKET,
                    libc::SO_ERROR,
                    (&mut socket_error as *mut libc::c_int).cast(),
                    &mut socket_error_len,
                )
            };
            if getsockopt_result == -1 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::WouldBlock {
                    if let Err(err) =
                        driver.submit_poll(self.handle, cx.waker().clone(), Interest::WRITABLE)
                    {
                        return Poll::Ready(Err(err));
                    }
                    return Poll::Pending;
                }
                return Poll::Ready(Err(error));
            }

            if socket_error != 0 {
                if matches!(
                    socket_error,
                    libc::EINPROGRESS | libc::EALREADY | libc::EWOULDBLOCK
                ) {
                    if let Err(err) =
                        driver.submit_poll(self.handle, cx.waker().clone(), Interest::WRITABLE)
                    {
                        return Poll::Ready(Err(err));
                    }
                    return Poll::Pending;
                }
                return Poll::Ready(Err(io::Error::from_raw_os_error(socket_error)));
            }

            let mut peer = MaybeUninit::<libc::sockaddr_storage>::zeroed();
            let mut peer_len = mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
            let getpeername_result = unsafe {
                libc::getpeername(
                    self.handle.handle,
                    peer.as_mut_ptr().cast::<libc::sockaddr>(),
                    &mut peer_len,
                )
            };

            if getpeername_result == -1 {
                let err = io::Error::last_os_error();
                if matches!(
                    err.raw_os_error(),
                    Some(libc::EINPROGRESS)
                        | Some(libc::EALREADY)
                        | Some(libc::EWOULDBLOCK)
                        | Some(libc::ENOTCONN)
                ) {
                    if let Err(err) =
                        driver.submit_poll(self.handle, cx.waker().clone(), Interest::WRITABLE)
                    {
                        return Poll::Ready(Err(err));
                    }
                    return Poll::Pending;
                }

                return Poll::Ready(Err(err));
            }

            Poll::Ready(Ok(()))
        }

        #[cfg(windows)]
        {
            let RawOsHandle::Socket(socket) = self.handle.handle else {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid raw handle",
                )));
            };
            let socket = socket as SOCKET;

            let mut socket_error: i32 = 0;
            let mut socket_error_len = std::mem::size_of::<i32>() as i32;
            let getsockopt_result = unsafe {
                WinSock::getsockopt(
                    socket,
                    SOL_SOCKET as i32,
                    WinSock::SO_ERROR as i32,
                    (&mut socket_error as *mut i32).cast(),
                    &mut socket_error_len,
                )
            };
            if getsockopt_result == SOCKET_ERROR {
                let error = io::Error::from_raw_os_error(unsafe { WinSock::WSAGetLastError() });
                if error.kind() == io::ErrorKind::WouldBlock {
                    if let Err(err) =
                        driver.submit_poll(self.handle, cx.waker().clone(), Interest::WRITABLE)
                    {
                        return Poll::Ready(Err(err));
                    }
                    return Poll::Pending;
                }
                return Poll::Ready(Err(error));
            }

            if socket_error != 0 {
                if matches!(socket_error, WSAEINPROGRESS | WSAEALREADY | WSAEWOULDBLOCK) {
                    if let Err(err) =
                        driver.submit_poll(self.handle, cx.waker().clone(), Interest::WRITABLE)
                    {
                        return Poll::Ready(Err(err));
                    }
                    return Poll::Pending;
                }
                return Poll::Ready(Err(io::Error::from_raw_os_error(socket_error)));
            }

            let mut peer = SOCKADDR_STORAGE::default();
            let mut peer_len = std::mem::size_of::<SOCKADDR_STORAGE>() as i32;
            let getpeername_result = unsafe {
                WinSock::getpeername(
                    socket,
                    (&mut peer as *mut SOCKADDR_STORAGE).cast::<SOCKADDR>(),
                    &mut peer_len,
                )
            };

            if getpeername_result == SOCKET_ERROR {
                let err_code = unsafe { WinSock::WSAGetLastError() };
                if matches!(
                    err_code,
                    WSAEINPROGRESS | WSAEALREADY | WSAEWOULDBLOCK | WSAENOTCONN
                ) {
                    if let Err(err) =
                        driver.submit_poll(self.handle, cx.waker().clone(), Interest::WRITABLE)
                    {
                        return Poll::Ready(Err(err));
                    }
                    return Poll::Pending;
                }

                return Poll::Ready(Err(io::Error::from_raw_os_error(err_code)));
            }

            Poll::Ready(Ok(()))
        }
    }

    #[cfg(any(unix, windows))]
    #[inline]
    fn poll_completion(
        &mut self,
        cx: &mut Context<'_>,
        driver: &AnyDriver,
    ) -> Poll<io::Result<Self::Output>> {
        let result = if let Some(completion_token) = self.completion_token {
            // Get the completion result
            match driver.get_completion_result(completion_token) {
                Some(result) => {
                    self.completion_token = None;
                    result
                }
                None => {
                    // The completion is not ready yet
                    driver.set_completion_waker(completion_token, cx.waker().clone());
                    return Poll::Pending;
                }
            }
        } else {
            // Submit the op
            match driver.submit_completion(self, cx.waker().clone()) {
                CompletionIoResult::Ok(result) => result,
                CompletionIoResult::Retry(token) => {
                    self.completion_token = Some(token);
                    return Poll::Pending;
                }
                CompletionIoResult::SubmitErr(err) => return Poll::Ready(Err(err)),
            }
        };
        if result < 0 {
            return Poll::Ready(Err(io::Error::from_raw_os_error(-result)));
        }

        #[cfg(windows)]
        {
            let RawOsHandle::Socket(socket) = self.handle.handle else {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "ConnectEx can be used only with socket handles",
                )));
            };

            if let Err(err) = set_connect_context(socket as SOCKET) {
                return Poll::Ready(Err(err));
            }
        }

        Poll::Ready(Ok(()))
    }

    #[cfg(windows)]
    #[inline]
    fn submit_windows(&mut self, overlapped: *mut OVERLAPPED) -> Result<(), io::Error> {
        let RawOsHandle::Socket(socket) = self.handle.handle else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "ConnectEx can be used only with socket handles",
            ));
        };

        let socket = socket as SOCKET;

        if !self.completion_bound {
            ensure_connectex_bound(socket, self.addr.0)?;
            self.completion_bound = true;
        }

        let connect_ex = if let Some(connect_ex) = self.connect_ex {
            connect_ex
        } else {
            let connect_ex = load_connect_ex(socket)?;
            self.connect_ex = Some(connect_ex);
            connect_ex
        };

        let Some(connect_ex_fn) = connect_ex else {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "ConnectEx extension function is unavailable",
            ));
        };

        let connect_result = unsafe {
            connect_ex_fn(
                socket,
                self.addr.0,
                self.addr.1,
                std::ptr::null(),
                0,
                std::ptr::null_mut(),
                overlapped,
            )
        };

        if connect_result != 0 {
            return Ok(());
        }

        let err = unsafe { WinSock::WSAGetLastError() };
        if err == WSA_IO_PENDING {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(err))
        }
    }

    #[cfg(target_os = "linux")]
    #[inline]
    fn build_completion_entry(
        &mut self,
        user_data: u64,
    ) -> Result<io_uring::squeue::Entry, io::Error> {
        use io_uring::{opcode, types};

        let (addr, addrlen) = self.addr;

        let entry = opcode::Connect::new(types::Fd(self.handle.handle), addr, addrlen)
            .build()
            .user_data(user_data);

        Ok(entry)
    }
}

impl Drop for ConnectOp<'_> {
    #[inline]
    fn drop(&mut self) {
        if let Some(completion_token) = self.completion_token {
            if let Some(driver) = crate::current_driver() {
                driver.ignore_completion(completion_token, Box::new(()));
            }
        }
    }
}
