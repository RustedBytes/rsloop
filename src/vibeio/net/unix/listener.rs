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
use std::os::fd::{AsRawFd, IntoRawFd, RawFd};
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
    /// Binding is synchronous; the returned listener supports async accepts.
    /// A missing runtime is rejected before binding creates a socket pathname.
    /// As with the standard listener, dropping it does not unlink that pathname.
    /// A later setup or driver-registration failure can also leave it in place;
    /// callers remain responsible for cleanup of paths they own.
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
        // Reject this known setup failure before bind mutates the filesystem.
        // Do not unlink on error: a pathname may have been replaced by another
        // process, and from_std must never remove a caller-owned listener path.
        crate::vibeio::executor::current_driver().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "can't register I/O handle outside runtime",
            )
        })?;
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
        let fd = poll_fn(move |cx| self.handle.poll_op(cx, &mut op)).await?;
        let std_stream = StdUnixStream::from(fd);
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

#[cfg(test)]
mod bind_tests {
    use super::*;
    use crate::vibeio::driver::AnyDriver;

    #[test]
    fn bind_without_runtime_does_not_create_a_socket_path() {
        struct Scratch(std::path::PathBuf);
        impl Drop for Scratch {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(self.0.join("socket"));
                let _ = std::fs::remove_dir(&self.0);
            }
        }

        assert!(crate::vibeio::executor::current_driver().is_none());
        // Keep the path short enough for macOS sockaddr_un as well as Linux.
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let directory =
            std::path::PathBuf::from(format!("/tmp/vb-bind-{}-{stamp:x}", std::process::id()));
        std::fs::create_dir(&directory).unwrap();
        let scratch = Scratch(directory);
        let path = scratch.0.join("socket");
        assert!(matches!(UnixListener::bind(&path), Err(error)
            if error.kind() == io::ErrorKind::NotConnected));
        assert!(
            matches!(std::fs::symlink_metadata(&path), Err(error)
                if error.kind() == io::ErrorKind::NotFound),
            "missing runtime must be rejected before creating the socket path"
        );
        std::fs::write(&path, b"caller-owned contents").unwrap();
        assert!(matches!(UnixListener::bind(&path), Err(error)
            if error.kind() == io::ErrorKind::NotConnected));
        assert_eq!(std::fs::read(&path).unwrap(), b"caller-owned contents");
        std::fs::remove_file(&path).unwrap();
        // Retrying in a valid runtime must not encounter our stale socket.
        let runtime = crate::vibeio::executor::Runtime::new(AnyDriver::new_mio().unwrap());
        runtime.block_on(async move { drop(UnixListener::bind(path).unwrap()) });
    }
}
