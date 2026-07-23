//! TCP listener types for async I/O.
//!
//! This module provides:
//! - [`TcpListener`]: An async TCP listener that can use either completion-based or poll-based I/O.
//!
//! # Implementation details
//!
//! - On Linux with io_uring support, TCP operations use native async syscalls via the async driver.
//! - When io_uring completion is available, operations complete directly.
//! - For platforms without native async support, operations fall back to synchronous std::net calls.
//! - The runtime must be active when calling these types' methods; otherwise they will panic.

use std::future::poll_fn;
use std::io;
use std::mem::ManuallyDrop;
use std::net::{SocketAddr, TcpListener as StdTcpListener, ToSocketAddrs};
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
#[cfg(windows)]
use std::os::windows::io::{AsRawSocket, FromRawSocket, IntoRawSocket, RawSocket};

use mio::Interest;
#[cfg(windows)]
use windows_sys::Win32::Networking::WinSock::{
    self as WinSock, AF_INET, AF_INET6, IPPROTO_IPV6, IPV6_V6ONLY, SOCKADDR, SOCKADDR_IN,
    SOCKADDR_IN6, SOCKADDR_STORAGE, SOCK_STREAM, SOMAXCONN, WSADATA,
};

use crate::op::AcceptOp;
use crate::{fd_inner::InnerRawHandle, net::TcpStream};

#[cfg(unix)]
fn socket_addr_to_raw(
    address: SocketAddr,
) -> (libc::c_int, libc::sockaddr_storage, libc::socklen_t) {
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

            let mut storage = std::mem::MaybeUninit::<libc::sockaddr_storage>::zeroed();
            unsafe {
                storage
                    .as_mut_ptr()
                    .cast::<libc::sockaddr_in>()
                    .write(sockaddr);
                (
                    libc::AF_INET,
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

            let mut storage = std::mem::MaybeUninit::<libc::sockaddr_storage>::zeroed();
            unsafe {
                storage
                    .as_mut_ptr()
                    .cast::<libc::sockaddr_in6>()
                    .write(sockaddr);
                (
                    libc::AF_INET6,
                    storage.assume_init(),
                    std::mem::size_of::<libc::sockaddr_in6>() as libc::socklen_t,
                )
            }
        }
    }
}

#[cfg(unix)]
fn bind_one(address: SocketAddr) -> Result<StdTcpListener, io::Error> {
    let (domain, raw_addr, raw_addr_len) = socket_addr_to_raw(address);
    let socket_fd = unsafe { libc::socket(domain, libc::SOCK_STREAM, 0) };
    if socket_fd == -1 {
        return Err(io::Error::last_os_error());
    }

    let reuse_addr: libc::c_int = 1;
    let reuse_addr_result = unsafe {
        libc::setsockopt(
            socket_fd,
            libc::SOL_SOCKET,
            libc::SO_REUSEADDR,
            (&reuse_addr as *const libc::c_int).cast(),
            std::mem::size_of_val(&reuse_addr) as libc::socklen_t,
        )
    };
    if reuse_addr_result == -1 {
        let err = io::Error::last_os_error();
        let _ = unsafe { libc::close(socket_fd) };
        return Err(err);
    }

    if domain == libc::AF_INET6 {
        let ipv6_only: libc::c_int = 0;
        let ipv6_only_result = unsafe {
            libc::setsockopt(
                socket_fd,
                libc::IPPROTO_IPV6,
                libc::IPV6_V6ONLY,
                (&ipv6_only as *const libc::c_int).cast(),
                std::mem::size_of_val(&ipv6_only) as libc::socklen_t,
            )
        };
        if ipv6_only_result == -1 {
            let err = io::Error::last_os_error();
            let _ = unsafe { libc::close(socket_fd) };
            return Err(err);
        }
    }

    let bind_result = unsafe {
        libc::bind(
            socket_fd,
            (&raw_addr as *const libc::sockaddr_storage).cast::<libc::sockaddr>(),
            raw_addr_len,
        )
    };
    if bind_result == -1 {
        let err = io::Error::last_os_error();
        let _ = unsafe { libc::close(socket_fd) };
        return Err(err);
    }

    let listen_result = unsafe { libc::listen(socket_fd, libc::SOMAXCONN) };
    if listen_result == -1 {
        let err = io::Error::last_os_error();
        let _ = unsafe { libc::close(socket_fd) };
        return Err(err);
    }

    Ok(unsafe { StdTcpListener::from_raw_fd(socket_fd) })
}

