//! Unix domain socket stream types for async I/O.
//!
//! This module provides:
//! - [`UnixStream`]: An async Unix domain socket stream that can use either completion-based or poll-based I/O.
//! - [`PollUnixStream`]: A poll-only variant that always uses readiness-based operations.
//!
//! # Implementation details
//!
//! - Unix domain sockets use native async syscalls via the async driver when available.
//! - When io_uring completion is available, operations complete directly.
//! - For platforms without native async support, operations fall back to synchronous std::os::unix::net calls.
//! - The runtime must be active when calling these types' methods; otherwise they will panic.

use std::cell::RefCell;
use std::future::poll_fn;
use std::io::{self, IoSlice};
use std::mem::{ManuallyDrop, MaybeUninit};
use std::net::Shutdown;
use std::os::fd::{AsRawFd, IntoRawFd, RawFd};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::net::{SocketAddr, UnixStream as StdUnixStream};
use std::path::Path;
use std::pin::Pin;
use std::task::{Context, Poll};

use mio::Interest;
use tokio::io::{AsyncRead as TokioAsyncRead, AsyncWrite as TokioAsyncWrite, ReadBuf};

use crate::vibeio::io::{
    AsInnerRawHandle, AsyncReadPoll, AsyncWritePoll, IoBuf, IoBufMut, IoBufTemporaryPoll,
    IoVectoredBuf, IoVectoredBufMut, IoVectoredBufTemporaryPoll,
};
use crate::vibeio::op::{ConnectOp, ReadOp, ReadinessOp, ReadvOp, WriteOp, WritevOp};
use crate::vibeio::{
    driver::RegistrationMode,
    fd_inner::InnerRawHandle,
    io::{AsyncRead, AsyncWrite},
};

#[inline]
fn socket_addr_to_raw(path: &Path) -> Result<(libc::sockaddr_un, libc::socklen_t), io::Error> {
    let bytes = path.as_os_str().as_bytes();
    if bytes.is_empty() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "empty socket path",
        ));
    }
    if bytes.contains(&0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "socket path contains interior NUL byte",
        ));
    }

    // SAFETY: sockaddr_un contains only integer/byte fields, all valid when
    // zeroed. This also initializes the trailing pathname terminator.
    let mut sockaddr = unsafe { MaybeUninit::<libc::sockaddr_un>::zeroed().assume_init() };
    sockaddr.sun_family = libc::AF_UNIX as libc::sa_family_t;

    let max_path_len = sockaddr.sun_path.len();
    if bytes.len() >= max_path_len {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "socket path is too long",
        ));
    }

    for (index, byte) in bytes.iter().copied().enumerate() {
        sockaddr.sun_path[index] = byte as libc::c_char;
    }

    let addr_len =
        (std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1) as libc::socklen_t;
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
    {
        sockaddr.sun_len = addr_len as libc::sa_family_t;
    }

    Ok((sockaddr, addr_len))
}

#[inline]
fn new_socket(
    path: &Path,
) -> Result<(StdUnixStream, libc::sockaddr_un, libc::socklen_t), io::Error> {
    let (raw_addr, raw_addr_len) = socket_addr_to_raw(path)?;
    let socket = socket2::Socket::new(socket2::Domain::UNIX, socket2::Type::STREAM, None)?;
    let owned: std::os::fd::OwnedFd = socket.into();
    Ok((owned.into(), raw_addr, raw_addr_len))
}

/// An async Unix domain socket stream that can use either completion-based or poll-based I/O.
///
/// This is the async version of [`std::os::unix::net::UnixStream`].
///
/// # Implementation details
///
/// - Unix domain sockets use native async syscalls via the async driver when available.
/// - When io_uring completion is available, operations complete directly.
/// - For platforms without native async support, operations fall back to synchronous std::os::unix::net calls.
/// - The runtime must be active when calling these methods; otherwise they will panic.
///
/// # Examples
///
/// ```ignore
/// use vibeio::net::UnixStream;
///
/// let mut stream = UnixStream::connect("/tmp/mysocket").await?;
/// stream.write(b"hello").await.0?;
/// let mut buf = [0u8; 1024];
/// let (read, buf) = stream.read(buf).await;
/// let read = read?;
/// ```
pub struct UnixStream {
    inner: StdUnixStream,
    handle: ManuallyDrop<InnerRawHandle>,
}

