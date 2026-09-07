#![deny(unsafe_op_in_unsafe_fn)]
#![warn(clippy::undocumented_unsafe_blocks)]

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
    self as WinSock, AF_INET, AF_INET6, SO_UPDATE_CONNECT_CONTEXT, SOCKADDR, SOCKADDR_IN,
    SOCKADDR_IN6, SOCKADDR_STORAGE, SOCKET, SOCKET_ERROR, SOL_SOCKET, WSA_IO_PENDING, WSAEALREADY,
    WSAEINPROGRESS, WSAEINVAL, WSAENOTCONN, WSAEWOULDBLOCK, WSAID_CONNECTEX,
};
#[cfg(windows)]
use windows_sys::Win32::System::IO::OVERLAPPED;

use crate::vibeio::driver::AnyDriver;
use crate::vibeio::driver::CompletionIoResult;
use crate::vibeio::fd_inner::InnerRawHandle;
#[cfg(windows)]
use crate::vibeio::fd_inner::RawOsHandle;
use crate::vibeio::op::Op;

#[cfg(unix)]
fn start_nonblocking_connect(
    fd: std::os::fd::RawFd,
    address: &ConnectAddress,
) -> Result<(), io::Error> {
    let (raw_addr, raw_addr_len) = address.raw();
    // SAFETY: the borrowed address owns aligned initialized storage and a
    // validated length. connect only reads it during this synchronous call.
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
    address: &ConnectAddress,
) -> Result<(), io::Error> {
    let (raw_addr, raw_addr_len) = address.raw();
    // SAFETY: the borrowed address keeps initialized storage of the validated
    // length alive until this synchronous call returns.
    let connect_result = unsafe { WinSock::connect(socket as SOCKET, raw_addr, raw_addr_len) };

    if connect_result == WinSock::SOCKET_ERROR {
        // SAFETY: reads the calling thread's Winsock error without pointer arguments.
        let err_code = unsafe { WinSock::WSAGetLastError() };
        if !matches!(err_code, WSAEINPROGRESS | WSAEWOULDBLOCK | WSAEALREADY) {
            return Err(io::Error::from_raw_os_error(err_code));
        }
    }

    Ok(())
}

#[cfg(windows)]
fn connectex_bind_error(err_code: i32) -> io::Result<()> {
    // bind documents WSAEINVAL as "already bound". WSAEADDRINUSE instead
    // means an address conflict, not that this socket acquired a local address.
    // Preserve all other errors instead of attempting ConnectEx unbound.
    if err_code == WSAEINVAL {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(err_code))
    }
}

#[cfg(windows)]
fn ensure_connectex_bound(socket: SOCKET, address: &ConnectAddress) -> Result<(), io::Error> {
    let AddressStorage::Inet(addr) = &address.storage;
    let family = addr.ss_family as i32;
    let bind_result = match family {
        x if x == AF_INET as i32 => {
            let local = SOCKADDR_IN {
                sin_family: AF_INET,
                ..Default::default()
            };
            // SAFETY: local is initialized IPv4 storage of the exact supplied
            // size, borrowed only for the duration of bind.
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
                sin6_family: AF_INET6,
                ..Default::default()
            };
            // SAFETY: local is initialized IPv6 storage of the exact supplied
            // size, and bind does not retain the pointer.
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
            ));
        }
    };

    if bind_result == WinSock::SOCKET_ERROR {
        // SAFETY: reads the calling thread's Winsock error without pointer arguments.
        let err_code = unsafe { WinSock::WSAGetLastError() };
        connectex_bind_error(err_code)?;
    }

    Ok(())
}

#[cfg(windows)]
fn load_connect_ex(socket: SOCKET) -> Result<WinSock::LPFN_CONNECTEX, io::Error> {
    let mut bytes_returned: u32 = 0;
    let mut connect_ex: WinSock::LPFN_CONNECTEX = None;
    let mut guid = WSAID_CONNECTEX;

    // SAFETY: GUID and function-pointer output have their exact supplied sizes;
    // all outputs remain live during this synchronous null-OVERLAPPED call.
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
        // SAFETY: reads the calling thread's Winsock error without pointer arguments.
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
    // SAFETY: this option requires no payload; null and zero provide none.
    let result = unsafe {
        WinSock::setsockopt(
            socket,
            SOL_SOCKET,
            SO_UPDATE_CONNECT_CONTEXT,
            std::ptr::null(),
            0,
        )
    };
    if result == WinSock::SOCKET_ERROR {
        // SAFETY: reads the calling thread's Winsock error without pointer arguments.
        let err_code = unsafe { WinSock::WSAGetLastError() };
        return Err(io::Error::from_raw_os_error(err_code));
    }
    Ok(())
}