#[cfg(windows)]
fn socket_addr_to_raw(address: SocketAddr) -> (i32, SOCKADDR_STORAGE, i32) {
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

            (
                AF_INET as i32,
                storage,
                std::mem::size_of::<SOCKADDR_IN>() as i32,
            )
        }
        SocketAddr::V6(address) => {
            let mut sockaddr = SOCKADDR_IN6::default();
            sockaddr.sin6_family = AF_INET6;
            sockaddr.sin6_port = address.port().to_be();
            sockaddr.sin6_flowinfo = address.flowinfo();
            sockaddr.sin6_addr.u.Byte = address.ip().octets();
            sockaddr.Anonymous.sin6_scope_id = address.scope_id();

            let mut storage = SOCKADDR_STORAGE::default();
            unsafe {
                std::ptr::copy_nonoverlapping(
                    &sockaddr as *const SOCKADDR_IN6 as *const u8,
                    &mut storage as *mut SOCKADDR_STORAGE as *mut u8,
                    std::mem::size_of::<SOCKADDR_IN6>(),
                );
            }

            (
                AF_INET6 as i32,
                storage,
                std::mem::size_of::<SOCKADDR_IN6>() as i32,
            )
        }
    }
}

#[cfg(windows)]
fn bind_one(address: SocketAddr) -> Result<StdTcpListener, io::Error> {
    // 0x202 = MAKEWORD(2, 2)
    let mut wsadata = WSADATA::default();
    if unsafe { WinSock::WSAStartup(0x202, &mut wsadata as *mut WSADATA) } != 0 {
        return Err(io::Error::last_os_error());
    }

    let (domain, raw_addr, raw_addr_len) = socket_addr_to_raw(address);
    let socket = unsafe { WinSock::socket(domain, SOCK_STREAM, 0) };
    if socket == WinSock::INVALID_SOCKET {
        let err_code = unsafe { WinSock::WSAGetLastError() };
        return Err(io::Error::from_raw_os_error(err_code));
    }

    if domain == AF_INET6 as i32 {
        let ipv6_only: i32 = 0;
        let ipv6_only_result = unsafe {
            WinSock::setsockopt(
                socket,
                IPPROTO_IPV6 as i32,
                IPV6_V6ONLY as i32,
                (&ipv6_only as *const i32).cast(),
                std::mem::size_of_val(&ipv6_only) as i32,
            )
        };
        if ipv6_only_result == WinSock::SOCKET_ERROR {
            let err_code = unsafe { WinSock::WSAGetLastError() };
            let _ = unsafe { WinSock::closesocket(socket) };
            return Err(io::Error::from_raw_os_error(err_code));
        }
    }

    let bind_result = unsafe {
        WinSock::bind(
            socket,
            (&raw_addr as *const SOCKADDR_STORAGE).cast::<SOCKADDR>(),
            raw_addr_len,
        )
    };
    if bind_result == WinSock::SOCKET_ERROR {
        let err_code = unsafe { WinSock::WSAGetLastError() };
        let _ = unsafe { WinSock::closesocket(socket) };
        return Err(io::Error::from_raw_os_error(err_code));
    }

    let listen_result = unsafe { WinSock::listen(socket, SOMAXCONN as i32) };
    if listen_result == WinSock::SOCKET_ERROR {
        let err_code = unsafe { WinSock::WSAGetLastError() };
        let _ = unsafe { WinSock::closesocket(socket) };
        return Err(io::Error::from_raw_os_error(err_code));
    }

    Ok(unsafe { StdTcpListener::from_raw_socket(socket as RawSocket) })
}

/// An async TCP listener that can use either completion-based or poll-based I/O.
///
/// This is the async version of [`std::net::TcpListener`].
///
/// # Implementation details
///
/// - On Linux with io_uring support, TCP operations use native async syscalls via the async driver.
/// - When io_uring completion is available, operations complete directly.
/// - For platforms without native async support, operations fall back to synchronous std::net calls.
/// - The runtime must be active when calling these methods; otherwise they will panic.
///
/// # Examples
///
/// ```ignore
/// use vibeio::net::TcpListener;
///
/// let listener = TcpListener::bind("127.0.0.1:8080").await?;
/// loop {
///     let (stream, addr) = listener.accept().await?;
///     println!("Connection from: {}", addr);
/// }
/// ```
pub struct TcpListener {
    inner: StdTcpListener,
    handle: ManuallyDrop<InnerRawHandle>,
}