/// A poll-only variant that always uses readiness-based operations.
///
/// This type is useful when you want to ensure readiness-based I/O is used,
/// for example when integrating with other readiness-based systems.
///
/// # Implementation details
///
/// - Always uses readiness-based I/O via `mio`.
/// - Can be converted to [`UnixStream`] with adaptive or completion mode.
pub struct PollUnixStream {
    stream: UnixStream,
    read_ready: RefCell<bool>,
    write_ready: RefCell<bool>,
}

impl UnixStream {
    /// Connects to the specified Unix domain socket path.
    ///
    /// This is the async version of [`std::os::unix::net::UnixStream::connect`].
    ///
    /// # Errors
    ///
    /// This function will return an error in the following situations:
    /// - The path does not exist
    /// - The path is too long
    /// - Connection refused
    /// - The runtime is not active
    #[inline]
    pub async fn connect(path: impl AsRef<Path>) -> Result<Self, io::Error> {
        let (inner, raw_addr, raw_addr_len) = new_socket(path.as_ref())?;
        let stream = Self::from_std(inner)?;

        let handle = &stream.handle;
        let mut op = ConnectOp::new_unix(handle, raw_addr, raw_addr_len)?;
        poll_fn(move |cx| handle.poll_op(cx, &mut op)).await?;

        Ok(stream)
    }

    /// Returns the local address of this connection.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket is not connected.
    #[inline]
    pub fn local_addr(&self) -> Result<SocketAddr, io::Error> {
        self.inner.local_addr()
    }

    /// Returns the remote address of this connection.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket is not connected.
    #[inline]
    pub fn peer_addr(&self) -> Result<SocketAddr, io::Error> {
        self.inner.peer_addr()
    }

    /// Shuts down the connection.
    #[inline]
    pub fn shutdown(&self, how: Shutdown) -> Result<(), io::Error> {
        match self.inner.shutdown(how) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == io::ErrorKind::NotConnected => Ok(()), // macOS-specific behavior
            Err(e) => Err(e),
        }
    }

    /// Creates a new `UnixStream` from a standard library `UnixStream`.
    ///
    /// # Errors
    ///
    /// This function will return an error if registration with the async driver fails.
    #[inline]
    pub fn from_std(inner: StdUnixStream) -> Result<Self, io::Error> {
        Self::from_std_with_mode(inner, RegistrationMode::Completion)
    }

    /// Creates a new `UnixStream` from a standard library `UnixStream` with a specific registration mode.
    #[inline]
    pub(crate) fn from_std_with_mode(
        inner: StdUnixStream,
        mode: RegistrationMode,
    ) -> Result<Self, io::Error> {
        let handle = InnerRawHandle::new_with_mode(
            inner.as_raw_fd(),
            Interest::READABLE | Interest::WRITABLE,
            mode,
        )?;
        inner.set_nonblocking(!handle.uses_completion())?;
        let handle = ManuallyDrop::new(handle);
        Ok(Self { inner, handle })
    }

    /// Converts this stream into a poll-only variant.
    ///
    /// The returned `PollUnixStream` will always use readiness-based I/O.
    #[inline]
    pub fn into_poll(self) -> Result<PollUnixStream, io::Error> {
        let mut stream = self;
        stream.handle.rebind_mode(RegistrationMode::Poll)?;
        stream
            .inner
            .set_nonblocking(!stream.handle.uses_completion())?;
        Ok(PollUnixStream {
            stream,
            read_ready: RefCell::new(false),
            write_ready: RefCell::new(false),
        })
    }
}

impl PollUnixStream {
    /// Connects to the specified Unix domain socket path using poll-based I/O.
    #[inline]
    pub async fn connect(path: impl AsRef<Path>) -> Result<Self, io::Error> {
        let (inner, raw_addr, raw_addr_len) = new_socket(path.as_ref())?;
        let stream = Self::from_std(inner)?;

        let handle = &stream.stream.handle;
        let mut op = ConnectOp::new_unix(handle, raw_addr, raw_addr_len)?;
        poll_fn(move |cx| handle.poll_op(cx, &mut op)).await?;

        Ok(stream)
    }

