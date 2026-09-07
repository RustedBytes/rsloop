#![deny(unsafe_op_in_unsafe_fn)]
#![warn(clippy::undocumented_unsafe_blocks)]

#[cfg(windows)]
use std::ffi::c_void;
use std::io;
#[cfg(unix)]
use std::mem::{self, MaybeUninit};
use std::net::SocketAddr;
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
#[cfg(windows)]
use std::os::windows::io::{AsRawSocket, FromRawSocket, OwnedSocket};
#[cfg(windows)]
use std::ptr;
use std::task::{Context, Poll};

use mio::Interest;
#[cfg(windows)]
use windows_sys::Win32::{
    Networking::WinSock::{
        self as WinSock, AF_INET, AF_INET6, INVALID_SOCKET, SO_UPDATE_ACCEPT_CONTEXT, SOCKADDR,
        SOCKADDR_STORAGE, SOCKET, SOL_SOCKET, WSA_IO_PENDING, WSAID_ACCEPTEX,
        WSAID_GETACCEPTEXSOCKADDRS,
    },
    System::IO::OVERLAPPED,
};

use crate::vibeio::driver::AnyDriver;
#[cfg(not(target_os = "linux"))]
use crate::vibeio::driver::CompletionIoResult;
use crate::vibeio::fd_inner::InnerRawHandle;
#[cfg(windows)]
use crate::vibeio::fd_inner::RawOsHandle;
use crate::vibeio::op::Op;
use crate::vibeio::op::socket_addr::sockaddr_storage_to_socketaddr;

#[cfg(unix)]
use crate::vibeio::op::io_util::set_cloexec;

#[cfg(unix)]
type OwnedAcceptSocket = OwnedFd;
#[cfg(windows)]
type OwnedAcceptSocket = OwnedSocket;

#[cfg(windows)]
fn load_accept_ex(socket: SOCKET) -> Result<WinSock::LPFN_ACCEPTEX, io::Error> {
    let mut bytes_returned: u32 = 0;
    let mut accept_ex: WinSock::LPFN_ACCEPTEX = None;
    let mut guid = WSAID_ACCEPTEX;

    // SAFETY: the GUID and function-pointer output have their exact supplied
    // sizes; all outputs remain live for this synchronous (null OVERLAPPED) call.
    let ioctl_result = unsafe {
        WinSock::WSAIoctl(
            socket,
            WinSock::SIO_GET_EXTENSION_FUNCTION_POINTER,
            (&mut guid as *mut _) as *mut c_void,
            std::mem::size_of_val(&guid) as u32,
            (&mut accept_ex as *mut _) as *mut c_void,
            std::mem::size_of_val(&accept_ex) as u32,
            &mut bytes_returned,
            ptr::null_mut(),
            None,
        )
    };

    if ioctl_result == WinSock::SOCKET_ERROR {
        return Err(last_socket_error());
    }

    if accept_ex.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "AcceptEx extension function is unavailable",
        ));
    }

    Ok(accept_ex)
}

#[cfg(windows)]
fn load_get_accept_ex_sockaddrs(
    socket: SOCKET,
) -> Result<WinSock::LPFN_GETACCEPTEXSOCKADDRS, io::Error> {
    let mut bytes_returned: u32 = 0;
    let mut get_accept_ex_sockaddrs: WinSock::LPFN_GETACCEPTEXSOCKADDRS = None;
    let mut guid = WSAID_GETACCEPTEXSOCKADDRS;

    // SAFETY: the GUID and function-pointer output have their exact supplied
    // sizes; null OVERLAPPED means none of these stack pointers are retained.
    let ioctl_result = unsafe {
        WinSock::WSAIoctl(
            socket,
            WinSock::SIO_GET_EXTENSION_FUNCTION_POINTER,
            (&mut guid as *mut _) as *mut c_void,
            std::mem::size_of_val(&guid) as u32,
            (&mut get_accept_ex_sockaddrs as *mut _) as *mut c_void,
            std::mem::size_of_val(&get_accept_ex_sockaddrs) as u32,
            &mut bytes_returned,
            ptr::null_mut(),
            None,
        )
    };

    if ioctl_result == WinSock::SOCKET_ERROR {
        return Err(last_socket_error());
    }

    if get_accept_ex_sockaddrs.is_none() {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "GetAcceptExSockaddrs extension function is unavailable",
        ));
    }

    Ok(get_accept_ex_sockaddrs)
}