#[cfg(unix)]
type NativeAddress = libc::sockaddr_storage;
#[cfg(windows)]
type NativeAddress = SOCKADDR_STORAGE;
#[cfg(unix)]
type AddressPointer = *const libc::sockaddr;
#[cfg(windows)]
type AddressPointer = *const SOCKADDR;
#[cfg(unix)]
type AddressLength = libc::socklen_t;
#[cfg(windows)]
type AddressLength = i32;

enum AddressStorage {
    Inet(Box<NativeAddress>),
    #[cfg(unix)]
    Unix(Box<libc::sockaddr_un>),
}

struct ConnectAddress {
    storage: AddressStorage,
    len: AddressLength,
}

impl ConnectAddress {
    fn raw(&self) -> (AddressPointer, AddressLength) {
        let ptr = match &self.storage {
            AddressStorage::Inet(addr) => std::ptr::from_ref(addr.as_ref()).cast(),
            #[cfg(unix)]
            AddressStorage::Unix(addr) => std::ptr::from_ref(addr.as_ref()).cast(),
        };
        (ptr, self.len)
    }

    fn validate_len(len: AddressLength, capacity: usize) -> io::Result<()> {
        let len = usize::try_from(len).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "negative socket address length",
            )
        })?;
        if !(2..=capacity).contains(&len) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "socket address length exceeds owned storage or omits its family",
            ));
        }
        Ok(())
    }
}

pub struct ConnectOp<'a> {
    handle: &'a InnerRawHandle,
    addr: Option<ConnectAddress>,
    #[cfg(windows)]
    connect_ex: WinSock::LPFN_CONNECTEX,
    #[cfg(windows)]
    completion_bound: bool,
    completion_token: Option<usize>,
    #[cfg(any(unix, windows))]
    poll_connect_started: bool,
}

