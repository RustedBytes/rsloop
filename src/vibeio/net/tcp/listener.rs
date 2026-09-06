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
use std::net::{SocketAddr, TcpListener as StdTcpListener, ToSocketAddrs};
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
#[cfg(windows)]
use std::os::windows::io::{AsRawSocket, FromRawSocket, IntoRawSocket, RawSocket};

use mio::Interest;

use crate::vibeio::op::AcceptOp;
use crate::vibeio::{fd_inner::InnerRawHandle, net::TcpStream};

fn bind_one(address: SocketAddr) -> Result<StdTcpListener, io::Error> {
    let socket = socket2::Socket::new(
        socket2::Domain::for_address(address),
        socket2::Type::STREAM,
        None,
    )?;
    // Preserve the existing platform policy: Unix listeners enable address
    // reuse, while Windows listeners retain their default exclusivity behavior.
    #[cfg(unix)]
    socket.set_reuse_address(true)?;
    if address.is_ipv6() {
        socket.set_only_v6(false)?;
    }
    socket.bind(&address.into())?;
    #[cfg(unix)]
    let backlog = libc::SOMAXCONN;
    #[cfg(windows)]
    let backlog = windows_sys::Win32::Networking::WinSock::SOMAXCONN as i32;
    socket.listen(backlog)?;
    Ok(socket.into())
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
    // Deregister before closing the socket (field declaration order).
    handle: InnerRawHandle,
    inner: StdTcpListener,
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
        let handle = InnerRawHandle::new(inner.as_raw_fd(), Interest::READABLE)?;
        #[cfg(windows)]
        let handle = InnerRawHandle::new(
            crate::vibeio::fd_inner::RawOsHandle::Socket(inner.as_raw_socket()),
            Interest::READABLE,
        )?;
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
        let handle = InnerRawHandle::new_with_mode(
            crate::vibeio::fd_inner::RawOsHandle::Socket(inner.as_raw_socket()),
            Interest::READABLE,
            crate::vibeio::driver::RegistrationMode::Poll,
        )?;
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
        let crate::vibeio::fd_inner::RawOsHandle::Socket(raw) = raw else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "invalid raw handle",
            ));
        };
        #[cfg(windows)]
        let std_stream = unsafe { std::net::TcpStream::from_raw_socket(raw) };
        TcpStream::from_std(std_stream).map(|stream| (stream, address))
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
        let Self { handle, inner } = self;
        drop(handle);
        inner.into_raw_fd()
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
        let Self { handle, inner } = self;
        drop(handle);
        inner.into_raw_socket()
    }
}

#[cfg(test)]
mod socket_creation_tests {
    use super::*;

    #[test]
    fn configured_listener_accepts_and_rejects_duplicate_bind() {
        use std::io::{Read, Write};
        let listener = bind_one("127.0.0.1:0".parse().unwrap()).unwrap();
        let address = listener.local_addr().unwrap();
        #[cfg(unix)]
        assert!(socket2::SockRef::from(&listener).reuse_address().unwrap());
        assert_eq!(
            bind_one(address).unwrap_err().kind(),
            io::ErrorKind::AddrInUse
        );
        let mut peer = std::net::TcpStream::connect(address).unwrap();
        let (mut accepted, remote) = listener.accept().unwrap();
        assert_eq!(remote, peer.local_addr().unwrap());
        accepted
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        peer.write_all(b"hello").unwrap();
        let mut payload = [0; 5];
        accepted.read_exact(&mut payload).unwrap();
        assert_eq!(&payload, b"hello");
    }

    #[test]
    fn ipv6_listener_preserves_dual_stack_option() {
        // A wildcard bind can actually serve both address families. Binding
        // specifically to ::1 can force IPv6-only behavior in the kernel.
        let listener = bind_one("[::]:0".parse().unwrap()).unwrap();
        assert!(!socket2::SockRef::from(&listener).only_v6().unwrap());
        assert!(listener.local_addr().unwrap().is_ipv6());
    }

    #[test]
    fn created_socket_is_close_on_exec() {
        let socket = bind_one("127.0.0.1:0".parse().unwrap()).unwrap();
        #[cfg(unix)]
        {
            // SAFETY: socket owns the live descriptor; F_GETFD has no pointer arguments.
            let flags = unsafe { libc::fcntl(socket.as_raw_fd(), libc::F_GETFD) };
            assert_ne!(flags, -1);
            assert_ne!(flags & libc::FD_CLOEXEC, 0);
        }
        #[cfg(windows)]
        {
            use windows_sys::Win32::Foundation::{GetHandleInformation, HANDLE_FLAG_INHERIT};
            let mut flags = 0;
            // SAFETY: socket owns the live kernel handle and flags is writable.
            let result =
                unsafe { GetHandleInformation(socket.as_raw_socket() as *mut _, &mut flags) };
            assert_ne!(result, 0, "{}", io::Error::last_os_error());
            assert_eq!(flags & HANDLE_FLAG_INHERIT, 0);
        }
    }
}