impl TcpListener {
    /// Creates a new `TcpListener` which will be bound to the specified address.
    ///
    /// This is the async version of [`std::net::TcpListener::bind`].
    ///
    /// # Errors
    ///
    /// This function will return an error in the following situations:
    /// - DNS resolution fails
    /// - The address is already in use
    /// - The process lacks permissions to bind to the address
    /// - The runtime is not active
    #[inline]
    pub fn bind(address: impl ToSocketAddrs) -> Result<Self, io::Error> {
        let addresses = address.to_socket_addrs()?;
        let mut last_error = None;
        for address in addresses {
            match bind_one(address) {
                Ok(inner) => return Self::from_std(inner),
                Err(err) => last_error = Some(err),
            }
        }

        Err(last_error
            .unwrap_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no addresses")))
    }

    /// Creates a new `TcpListener` from a standard library `TcpListener`.
    ///
    /// # Errors
    ///
    /// This function will return an error if registration with the async driver fails.
    #[inline]
    pub fn from_std(inner: std::net::TcpListener) -> Result<Self, io::Error> {
        #[cfg(unix)]
        let handle = ManuallyDrop::new(InnerRawHandle::new(inner.as_raw_fd(), Interest::READABLE)?);
        #[cfg(windows)]
        let handle = ManuallyDrop::new(InnerRawHandle::new(
            crate::fd_inner::RawOsHandle::Socket(inner.as_raw_socket()),
            Interest::READABLE,
        )?);
        inner.set_nonblocking(!handle.uses_completion())?;
        Ok(Self { inner, handle })
    }

    /// Creates a new `TcpListener` from a standard library `TcpListener` in poll mode.
    ///
    /// This could be useful when using cloned `TcpListener` on Windows.
    ///
    /// # Errors
    ///
    /// This function will return an error if registration with the async driver fails.
    #[cfg(windows)]
    #[inline]
    pub fn from_std_poll(inner: std::net::TcpListener) -> Result<Self, io::Error> {
        let handle = ManuallyDrop::new(InnerRawHandle::new_with_mode(
            crate::fd_inner::RawOsHandle::Socket(inner.as_raw_socket()),
            Interest::READABLE,
            crate::driver::RegistrationMode::Poll,
        )?);
        inner.set_nonblocking(!handle.uses_completion())?;
        Ok(Self { inner, handle })
    }

    /// Returns the local address of this listener.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket is not bound.
    #[inline]
    pub fn local_addr(&self) -> Result<SocketAddr, io::Error> {
        self.inner.local_addr()
    }

    /// Accepts a new incoming connection from this listener.
    ///
    /// This is the async version of [`std::net::TcpListener::accept`].
    ///
    /// # Errors
    ///
    /// This function will return an error in the following situations:
    /// - The listener is not bound to an address
    /// - The runtime is not active
    #[inline]
    pub async fn accept(&self) -> Result<(TcpStream, SocketAddr), io::Error> {
        let mut op = AcceptOp::new(&self.handle);
        let (raw, address) = poll_fn(move |cx| self.handle.poll_op(cx, &mut op)).await?;
        // Recreate a std TcpStream from the raw fd and convert it into our async TcpStream.
        // If conversion fails, the std TcpStream will be dropped and the fd closed.
        #[cfg(unix)]
        let std_stream = unsafe { std::net::TcpStream::from_raw_fd(raw) };
        #[cfg(windows)]
        let crate::fd_inner::RawOsHandle::Socket(raw) = raw
        else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid raw handle",
            ));
        };
        #[cfg(windows)]
        let std_stream = unsafe { std::net::TcpStream::from_raw_socket(raw) };
        match TcpStream::from_std(std_stream) {
            Ok(stream) => Ok((stream, address)),
            Err(err) => Err(err),
        }
    }
}

#[cfg(unix)]
impl AsRawFd for TcpListener {
    #[inline]
    fn as_raw_fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }
}

#[cfg(unix)]
impl IntoRawFd for TcpListener {
    #[inline]
    fn into_raw_fd(self) -> RawFd {
        let mut this = ManuallyDrop::new(self);

        // Safety: `this` will not be dropped, so we must drop the registration handle manually.
        // We then move out the inner std stream and transfer its fd ownership to the caller.
        unsafe {
            ManuallyDrop::drop(&mut this.handle);
            std::ptr::read(&this.inner).into_raw_fd()
        }
    }
}

#[cfg(windows)]
impl AsRawSocket for TcpListener {
    #[inline]
    fn as_raw_socket(&self) -> RawSocket {
        self.inner.as_raw_socket()
    }
}

#[cfg(windows)]
impl IntoRawSocket for TcpListener {
    #[inline]
    fn into_raw_socket(self) -> RawSocket {
        let mut this = ManuallyDrop::new(self);

        // Safety: `this` will not be dropped, so we must drop the registration handle manually.
        // We then move out the inner std stream and transfer its socket ownership to the caller.
        unsafe {
            ManuallyDrop::drop(&mut this.handle);
            std::ptr::read(&this.inner).into_raw_socket()
        }
    }
}

impl Drop for TcpListener {
    #[inline]
    fn drop(&mut self) {
        // Safety: The struct is dropped after the handle is dropped.
        unsafe {
            ManuallyDrop::drop(&mut self.handle);
        }
    }
}