impl<'a> ConnectOp<'a> {
    /// Own aligned address storage; callers cannot submit a dangling raw pointer.
    pub fn new(
        handle: &'a InnerRawHandle,
        addr: NativeAddress,
        len: AddressLength,
    ) -> io::Result<Self> {
        ConnectAddress::validate_len(len, std::mem::size_of::<NativeAddress>())?;
        crate::vibeio::op::socket_addr::sockaddr_storage_to_socketaddr(&addr, len as usize)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidInput, error))?;
        Ok(Self::with_address(
            handle,
            ConnectAddress {
                storage: AddressStorage::Inet(Box::new(addr)),
                len,
            },
        ))
    }

    #[cfg(unix)]
    pub fn new_unix(
        handle: &'a InnerRawHandle,
        addr: libc::sockaddr_un,
        len: libc::socklen_t,
    ) -> io::Result<Self> {
        ConnectAddress::validate_len(len, std::mem::size_of::<libc::sockaddr_un>())?;
        if addr.sun_family as i32 != libc::AF_UNIX {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "expected Unix socket family",
            ));
        }
        Ok(Self::with_address(
            handle,
            ConnectAddress {
                storage: AddressStorage::Unix(Box::new(addr)),
                len,
            },
        ))
    }

    fn with_address(handle: &'a InnerRawHandle, addr: ConnectAddress) -> Self {
        Self {
            handle,
            addr: Some(addr),
            completion_token: None,
            #[cfg(windows)]
            connect_ex: None,
            #[cfg(windows)]
            completion_bound: false,
            poll_connect_started: false,
        }
    }

    #[cfg(any(target_os = "linux", windows, test))]
    fn address(&self) -> (AddressPointer, AddressLength) {
        self.addr.as_ref().expect("connect address missing").raw()
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
            let crate::vibeio::fd_inner::RawOsHandle::Socket(handle) = self.handle.handle else {
                return Poll::Ready(Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "invalid raw handle",
                )));
            };

            if let Err(err) = start_nonblocking_connect(
                handle,
                self.addr.as_ref().expect("connect address missing"),
            ) {
                return Poll::Ready(Err(err));
            };

            self.poll_connect_started = true;
        }

        #[cfg(unix)]
        {
            let mut socket_error: libc::c_int = 0;
            let mut socket_error_len = mem::size_of::<libc::c_int>() as libc::socklen_t;
            // SAFETY: both outputs are live writable storage, and the option
            // capacity matches the integer result; no pointer is retained.
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
            // SAFETY: the supplied peer buffer and length are live writable
            // outputs. Only success/failure is used; no address fields are read.
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
            // SAFETY: socket_error and its length are writable initialized
            // outputs of the supplied capacity, used only during getsockopt.
            let getsockopt_result = unsafe {
                WinSock::getsockopt(
                    socket,
                    SOL_SOCKET,
                    WinSock::SO_ERROR,
                    (&mut socket_error as *mut i32).cast(),
                    &mut socket_error_len,
                )
            };
            if getsockopt_result == SOCKET_ERROR {
                // SAFETY: reads the calling thread's Winsock error without pointer arguments.
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
            // SAFETY: peer and peer_len are writable initialized output storage
            // of the supplied capacity; only the call's success status is used.
            let getpeername_result = unsafe {
                WinSock::getpeername(
                    socket,
                    (&mut peer as *mut SOCKADDR_STORAGE).cast::<SOCKADDR>(),
                    &mut peer_len,
                )
            };

            if getpeername_result == SOCKET_ERROR {
                // SAFETY: reads the calling thread's Winsock error without pointer arguments.
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
            return Poll::Ready(Err(crate::vibeio::op::io_util::completion_error(result)));
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
            ensure_connectex_bound(socket, self.addr.as_ref().expect("connect address missing"))?;
            self.completion_bound = true;
        }

        if self.connect_ex.is_none() {
            self.connect_ex = load_connect_ex(socket)?;
        }
        let Some(connect_ex_fn) = self.connect_ex else {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "ConnectEx extension function is unavailable",
            ));
        };

        // SAFETY: boxed address storage stays alive through completion; Drop
        // transfers it to cancellation retention if necessary. The driver owns
        // OVERLAPPED through acknowledgement, and no initial send data is supplied.
        let connect_result = unsafe {
            connect_ex_fn(
                socket,
                self.address().0,
                self.address().1,
                std::ptr::null(),
                0,
                std::ptr::null_mut(),
                overlapped,
            )
        };

        if connect_result != 0 {
            return Ok(());
        }

        // SAFETY: reads the calling thread's Winsock error without pointer arguments.
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

        let (addr, addrlen) = self.address();

        let entry = opcode::Connect::new(types::Fd(self.handle.handle), addr, addrlen)
            .build()
            .user_data(user_data);

        Ok(entry)
    }
}

impl Drop for ConnectOp<'_> {
    fn drop(&mut self) {
        if let Some(token) = self.completion_token.take() {
            // Retain the original allocation, not a copy: the kernel may still
            // dereference the submitted pointer after this future is cancelled.
            self.handle
                .cancel_completion(token, Box::new(self.addr.take()));
        }
    }
}

#[cfg(test)]
mod ownership_tests {
    use super::*;
    use std::rc::Rc;