#[cfg(windows)]
fn listener_socket_family(listener_socket: SOCKET) -> Result<i32, io::Error> {
    let mut addr = SOCKADDR_STORAGE::default();
    let mut addr_len = std::mem::size_of::<SOCKADDR_STORAGE>() as i32;
    // SAFETY: addr and addr_len are live writable output storage of the supplied
    // size. getsockname does not retain their pointers.
    let result = unsafe {
        WinSock::getsockname(
            listener_socket,
            (&mut addr as *mut SOCKADDR_STORAGE).cast::<SOCKADDR>(),
            &mut addr_len,
        )
    };

    if result == WinSock::SOCKET_ERROR {
        return Err(last_socket_error());
    }

    let address = sockaddr_storage_to_socketaddr(&addr, addr_len as usize)?;
    Ok(if address.is_ipv4() { AF_INET } else { AF_INET6 } as i32)
}

#[cfg(windows)]
fn create_accept_socket(listener_socket: SOCKET) -> Result<OwnedSocket, io::Error> {
    let family = listener_socket_family(listener_socket)?;
    // Socket::new requests both overlapped I/O and non-inheritance on Windows.
    // Return an owner immediately so all later failure paths close the socket.
    let socket = socket2::Socket::new(
        socket2::Domain::from(family),
        socket2::Type::STREAM,
        Some(socket2::Protocol::TCP),
    )?;
    Ok(socket.into())
}

#[cfg(windows)]
fn set_accept_context(listener_socket: SOCKET, accepted_socket: SOCKET) -> Result<(), io::Error> {
    // SAFETY: the option value points to a live SOCKET of the exact supplied
    // size; setsockopt reads it synchronously and retains no pointer.
    let result = unsafe {
        WinSock::setsockopt(
            accepted_socket,
            SOL_SOCKET,
            SO_UPDATE_ACCEPT_CONTEXT,
            (&listener_socket as *const SOCKET).cast(),
            std::mem::size_of::<SOCKET>() as i32,
        )
    };
    if result == WinSock::SOCKET_ERROR {
        return Err(last_socket_error());
    }
    Ok(())
}

#[cfg(windows)]
const ACCEPTEX_ADDR_LEN: usize = std::mem::size_of::<SOCKADDR_STORAGE>() + 16;
#[cfg(windows)]
const ACCEPTEX_OUTPUT_BUFFER_LEN: usize = ACCEPTEX_ADDR_LEN * 2;

#[cfg(unix)]
fn finish_unix_accept(
    owned: OwnedFd,
    set_flags: bool,
) -> io::Result<(OwnedAcceptSocket, SocketAddr)> {
    let fd = owned.as_raw_fd();
    if set_flags {
        set_cloexec(fd)?;
    }
    let mut peer = MaybeUninit::<libc::sockaddr_storage>::zeroed();
    let mut peer_len = mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    // SAFETY: peer and peer_len are valid writable storage; owned keeps fd open.
    let result = unsafe {
        libc::getpeername(
            fd,
            peer.as_mut_ptr().cast::<libc::sockaddr>(),
            &mut peer_len,
        )
    };
    if result == -1 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: the storage was zero-initialized before the kernel filled it.
    let peer = unsafe { peer.assume_init() };
    let address = sockaddr_storage_to_socketaddr(&peer, peer_len as usize)?;
    Ok((owned, address))
}

