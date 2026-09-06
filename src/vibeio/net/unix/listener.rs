//! Unix domain socket listener types for async I/O.
//!
//! This module provides:
//! - [`UnixListener`]: An async Unix domain socket listener.
//!
//! # Implementation details
//!
//! - Unix domain sockets use native async syscalls via the async driver when available.
//! - When io_uring completion is available, operations complete directly.
//! - Poll mode uses nonblocking socket calls and driver readiness notifications.
//! - Register sockets and drive async I/O inside a runtime. Registration without
//!   one returns an error; direct address/option queries need no current runtime.

use std::future::poll_fn;
use std::io;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::os::unix::net::{
    SocketAddr, UnixListener as StdUnixListener, UnixStream as StdUnixStream,
};
use std::path::Path;

use mio::Interest;

use crate::vibeio::fd_inner::InnerRawHandle;
use crate::vibeio::net::UnixStream;
use crate::vibeio::op::AcceptUnixOp;

/// An async Unix domain socket listener.
///
/// This is the async version of [`std::os::unix::net::UnixListener`].
///
/// # Implementation details
///
/// - Unix domain sockets use native async syscalls via the async driver when available.
/// - When io_uring completion is available, operations complete directly.
/// - Poll mode uses nonblocking socket calls and driver readiness notifications.
/// - Registration needs an entered runtime and returns an error without one.
///   Drive async I/O inside a runtime; direct socket queries need no current runtime.
///
/// # Examples
///
/// See "Unix socket exchange and path cleanup" in
/// `tools/vibeio-check/EXAMPLES.md`. Bind is synchronous; accepting is async.
/// Dropping a listener closes its socket but does not unlink its pathname.
pub struct UnixListener {
    // Deregister before closing the socket (field declaration order).
    handle: InnerRawHandle,
    inner: StdUnixListener,
}

impl UnixListener {
    /// Creates a new `UnixListener` which will be bound to the specified path.
    ///
    /// This is the async version of [`std::os::unix::net::UnixListener::bind`].
    ///
    /// # Errors
    ///
    /// This function will return an error in the following situations:
    /// - The path does not exist
    /// - The path is too long
    /// - The path is already in use
    /// - The process lacks permissions
    /// - The runtime is not active
    #[inline]
    pub fn bind(path: impl AsRef<Path>) -> Result<Self, io::Error> {
        let inner = StdUnixListener::bind(path)?;
        Self::from_std(inner)
    }

    /// Creates a new `UnixListener` from a standard library `UnixListener`.
    ///
    /// # Errors
    ///
    /// This function will return an error if registration with the async driver fails.
    #[inline]
    pub fn from_std(inner: StdUnixListener) -> Result<Self, io::Error> {
        let handle = InnerRawHandle::new(inner.as_raw_fd(), Interest::READABLE)?;
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
    /// This is the async version of [`std::os::unix::net::UnixListener::accept`].
    ///
    /// # Errors
    ///
    /// This function will return an error in the following situations:
    /// - The listener is not bound to an address
    /// - The runtime is not active
    #[inline]
    pub async fn accept(&self) -> Result<(UnixStream, SocketAddr), io::Error> {
        let mut op = AcceptUnixOp::new(&self.handle);
        let raw = poll_fn(move |cx| self.handle.poll_op(cx, &mut op)).await?;
        let std_stream = unsafe { StdUnixStream::from_raw_fd(raw) };
        let address = std_stream.peer_addr()?;
        let stream = UnixStream::from_std(std_stream)?;
        Ok((stream, address))
    }
}

impl AsRawFd for UnixListener {
    #[inline]
    fn as_raw_fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }
}

impl IntoRawFd for UnixListener {
    #[inline]
    fn into_raw_fd(self) -> RawFd {
        let Self { handle, inner } = self;
        drop(handle);
        inner.into_raw_fd()
    }
}
