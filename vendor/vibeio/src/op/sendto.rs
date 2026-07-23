use std::io;
#[cfg(unix)]
use std::mem::MaybeUninit;
use std::net::SocketAddr;
use std::task::{Context, Poll};

use mio::Interest;
#[cfg(windows)]
use windows_sys::Win32::{
    Networking::WinSock::{
        self as WinSock, AF_INET, AF_INET6, SOCKADDR, SOCKADDR_IN, SOCKADDR_IN6, SOCKADDR_STORAGE,
        SOCKET, WSABUF, WSA_IO_PENDING,
    },
    System::IO::OVERLAPPED,
};

use crate::driver::AnyDriver;
use crate::driver::CompletionIoResult;
use crate::fd_inner::InnerRawHandle;
#[cfg(windows)]
use crate::fd_inner::RawOsHandle;
use crate::io::IoBuf;
use crate::op::io_util::{poll_result_or_wait, CompletionBuffer};
use crate::op::Op;

#[cfg(unix)]
#[inline]
fn socket_addr_to_raw(address: SocketAddr) -> (libc::sockaddr_storage, libc::socklen_t) {
    match address {
        SocketAddr::V4(address) => {
            let sockaddr = libc::sockaddr_in {
                sin_family: libc::AF_INET as libc::sa_family_t,
                sin_port: address.port().to_be(),
                sin_addr: libc::in_addr {
                    s_addr: u32::from_ne_bytes(address.ip().octets()),
                },
                sin_zero: [0; 8],
                #[cfg(any(
                    target_os = "macos",
                    target_os = "ios",
                    target_os = "freebsd",
                    target_os = "openbsd",
                    target_os = "dragonfly",
                    target_os = "netbsd",
                    target_os = "haiku",
                    target_os = "aix",
                ))]
                sin_len: 0,
            };

            let mut storage = MaybeUninit::<libc::sockaddr_storage>::zeroed();
            unsafe {
                storage
                    .as_mut_ptr()
                    .cast::<libc::sockaddr_in>()
                    .write(sockaddr);
                (
                    storage.assume_init(),
                    std::mem::size_of::<libc::sockaddr_in>() as libc::socklen_t,
                )
            }
        }
        SocketAddr::V6(address) => {
            let sockaddr = libc::sockaddr_in6 {
                sin6_family: libc::AF_INET6 as libc::sa_family_t,
                sin6_port: address.port().to_be(),
                sin6_flowinfo: address.flowinfo(),
                sin6_addr: libc::in6_addr {
                    s6_addr: address.ip().octets(),
                },
                sin6_scope_id: address.scope_id(),
                #[cfg(any(
                    target_os = "macos",
                    target_os = "ios",
                    target_os = "freebsd",
                    target_os = "openbsd",
                    target_os = "dragonfly",
                    target_os = "netbsd",
                    target_os = "haiku",
                    target_os = "aix",
                ))]
                sin6_len: 0,
            };

            let mut storage = MaybeUninit::<libc::sockaddr_storage>::zeroed();
            unsafe {
                storage
                    .as_mut_ptr()
                    .cast::<libc::sockaddr_in6>()
                    .write(sockaddr);
                (
                    storage.assume_init(),
                    std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
                )
            }
        }
    }
}

#[cfg(windows)]
#[inline]
fn socket_addr_to_raw(address: SocketAddr) -> (SOCKADDR_STORAGE, i32) {
    match address {
        SocketAddr::V4(address) => {
            let mut sockaddr = SOCKADDR_IN::default();
            sockaddr.sin_family = AF_INET;
            sockaddr.sin_port = address.port().to_be();
            sockaddr.sin_addr.S_un.S_addr = u32::from_ne_bytes(address.ip().octets());

            let mut storage = SOCKADDR_STORAGE::default();
            unsafe {
                std::ptr::copy_nonoverlapping(
                    &sockaddr as *const SOCKADDR_IN as *const u8,
                    &mut storage as *mut SOCKADDR_STORAGE as *mut u8,
                    std::mem::size_of::<SOCKADDR_IN>(),
                );
            }
            (storage, std::mem::size_of::<SOCKADDR_IN>() as i32)
        }
        SocketAddr::V6(address) => {
            let mut sockaddr = SOCKADDR_IN6::default();
            sockaddr.sin6_family = AF_INET6;
            sockaddr.sin6_port = address.port().to_be();
            sockaddr.sin6_flowinfo = address.flowinfo();
            sockaddr.sin6_addr.u.Byte = address.ip().octets();
            sockaddr.Anonymous.sin6_scope_id = address.scope_id() as u32;

            let mut storage = SOCKADDR_STORAGE::default();
            unsafe {
                std::ptr::copy_nonoverlapping(
                    &sockaddr as *const SOCKADDR_IN6 as *const u8,
                    &mut storage as *mut SOCKADDR_STORAGE as *mut u8,
                    std::mem::size_of::<SOCKADDR_IN6>(),
                );
            }
            (storage, std::mem::size_of::<SOCKADDR_IN6>() as i32)
        }
    }
}