#[cfg(windows)]
fn finish_windows_accept(owned: OwnedSocket) -> io::Result<(OwnedAcceptSocket, SocketAddr)> {
    let mut peer = SOCKADDR_STORAGE::default();
    let mut peer_len = std::mem::size_of::<SOCKADDR_STORAGE>() as i32;
    // SAFETY: owned keeps the socket open; peer and peer_len are live writable
    // output storage with the supplied capacity, not retained after the call.
    let result = unsafe {
        WinSock::getpeername(
            owned.as_raw_socket() as SOCKET,
            (&mut peer as *mut SOCKADDR_STORAGE).cast::<SOCKADDR>(),
            &mut peer_len,
        )
    };
    if result == WinSock::SOCKET_ERROR {
        return Err(last_socket_error());
    }
    let address = sockaddr_storage_to_socketaddr(&peer, peer_len as usize)?;
    Ok((owned, address))
}

#[cfg(windows)]
fn last_socket_error() -> io::Error {
    // SAFETY: reads this thread's Winsock error state without pointer arguments.
    io::Error::from_raw_os_error(unsafe { WinSock::WSAGetLastError() })
}

pub struct AcceptOp<'a> {
    handle: &'a InnerRawHandle,
    #[cfg(windows)]
    accept_ex: WinSock::LPFN_ACCEPTEX,
    #[cfg(windows)]
    get_accept_ex_sockaddrs: WinSock::LPFN_GETACCEPTEXSOCKADDRS,
    #[cfg(windows)]
    accept_socket: Option<OwnedSocket>,
    #[cfg(windows)]
    bytes_received: Option<Box<u32>>,
    #[cfg(windows)]
    accept_output_buffer: Option<Box<[u8]>>,
    completion_token: Option<usize>,
}

impl<'a> AcceptOp<'a> {
    #[inline]
    pub fn new(handle: &'a InnerRawHandle) -> Self {
        Self {
            handle,
            #[cfg(windows)]
            accept_ex: None,
            #[cfg(windows)]
            get_accept_ex_sockaddrs: None,
            #[cfg(windows)]
            accept_socket: None,
            #[cfg(windows)]
            bytes_received: None,
            #[cfg(windows)]
            accept_output_buffer: None,
            completion_token: None,
        }
    }
}