    #[cfg(windows)]
    #[test]
    fn connectex_bind_errors_preserve_address_conflicts() {
        assert!(connectex_bind_error(WSAEINVAL).is_ok());
        for error in [
            WinSock::WSAEADDRINUSE,
            WinSock::WSAEACCES,
            WinSock::WSAENOTSOCK,
            WinSock::WSAENOBUFS,
            WinSock::WSAEAFNOSUPPORT,
        ] {
            assert_eq!(
                connectex_bind_error(error).unwrap_err().raw_os_error(),
                Some(error)
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn connectex_binding_assigns_and_preserves_the_local_port() {
        use std::os::windows::io::AsRawSocket;
        for destination in ["127.0.0.1:12345", "[::1]:12345"] {
            let destination: std::net::SocketAddr = destination.parse().unwrap();
            let socket = socket2::Socket::new(
                socket2::Domain::for_address(destination),
                socket2::Type::STREAM,
                Some(socket2::Protocol::TCP),
            )
            .unwrap();
            let handle = InnerRawHandle::for_mock_completion(Rc::new(AnyDriver::new_mock()));
            let (storage, length) = crate::vibeio::op::socket_addr_to_raw(destination);
            let op = ConnectOp::new(&handle, storage, length).unwrap();
            ensure_connectex_bound(socket.as_raw_socket() as SOCKET, op.addr.as_ref().unwrap())
                .unwrap();
            let local = socket.local_addr().unwrap().as_socket().unwrap();
            assert_ne!(local.port(), 0);
            assert_eq!(local.is_ipv4(), destination.is_ipv4());
            // The second bind fails with WSAEINVAL; it must preserve the first
            // binding rather than demand a new ephemeral port.
            ensure_connectex_bound(socket.as_raw_socket() as SOCKET, op.addr.as_ref().unwrap())
                .unwrap();
            assert_eq!(socket.local_addr().unwrap().as_socket().unwrap(), local);
            assert_eq!(
                ensure_connectex_bound(WinSock::INVALID_SOCKET, op.addr.as_ref().unwrap())
                    .unwrap_err()
                    .raw_os_error(),
                Some(WinSock::WSAENOTSOCK)
            );
        }
    }

    fn inet_address() -> NativeAddress {
        crate::vibeio::op::socket_addr_to_raw("127.0.0.1:0".parse().unwrap()).0
    }

    #[test]
    fn address_survives_moves_and_cancellation_on_original_driver() {
        for entered in [false, true] {
            let owner = Rc::new(AnyDriver::new_mock());
            let handle = InnerRawHandle::for_mock_completion(owner.clone());
            let mut op = ConnectOp::new(&handle, inet_address(), 16).unwrap();
            let original = op.address();
            // Moving the operation cannot invalidate its submitted address.
            let mut moved = Box::new(op);
            assert_eq!(moved.address(), original);
            moved.completion_token = Some(41);
            op = *moved;
            assert_eq!(op.address(), original);
            drop(op);
            // Also verify cancellation under a different entered runtime.
            if entered {
                let runtime = crate::vibeio::executor::Runtime::new(AnyDriver::new_mock());
                let handle = InnerRawHandle::for_mock_completion(owner.clone());
                runtime.block_on(async move {
                    let mut op = ConnectOp::new(&handle, inet_address(), 16).unwrap();
                    op.completion_token = Some(42);
                    drop(op);
                });
            }
            let AnyDriver::Mock(driver) = owner.as_ref() else {
                unreachable!()
            };
            let held = driver.ignored.take();
            assert_eq!(held.len(), if entered { 2 } else { 1 });
            for (token, data) in held {
                let address = data.downcast::<Option<ConnectAddress>>().unwrap().unwrap();
                if token == 41 {
                    assert_eq!(address.raw(), original);
                }
                assert_eq!(address.len, 16);
                let storage = match address.storage {
                    AddressStorage::Inet(storage) => storage,
                    #[cfg(unix)]
                    AddressStorage::Unix(_) => panic!("expected internet address"),
                };
                #[cfg(unix)]
                assert_eq!(i32::from(storage.ss_family), libc::AF_INET);
                #[cfg(windows)]
                assert_eq!(storage.ss_family, AF_INET);
            }
        }
    }

    #[test]
    fn tcp_connect_uses_owned_address_on_live_driver() {
        use crate::vibeio::net::{PollTcpStream, TcpStream};

        #[cfg(unix)]
        let driver = AnyDriver::new_mio().unwrap();
        #[cfg(windows)]
        let driver = AnyDriver::new_iocp().unwrap();
        let drivers = vec![driver];
        #[cfg(target_os = "linux")]
        let drivers = {
            let mut drivers = drivers;
            match AnyDriver::new_uring_custom(io_uring::IoUring::builder()) {
                Ok(driver) => drivers.push(driver),
                Err(error)
                    if matches!(
                        error.raw_os_error(),
                        Some(libc::EPERM | libc::ENOSYS | libc::EOPNOTSUPP)
                    ) =>
                {
                    eprintln!("io_uring connect check unavailable: {error}")
                }
                Err(error) => panic!("io_uring initialization failed: {error}"),
            }
            drivers
        };
        for driver in drivers {
            let runtime = crate::vibeio::executor::Runtime::new(driver);
            for bind_address in ["127.0.0.1:0", "[::1]:0"] {
                let listener = std::net::TcpListener::bind(bind_address).unwrap();
                listener.set_nonblocking(true).unwrap();
                let address = listener.local_addr().unwrap();
                runtime.block_on(async move {
                    let stream = crate::vibeio::time::timeout(
                        crate::vibeio::test_support::WATCHDOG,
                        TcpStream::connect(address),
                    )
                    .await
                    .unwrap()
                    .unwrap();
                    assert_eq!(stream.peer_addr().unwrap(), address);
                    let (_, peer) = listener.accept().unwrap();
                    assert_eq!(peer, stream.local_addr().unwrap());

                    let stream = crate::vibeio::time::timeout(
                        crate::vibeio::test_support::WATCHDOG,
                        PollTcpStream::connect(address),
                    )
                    .await
                    .unwrap()
                    .unwrap();
                    assert_eq!(stream.peer_addr().unwrap(), address);
                    let (_, peer) = listener.accept().unwrap();
                    assert_eq!(peer, stream.local_addr().unwrap());
                });
            }
        }
    }

    #[test]
    fn invalid_address_lengths_are_rejected() {
        let owner = Rc::new(AnyDriver::new_mock());
        let handle = InnerRawHandle::for_mock_completion(owner);
        for len in [
            0,
            1,
            2,
            15,
            (std::mem::size_of::<NativeAddress>() + 1) as AddressLength,
        ] {
            assert!(
                matches!(ConnectOp::new(&handle, inet_address(), len), Err(e) if e.kind() == io::ErrorKind::InvalidInput)
            );
        }
        #[cfg(windows)]
        assert!(ConnectOp::new(&handle, inet_address(), -1).is_err());
    }

    #[test]
    fn internet_address_family_and_complete_structure_are_required() {
        let owner = Rc::new(AnyDriver::new_mock());
        let handle = InnerRawHandle::for_mock_completion(owner);
        for address in ["192.0.2.1:1234", "[2001:db8::1]:4321"] {
            let (storage, length) = crate::vibeio::op::socket_addr_to_raw(address.parse().unwrap());
            assert!(ConnectOp::new(&handle, storage, length).is_ok());
            assert!(
                matches!(ConnectOp::new(&handle, storage, length - 1), Err(error) if error.kind() == io::ErrorKind::InvalidInput)
            );
            let mut wrong_family = storage;
            wrong_family.ss_family = 0;
            assert!(
                matches!(ConnectOp::new(&handle, wrong_family, length), Err(error) if error.kind() == io::ErrorKind::InvalidInput)
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn unix_address_storage_remains_stable() {
        let owner = Rc::new(AnyDriver::new_mock());
        let handle = InnerRawHandle::for_mock_completion(owner);
        // SAFETY: sockaddr_un is an integer-only C structure; zeroing initializes
        // its path terminator and any platform length/padding fields.
        let mut addr: libc::sockaddr_un = unsafe { std::mem::zeroed() };
        addr.sun_family = libc::AF_UNIX as _;
        addr.sun_path[0] = b'x' as _;
        let len = (std::mem::offset_of!(libc::sockaddr_un, sun_path) + 2) as libc::socklen_t;
        let mut wrong_family = addr;
        wrong_family.sun_family = libc::AF_INET as _;
        assert!(
            matches!(ConnectOp::new_unix(&handle, wrong_family, len), Err(error) if error.kind() == io::ErrorKind::InvalidInput)
        );
        let op = ConnectOp::new_unix(&handle, addr, len).unwrap();
        let original = op.address();
        let moved = Box::new(op);
        assert_eq!(moved.address(), original);
        let AddressStorage::Unix(addr) = &moved.addr.as_ref().unwrap().storage else {
            panic!("expected Unix address")
        };
        assert_eq!(addr.sun_path[0], b'x' as libc::c_char);
    }
}