#[cfg(windows)]
#[inline]
fn socket_sendto<B: IoBuf>(socket: SOCKET, buf: &B, addr: SocketAddr) -> io::Result<usize> {
    use windows_sys::Win32::Networking::WinSock::{self as WinSock, SOCKET_ERROR, WSABUF};

    let len = u32::try_from(buf.buf_len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "write buffer is too large for Windows socket I/O",
        )
    })?;

    let mut wsabuf = WSABUF {
        len,
        buf: buf.as_buf_ptr().cast_mut().cast(),
    };
    let (raw_addr, raw_addr_len) = socket_addr_to_raw(addr);
    let mut bytes: u32 = 0;

    let send_result = unsafe {
        WinSock::WSASendTo(
            socket,
            &mut wsabuf,
            1,
            &mut bytes,
            0,
            (&raw_addr as *const SOCKADDR_STORAGE).cast::<SOCKADDR>(),
            raw_addr_len,
            std::ptr::null_mut(),
            None,
        )
    };
    if send_result == SOCKET_ERROR {
        return Err(io::Error::from_raw_os_error(unsafe {
            WinSock::WSAGetLastError()
        }));
    }

    Ok(bytes as usize)
}

#[cfg(windows)]
struct SendtoWindowsCompletion {
    socket_buf: WSABUF,
    addr: SOCKADDR_STORAGE,
    addr_len: i32,
}

#[cfg(target_os = "linux")]
struct SendtoLinuxCompletion {
    addr: libc::sockaddr_storage,
    addr_len: libc::socklen_t,
    iovec: libc::iovec,
    msghdr: libc::msghdr,
}

pub struct SendtoOp<'a, B: IoBuf> {
    handle: &'a InnerRawHandle,
    buf: Option<CompletionBuffer<B>>,
    addr: SocketAddr,
    completion_token: Option<usize>,
    #[cfg(windows)]
    completion_state: Option<Box<SendtoWindowsCompletion>>,
    #[cfg(target_os = "linux")]
    completion_state: Option<Box<SendtoLinuxCompletion>>,
}

impl<'a, B: IoBuf> SendtoOp<'a, B> {
    #[inline]
    pub fn new(handle: &'a InnerRawHandle, buf: B, addr: SocketAddr) -> Self {
        Self {
            handle,
            buf: Some(CompletionBuffer::new(buf, handle.uses_completion())),
            addr,
            completion_token: None,
            #[cfg(windows)]
            completion_state: None,
            #[cfg(target_os = "linux")]
            completion_state: None,
        }
    }

    #[inline]
    pub fn take_bufs(mut self) -> B {
        self.buf.take().unwrap().into_inner()
    }
}