impl Op for AcceptOp<'_> {
    #[cfg(target_os = "linux")]
    fn completion_returns_fd(&self) -> bool {
        true
    }
    type Output = (OwnedAcceptSocket, SocketAddr);

    #[cfg(any(unix, windows))]
    #[inline]
    fn poll_poll(
        &mut self,
        cx: &mut Context<'_>,
        driver: &AnyDriver,
    ) -> Poll<io::Result<Self::Output>> {
        #[cfg(unix)]
        {
            #[cfg(syscall_accept4)]
            // SAFETY: null address outputs are permitted. The borrowed handle
            // keeps the listener open; a successful call returns a new owned fd.
            let accepted_fd = unsafe {
                libc::accept4(
                    self.handle.handle,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                    libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
                )
            };
            #[cfg(not(syscall_accept4))]
            // SAFETY: null address outputs are permitted; success transfers a
            // new fd, which is immediately wrapped in an ownership guard below.
            let accepted_fd = unsafe {
                libc::accept(
                    self.handle.handle,
                    std::ptr::null_mut(),
                    std::ptr::null_mut(),
                )
            };
            if accepted_fd == -1 {
                let error = io::Error::last_os_error();
                if error.kind() == io::ErrorKind::WouldBlock {
                    if let Err(err) =
                        driver.submit_poll(self.handle, cx.waker().clone(), Interest::READABLE)
                    {
                        return Poll::Ready(Err(err));
                    }
                    return Poll::Pending;
                }
                return Poll::Ready(Err(error));
            }

            // SAFETY: accept transferred a new descriptor to this operation.
            let owned = unsafe { OwnedFd::from_raw_fd(accepted_fd) };
            Poll::Ready(finish_unix_accept(owned, !cfg!(syscall_accept4)))
        }

        #[cfg(windows)]
        {
            let RawOsHandle::Socket(listener_socket) = self.handle.handle else {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid raw handle",
                )));
            };

            // SAFETY: the handle keeps the listener open and null address
            // outputs are permitted. Success transfers a new socket.
            let accepted_socket = unsafe {
                WinSock::accept(listener_socket as SOCKET, ptr::null_mut(), ptr::null_mut())
            };
            if accepted_socket == INVALID_SOCKET {
                let error = last_socket_error();
                if error.kind() == io::ErrorKind::WouldBlock {
                    if let Err(err) =
                        driver.submit_poll(self.handle, cx.waker().clone(), Interest::READABLE)
                    {
                        return Poll::Ready(Err(err));
                    }
                    return Poll::Pending;
                }
                return Poll::Ready(Err(error));
            }

            // SAFETY: accept returned a new socket, checked against INVALID_SOCKET.
            let owned = unsafe { OwnedSocket::from_raw_socket(accepted_socket as _) };
            Poll::Ready(finish_windows_accept(owned))
        }
    }

    #[cfg(any(unix, windows))]
    #[inline]
    fn poll_completion(
        &mut self,
        cx: &mut Context<'_>,
        driver: &AnyDriver,
    ) -> Poll<io::Result<Self::Output>> {
        #[cfg(target_os = "linux")]
        let result = match driver.poll_multishot_accept(self.handle, cx) {
            Poll::Ready(Ok(result)) => result,
            Poll::Ready(Err(err)) => return Poll::Ready(Err(err)),
            Poll::Pending => return Poll::Pending,
        };

        #[cfg(not(target_os = "linux"))]
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
            #[cfg(windows)]
            drop(self.accept_socket.take());
            return Poll::Ready(Err(crate::vibeio::op::io_util::completion_error(result)));
        }

        #[cfg(unix)]
        {
            // SAFETY: the driver transferred its successful accept result.
            let owned = unsafe { OwnedFd::from_raw_fd(result as RawFd) };
            Poll::Ready(finish_unix_accept(owned, true))
        }

        #[cfg(windows)]
        {
            let RawOsHandle::Socket(listener_socket) = self.handle.handle else {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "AcceptEx can be used only with listening sockets",
                )));
            };

            let Some(accept_socket) = self.accept_socket.take() else {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::Other,
                    "AcceptEx completion missing accepted socket",
                )));
            };

            if let Err(err) = set_accept_context(
                listener_socket as SOCKET,
                accept_socket.as_raw_socket() as SOCKET,
            ) {
                return Poll::Ready(Err(err));
            }

            let peer = match self.accept_output_buffer.take() {
                Some(buf) => buf,
                None => {
                    return Poll::Ready(Err(io::Error::new(
                        io::ErrorKind::Other,
                        "AcceptEx completion missing peer address",
                    )));
                }
            };

            let mut local_sockaddr: *mut SOCKADDR_STORAGE = std::ptr::null_mut();
            let mut local_sockaddr_len: i32 = 0;
            let mut remote_sockaddr: *mut SOCKADDR_STORAGE = std::ptr::null_mut();
            let mut remote_sockaddr_len: i32 = 0;

            let get_accept_ex_sockaddrs_fn = self.get_accept_ex_sockaddrs.ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::Unsupported,
                    "GetAcceptExSockaddrs extension function is unavailable",
                )
            })?;

            // SAFETY: the completed AcceptEx buffer remains live and has the
            // same address-region sizes used at submission. All four outputs
            // point to writable locals; returned addresses are bounds-checked
            // against this buffer before reading any bytes.
            unsafe {
                get_accept_ex_sockaddrs_fn(
                    peer.as_ptr() as *const c_void,
                    0,
                    ACCEPTEX_ADDR_LEN as _,
                    ACCEPTEX_ADDR_LEN as _,
                    &mut local_sockaddr as *mut *mut SOCKADDR_STORAGE as *mut *mut SOCKADDR,
                    &mut local_sockaddr_len as *mut i32,
                    &mut remote_sockaddr as *mut *mut SOCKADDR_STORAGE as *mut *mut SOCKADDR,
                    &mut remote_sockaddr_len as *mut i32,
                )
            };

            let address = crate::vibeio::op::socket_addr::socketaddr_from_buffer(
                &peer,
                remote_sockaddr as usize,
                remote_sockaddr_len,
            )?;

            return Poll::Ready(Ok((accept_socket, address)));
        }
    }

    #[cfg(windows)]
    #[inline]
    fn submit_windows(&mut self, overlapped: *mut OVERLAPPED) -> Result<(), io::Error> {
        let RawOsHandle::Socket(listener_socket) = self.handle.handle else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "AcceptEx can be used only with listening sockets",
            ));
        };
        let listener_socket = listener_socket as SOCKET;

        if self.accept_ex.is_none() {
            self.accept_ex = load_accept_ex(listener_socket)?;
        }
        if self.get_accept_ex_sockaddrs.is_none() {
            self.get_accept_ex_sockaddrs = load_get_accept_ex_sockaddrs(listener_socket)?;
        }
        let accept_ex_fn = self.accept_ex.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "AcceptEx extension function is unavailable",
            )
        })?;

        if self.accept_socket.is_none() {
            self.accept_socket = Some(create_accept_socket(listener_socket)?);
        }
        let accept_socket = self
            .accept_socket
            .as_ref()
            .expect("accept_socket must be initialized")
            .as_raw_socket() as SOCKET;
        if self.accept_output_buffer.is_none() {
            self.accept_output_buffer =
                Some(vec![0u8; ACCEPTEX_OUTPUT_BUFFER_LEN].into_boxed_slice());
        }
        let accept_output_buffer = self
            .accept_output_buffer
            .as_mut()
            .expect("accept_output_buffer must be initialized");

        let bytes_received = self.bytes_received.get_or_insert_with(|| Box::new(0));
        // SAFETY: both sockets stay owned through completion. Boxed output
        // storage covers the two address regions and has stable addresses; the
        // driver owns OVERLAPPED until acknowledgement. Drop transfers these
        // allocations and the accepted socket to cancellation retention.
        let accept_result = unsafe {
            accept_ex_fn(
                listener_socket,
                accept_socket,
                accept_output_buffer.as_mut_ptr().cast::<c_void>(),
                0,
                ACCEPTEX_ADDR_LEN as u32,
                ACCEPTEX_ADDR_LEN as u32,
                bytes_received.as_mut(),
                overlapped,
            )
        };
        if accept_result != 0 {
            return Ok(());
        }

        let error = last_socket_error();
        if error.raw_os_error() == Some(WSA_IO_PENDING) {
            Ok(())
        } else {
            drop(self.accept_socket.take());
            Err(error)
        }
    }

    #[cfg(target_os = "linux")]
    #[inline]
    fn build_completion_entry(
        &mut self,
        user_data: u64,
    ) -> Result<io_uring::squeue::Entry, io::Error> {
        use io_uring::{opcode, types};

        let entry = opcode::Accept::new(
            types::Fd(self.handle.handle),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
        .flags(libc::SOCK_NONBLOCK | libc::SOCK_CLOEXEC)
        .build()
        .user_data(user_data);

        Ok(entry)
    }
}