    /// Creates a new `PollUnixStream` from a standard library `UnixStream`.
    #[inline]
    pub fn from_std(inner: StdUnixStream) -> Result<Self, io::Error> {
        Ok(Self {
            stream: UnixStream::from_std_with_mode(inner, RegistrationMode::Poll)?,
            read_ready: RefCell::new(false),
            write_ready: RefCell::new(false),
        })
    }

    /// Converts this poll stream into an adaptive `UnixStream`.
    #[inline]
    pub fn into_adaptive(self) -> UnixStream {
        self.stream
    }

    /// Converts this poll stream into a completion-based `UnixStream`.
    #[inline]
    pub fn into_completion(self) -> Result<UnixStream, io::Error> {
        let mut stream = self.stream;
        stream.handle.rebind_mode(RegistrationMode::Completion)?;
        stream
            .inner
            .set_nonblocking(!stream.handle.uses_completion())?;
        Ok(stream)
    }

    /// Returns the local address of this connection.
    #[inline]
    pub fn local_addr(&self) -> Result<SocketAddr, io::Error> {
        self.stream.local_addr()
    }

    /// Returns the remote address of this connection.
    #[inline]
    pub fn peer_addr(&self) -> Result<SocketAddr, io::Error> {
        self.stream.peer_addr()
    }

    /// Shuts down the connection.
    #[inline]
    pub fn shutdown(&self, how: Shutdown) -> Result<(), io::Error> {
        self.stream.shutdown(how)
    }

    /// Tries to perform an I/O operation on the socket, returning an error if it is not ready.
    #[inline]
    pub fn try_io_readable<Io, IoR>(&self, io: Io) -> io::Result<IoR>
    where
        Io: FnOnce() -> io::Result<IoR>,
    {
        if *self.read_ready.borrow() {
            let result = io();
            if result.is_err() {
                *self.read_ready.borrow_mut() = false;
            }
            result
        } else {
            Err(io::Error::new(io::ErrorKind::WouldBlock, "read not ready"))
        }
    }

    /// Tries to perform an I/O operation on the socket, returning an error if it is not ready.
    #[inline]
    pub fn try_io_writable<Io, IoR>(&self, io: Io) -> io::Result<IoR>
    where
        Io: FnOnce() -> io::Result<IoR>,
    {
        if *self.write_ready.borrow() {
            let result = io();
            if result.is_err() {
                *self.write_ready.borrow_mut() = false;
            }
            result
        } else {
            Err(io::Error::new(io::ErrorKind::WouldBlock, "write not ready"))
        }
    }
}

impl AsRawFd for PollUnixStream {
    #[inline]
    fn as_raw_fd(&self) -> RawFd {
        self.stream.inner.as_raw_fd()
    }
}

impl IntoRawFd for PollUnixStream {
    #[inline]
    fn into_raw_fd(self) -> RawFd {
        self.stream.into_raw_fd()
    }
}

impl TokioAsyncRead for PollUnixStream {
    #[inline]
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<Result<(), io::Error>> {
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }

        let this = self.get_mut();
        // SAFETY: only a raw pointer is passed to the synchronous read below;
        // no initialized-byte reference is formed and no pointer is retained.
        let unfilled = unsafe { buf.unfilled_mut() };
        // SAFETY: ReadBuf exclusively owns this writable region for this poll.
        let buf_temp =
            unsafe { IoBufTemporaryPoll::new_uninit(unfilled.as_mut_ptr().cast(), unfilled.len()) };
        let mut op = ReadOp::new(&this.stream.handle, buf_temp);
        match this.stream.handle.poll_op_poll(cx, &mut op) {
            Poll::Ready(Ok(read)) => {
                // SAFETY: the successful read initialized exactly this prefix.
                unsafe {
                    buf.assume_init(read);
                }
                buf.advance(read);
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(err)) => Poll::Ready(Err(err)),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl TokioAsyncWrite for PollUnixStream {
    #[inline]
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        let this = self.get_mut();
        let buf = unsafe { IoBufTemporaryPoll::new(buf.as_ptr() as *mut u8, buf.len()) };
        let mut op = WriteOp::new(&this.stream.handle, buf);
        this.stream.handle.poll_op_poll(cx, &mut op)
    }

    #[inline]
    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<Result<usize, io::Error>> {
        if bufs.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let this = self.get_mut();
        let bufs = unsafe { IoVectoredBufTemporaryPoll::new(bufs) };
        let mut op = WritevOp::new(&this.stream.handle, bufs);
        this.stream.handle.poll_op_poll(cx, &mut op)
    }

    #[inline]
    fn is_write_vectored(&self) -> bool {
        true
    }

    #[inline]
    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }

    #[inline]
    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Poll::Ready(self.get_mut().shutdown(Shutdown::Write))
    }
}

impl UnixStream {
    /// Creates a new `UnixStream` that is ready to use with readiness-based I/O.
    ///
    /// This is useful when you want to create a Unix stream that uses poll-based I/O.
    #[inline]
    pub fn from_std_poll(inner: StdUnixStream) -> Result<PollUnixStream, io::Error> {
        Ok(PollUnixStream {
            stream: Self::from_std_with_mode(inner, RegistrationMode::Poll)?,
            read_ready: RefCell::new(false),
            write_ready: RefCell::new(false),
        })
    }
}

impl AsRawFd for UnixStream {
    #[inline]
    fn as_raw_fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }
}

impl<'a> AsInnerRawHandle<'a> for UnixStream {
    #[inline]
    fn as_inner_raw_handle(&'a self) -> &'a InnerRawHandle {
        &self.handle
    }
}

impl<'a> AsInnerRawHandle<'a> for PollUnixStream {
    #[inline]
    fn as_inner_raw_handle(&'a self) -> &'a InnerRawHandle {
        self.stream.as_inner_raw_handle()
    }
}

impl IntoRawFd for UnixStream {
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

impl AsyncRead for UnixStream {
    #[inline]
    async fn read<B: IoBufMut>(&mut self, buf: B) -> (Result<usize, io::Error>, B) {
        let handle = &self.handle;
        let mut op = ReadOp::new(handle, buf);
        let result = poll_fn(|cx| handle.poll_op(cx, &mut op)).await;
        (result, op.take_bufs())
    }

    #[inline]
    async fn read_vectored<B: IoVectoredBufMut>(
        &mut self,
        bufs: B,
    ) -> (Result<usize, io::Error>, B) {
        if bufs.is_empty() {
            return (Ok(0), bufs);
        }
        let handle = &self.handle;
        let mut op = ReadvOp::new(handle, bufs);
        let result = poll_fn(|cx| handle.poll_op(cx, &mut op)).await;
        (result, op.take_bufs())
    }
}

impl AsyncWrite for UnixStream {
    #[inline]
    async fn write<B: IoBuf>(&mut self, buf: B) -> (Result<usize, io::Error>, B) {
        let handle = &self.handle;
        let mut op = WriteOp::new(handle, buf);
        let result = poll_fn(|cx| handle.poll_op(cx, &mut op)).await;
        (result, op.take_bufs())
    }

    #[inline]
    async fn flush(&mut self) -> Result<(), io::Error> {
        Ok(())
    }