impl<B: IoBuf> Op for SendtoOp<'_, B> {
    type Output = usize;

    #[cfg(any(unix, windows))]
    #[inline]
    fn poll_poll(
        &mut self,
        cx: &mut Context<'_>,
        driver: &AnyDriver,
    ) -> Poll<io::Result<Self::Output>> {
        let buf = self.buf.as_ref().unwrap().as_ref();

        #[cfg(unix)]
        let result = {
            let (raw_addr, raw_addr_len) = socket_addr_to_raw(self.addr);
            let written = unsafe {
                libc::sendto(
                    self.handle.handle,
                    buf.as_buf_ptr().cast::<libc::c_void>(),
                    buf.buf_len(),
                    0,
                    (&raw_addr as *const libc::sockaddr_storage).cast::<libc::sockaddr>(),
                    raw_addr_len,
                )
            };
            if written == -1 {
                Err(io::Error::last_os_error())
            } else {
                Ok(written as usize)
            }
        };

        #[cfg(windows)]
        let result = match self.handle.handle {
            RawOsHandle::Socket(socket) => socket_sendto(socket as SOCKET, buf, self.addr),
            RawOsHandle::Handle(_) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "poll-based sendto currently supports sockets only on Windows",
            )),
        };

        match poll_result_or_wait(result, self.handle, cx, driver, Interest::WRITABLE) {
            Poll::Ready(Ok(written)) => Poll::Ready(Ok(written)),
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
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
            match driver.get_completion_result(completion_token) {
                Some(result) => {
                    self.completion_token = None;
                    result
                }
                None => {
                    driver.set_completion_waker(completion_token, cx.waker().clone());
                    return Poll::Pending;
                }
            }
        } else {
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
        let written = result as usize;
        Poll::Ready(Ok(written))
    }

    #[cfg(windows)]
    #[inline]
    fn submit_windows(&mut self, overlapped: *mut OVERLAPPED) -> Result<(), io::Error> {
        let buf = self.buf.as_ref().unwrap().as_ref();
        let RawOsHandle::Socket(socket) = self.handle.handle else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "WSASendTo can be used only with sockets",
            ));
        };

        let write_len = u32::try_from(buf.buf_len()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "write buffer is too large for Windows socket I/O",
            )
        })?;

        let (raw_addr, raw_addr_len) = socket_addr_to_raw(self.addr);
        let completion = self.completion_state.get_or_insert_with(|| {
            Box::new(SendtoWindowsCompletion {
                socket_buf: WSABUF {
                    len: 0,
                    buf: std::ptr::null_mut(),
                },
                addr: SOCKADDR_STORAGE::default(),
                addr_len: 0,
            })
        });
        completion.socket_buf.len = write_len;
        completion.socket_buf.buf = buf.as_buf_ptr().cast_mut().cast();
        completion.addr = raw_addr;
        completion.addr_len = raw_addr_len;

        let send_result = unsafe {
            WinSock::WSASendTo(
                socket as SOCKET,
                &mut completion.socket_buf as *mut WSABUF,
                1,
                std::ptr::null_mut(),
                0,
                (&completion.addr as *const SOCKADDR_STORAGE).cast::<SOCKADDR>(),
                completion.addr_len,
                overlapped,
                None,
            )
        };

        if send_result == 0 {
            return Ok(());
        }

        let err = unsafe { WinSock::WSAGetLastError() };
        if err == WSA_IO_PENDING {
            Ok(())
        } else {
            self.completion_state = None;
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

        let (raw_addr, raw_addr_len) = socket_addr_to_raw(self.addr);
        let buf = self.buf.as_ref().unwrap().as_ref();
        let completion = self.completion_state.get_or_insert_with(|| {
            Box::new(SendtoLinuxCompletion {
                addr: unsafe { std::mem::zeroed() },
                addr_len: 0,
                iovec: libc::iovec {
                    iov_base: std::ptr::null_mut(),
                    iov_len: 0,
                },
                msghdr: unsafe { std::mem::zeroed() },
            })
        });
        completion.addr = raw_addr;
        completion.addr_len = raw_addr_len;
        completion.iovec = libc::iovec {
            iov_base: buf.as_buf_ptr().cast_mut().cast::<libc::c_void>(),
            iov_len: buf.buf_len(),
        };

        completion.msghdr = unsafe { std::mem::zeroed::<libc::msghdr>() };
        completion.msghdr.msg_name =
            &mut completion.addr as *mut libc::sockaddr_storage as *mut libc::c_void;
        completion.msghdr.msg_namelen = completion.addr_len;
        completion.msghdr.msg_iov = &mut completion.iovec as *mut libc::iovec;
        completion.msghdr.msg_iovlen = 1;
        completion.msghdr.msg_control = std::ptr::null_mut();
        completion.msghdr.msg_controllen = 1;
        completion.msghdr.msg_flags = 1;

        let entry = opcode::SendMsg::new(
            types::Fd(self.handle.handle),
            &completion.msghdr as *const libc::msghdr,
        )
        .build()
        .user_data(user_data);

        Ok(entry)
    }
}

impl<B: IoBuf> Drop for SendtoOp<'_, B> {
    #[inline]
    fn drop(&mut self) {
        if let Some(completion_token) = self.completion_token {
            if let Some(driver) = crate::current_driver() {
                #[cfg(any(windows, target_os = "linux"))]
                let completion_state = self.completion_state.take();
                #[cfg(not(any(windows, target_os = "linux")))]
                let completion_state = ();

                driver.ignore_completion(
                    completion_token,
                    Box::new((
                        completion_state,
                        self.buf.take().map(CompletionBuffer::into_stable_box),
                    )),
                );
            }
        }
    }
}