impl Drop for AcceptOp<'_> {
    #[inline]
    fn drop(&mut self) {
        if let Some(token) = self.completion_token.take() {
            #[cfg(windows)]
            let storage = (
                self.accept_socket.take(),
                self.accept_output_buffer.take(),
                self.bytes_received.take(),
            );
            #[cfg(not(windows))]
            let storage = ();
            self.handle.cancel_completion(token, Box::new(storage));
        }
    }
}

#[cfg(test)]
mod ownership_tests {
    use super::*;
    use std::io::Read;
    #[cfg(unix)]
    use std::os::unix::net::UnixStream;

    #[cfg(windows)]
    #[test]
    fn acceptex_socket_is_owned_and_non_inheritable() {
        use windows_sys::Win32::Foundation::{GetHandleInformation, HANDLE_FLAG_INHERIT};
        for (address, family) in [("127.0.0.1:0", AF_INET), ("[::1]:0", AF_INET6)] {
            let listener = std::net::TcpListener::bind(address).unwrap();
            let raw = listener.as_raw_socket() as SOCKET;
            assert_eq!(listener_socket_family(raw).unwrap(), family as i32);
            let accepted = create_accept_socket(raw).unwrap();
            let mut flags = 0;
            // SAFETY: accepted owns the live socket handle and flags is writable.
            let result =
                unsafe { GetHandleInformation(accepted.as_raw_socket() as *mut _, &mut flags) };
            assert_ne!(result, 0, "{}", io::Error::last_os_error());
            assert_eq!(flags & HANDLE_FLAG_INHERIT, 0);
            assert_eq!(
                socket2::SockRef::from(&accepted).r#type().unwrap(),
                socket2::Type::STREAM
            );
        }
        assert_eq!(
            create_accept_socket(INVALID_SOCKET)
                .unwrap_err()
                .raw_os_error(),
            Some(WinSock::WSAENOTSOCK)
        );
    }