    #[inline]
    async fn write_vectored<B: IoVectoredBuf>(&mut self, bufs: B) -> (Result<usize, io::Error>, B) {
        if bufs.is_empty() {
            return (Ok(0), bufs);
        }
        let handle = &self.handle;
        let mut op = WritevOp::new(handle, bufs);
        let result = poll_fn(|cx| handle.poll_op(cx, &mut op)).await;
        (result, op.take_bufs())
    }
}

impl AsyncReadPoll for PollUnixStream {
    #[inline]
    fn poll_readable(&self, cx: &mut std::task::Context) -> std::task::Poll<io::Result<()>> {
        if *self.read_ready.borrow() {
            return Poll::Ready(Ok(()));
        }
        let poll = self
            .stream
            .handle
            .poll_op_poll(cx, &mut ReadinessOp::new_readable(&self.stream.handle))?;
        *self.read_ready.borrow_mut() = true;
        poll.map(Ok)
    }
}

impl AsyncWritePoll for PollUnixStream {
    #[inline]
    fn poll_writable(&self, cx: &mut std::task::Context) -> std::task::Poll<io::Result<()>> {
        if *self.write_ready.borrow() {
            return Poll::Ready(Ok(()));
        }
        let poll = self
            .stream
            .handle
            .poll_op_poll(cx, &mut ReadinessOp::new_writable(&self.stream.handle))?;
        *self.write_ready.borrow_mut() = true;
        poll.map(Ok)
    }
}

impl Drop for UnixStream {
    #[inline]
    fn drop(&mut self) {
        // Safety: The struct is dropped after the handle is dropped.
        unsafe {
            ManuallyDrop::drop(&mut self.handle);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vibeio::{driver::AnyDriver, executor::Runtime};

    #[test]
    fn unix_address_rejects_empty_nul_and_overlong_paths() {
        use std::ffi::OsStr;
        let (short, _) = socket_addr_to_raw(Path::new("x")).unwrap();
        let capacity = short.sun_path.len();
        for bytes in [Vec::new(), b"a\0b".to_vec(), vec![b'x'; capacity]] {
            let path = Path::new(OsStr::from_bytes(&bytes));
            assert_eq!(
                socket_addr_to_raw(path).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
            assert_eq!(
                new_socket(path).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
        }
    }

    #[test]
    fn unix_address_preserves_maximum_and_non_utf8_pathnames() {
        use std::ffi::OsStr;
        let (short, _) = socket_addr_to_raw(Path::new("x")).unwrap();
        let capacity = short.sun_path.len();
        for bytes in [vec![b'x'; capacity - 1], vec![b'a', 0xff, b'b']] {
            let path = Path::new(OsStr::from_bytes(&bytes));
            let (address, length) = socket_addr_to_raw(path).unwrap();
            assert_eq!(address.sun_family as i32, libc::AF_UNIX);
            assert_eq!(
                length as usize,
                std::mem::offset_of!(libc::sockaddr_un, sun_path) + bytes.len() + 1
            );
            let encoded: Vec<_> = address.sun_path.iter().map(|&byte| byte as u8).collect();
            assert_eq!(&encoded[..bytes.len()], bytes.as_slice());
            assert!(encoded[bytes.len()..].iter().all(|&byte| byte == 0));
            #[cfg(any(
                target_os = "macos",
                target_os = "ios",
                target_os = "freebsd",
                target_os = "openbsd",
                target_os = "dragonfly",
                target_os = "netbsd",
                target_os = "haiku",
                target_os = "aix"
            ))]
            assert_eq!(address.sun_len as usize, length as usize);
        }
    }

    #[test]
    fn created_unix_socket_is_close_on_exec() {
        // Construction validates the path but does not bind or create a file.
        let (socket, _, _) = new_socket(Path::new("vibeio-unbound.sock")).unwrap();
        // SAFETY: socket owns a live fd; F_GETFD only returns integer flags.
        let flags = unsafe { libc::fcntl(socket.as_raw_fd(), libc::F_GETFD) };
        assert_ne!(flags, -1);
        assert_ne!(flags & libc::FD_CLOEXEC, 0);
    }
    #[test]
    fn from_std_poll_stays_nonblocking_on_a_completion_capable_driver() {
        let mut driver = AnyDriver::new_mock();
        let AnyDriver::Mock(mock) = &mut driver else {
            unreachable!()
        };
        mock.registrations = Some(Default::default());
        mock.registrations
            .as_ref()
            .unwrap()
            .results
            .borrow_mut()
            .push_back(Ok(mio::Token(0)));
        Runtime::new(driver).block_on(async {
            let (socket, _peer) = StdUnixStream::pair().unwrap();
            let stream = UnixStream::from_std_poll(socket).unwrap();
            assert_eq!(stream.stream.handle.mode(), RegistrationMode::Poll);
            assert!(!stream.stream.handle.uses_completion());
            // SAFETY: the stream owns this live descriptor; F_GETFL takes no
            // pointer arguments and does not alter its ownership.
            let flags = unsafe { libc::fcntl(stream.stream.inner.as_raw_fd(), libc::F_GETFL) };
            assert_ne!(flags, -1);
            assert_ne!(flags & libc::O_NONBLOCK, 0);
        });
    }
}