    #[cfg(unix)]
    #[test]
    fn unsupported_peer_address_closes_accepted_socket() {
        let (socket, mut peer) = UnixStream::pair().unwrap();
        peer.set_nonblocking(true).unwrap();
        let owned: OwnedFd = socket.into();
        assert_eq!(
            finish_unix_accept(owned, true).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
        crate::vibeio::test_support::assert_eof(&mut peer);
    }

    #[test]
    fn discarding_poll_accept_result_closes_the_connection() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        listener.set_nonblocking(true).unwrap();
        let mut peer = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        peer.set_read_timeout(Some(crate::vibeio::test_support::WATCHDOG))
            .unwrap();
        let driver = std::rc::Rc::new(AnyDriver::new_mock());
        let mut handle = InnerRawHandle::for_mock_completion(driver.clone());
        #[cfg(unix)]
        {
            handle.handle = listener.as_raw_fd();
        }
        #[cfg(windows)]
        {
            handle.handle = RawOsHandle::Socket(listener.as_raw_socket());
        }
        let mut op = AcceptOp::new(&handle);
        let Poll::Ready(Ok(result)) =
            op.poll_poll(&mut Context::from_waker(std::task::Waker::noop()), &driver)
        else {
            panic!("queued connection must be accepted");
        };
        assert_eq!(result.1, peer.local_addr().unwrap());
        drop(result);
        crate::vibeio::test_support::assert_eof(&mut peer);
    }

    #[test]
    fn successful_accept_transfers_descriptor_and_peer_address() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mut peer = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        peer.set_nonblocking(true).unwrap();
        let (socket, _) = listener.accept().unwrap();
        #[cfg(unix)]
        let (owned, address) = finish_unix_accept(socket.into(), true).unwrap();
        #[cfg(windows)]
        let (owned, address) = finish_windows_accept(socket.into()).unwrap();
        assert_eq!(address, peer.local_addr().unwrap());
        #[cfg(unix)]
        {
            // SAFETY: owned keeps this descriptor valid during the query.
            let flags = unsafe { libc::fcntl(owned.as_raw_fd(), libc::F_GETFD) };
            assert_ne!(flags, -1);
            assert_ne!(flags & libc::FD_CLOEXEC, 0);
        }
        assert_eq!(
            peer.read(&mut [0; 1]).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        drop(owned);
        peer.set_nonblocking(false).unwrap();
        peer.set_read_timeout(Some(crate::vibeio::test_support::WATCHDOG))
            .unwrap();
        crate::vibeio::test_support::assert_eof(&mut peer);
    }

    #[cfg(windows)]
    #[test]
    fn unconnected_socket_cannot_be_returned_as_an_accepted_connection() {
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let error = finish_windows_accept(listener.into()).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(WinSock::WSAENOTCONN));
    }

    #[test]
    fn cancelled_accept_uses_the_owning_driver() {
        use std::rc::Rc;
        for entered in [false, true] {
            let owner = Rc::new(AnyDriver::new_mock());
            let handle = InnerRawHandle::for_mock_completion(owner.clone());
            let cancel = move || {
                let mut op = AcceptOp::new(&handle);
                op.completion_token = Some(41);
                drop(op);
            };
            if entered {
                let runtime = crate::vibeio::executor::Runtime::new(AnyDriver::new_mock());
                runtime.block_on(async move { cancel() });
            } else {
                cancel();
            }
            let AnyDriver::Mock(driver) = owner.as_ref() else {
                unreachable!()
            };
            let held = driver.ignored.take();
            assert_eq!(held.len(), 1);
            assert_eq!(held[0].0, 41);
        }
    }
}
