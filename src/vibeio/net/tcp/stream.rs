//! TCP stream types for async I/O.
#![warn(clippy::undocumented_unsafe_blocks)]
//!
//! This module provides:
//! - [`TcpStream`]: An async TCP stream that can use either completion-based or poll-based I/O.
//! - [`PollTcpStream`]: A poll-only variant that always uses readiness-based operations.
//!
//! # Implementation details
//!
//! - On Linux with io_uring support, TCP operations use native async syscalls via the async driver.
//! - When io_uring completion is available, operations complete directly.
//! - Poll mode uses nonblocking socket calls and driver readiness notifications.
//! - Register sockets and drive async I/O inside a runtime. Registration without
//!   one returns an error; direct address/option queries need no current runtime.

use std::cell::RefCell;
use std::future::poll_fn;
use std::io::{self, IoSlice};
use std::net::{Shutdown, SocketAddr, ToSocketAddrs};
#[cfg(unix)]
use std::os::fd::{AsRawFd, IntoRawFd, RawFd};
#[cfg(windows)]
use std::os::windows::io::{AsRawSocket, IntoRawSocket, RawSocket};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use mio::Interest;
use tokio::io::{AsyncRead as TokioAsyncRead, AsyncWrite as TokioAsyncWrite, ReadBuf};

#[cfg(windows)]
use windows_sys::Win32::Networking::WinSock::SOCKADDR_STORAGE;

use crate::vibeio::io::{
    AsInnerRawHandle, AsyncReadPoll, AsyncWritePoll, IoBuf, IoBufMut, IoBufTemporaryPoll,
    IoVectoredBuf, IoVectoredBufMut, IoVectoredBufTemporaryPoll,
};
use crate::vibeio::op::{
    ConnectOp, ReadOp, ReadinessOp, ReadvOp, RecvOp, WriteOp, WritevOp, socket_addr_to_raw,
};
use crate::vibeio::{
    driver::RegistrationMode,
    fd_inner::InnerRawHandle,
    io::{AsyncRead, AsyncWrite},
};

#[cfg(unix)]
fn new_socket(
    address: SocketAddr,
) -> Result<(std::net::TcpStream, libc::sockaddr_storage, libc::socklen_t), io::Error> {
    let (raw_addr, raw_addr_len) = socket_addr_to_raw(address);
    let socket = socket2::Socket::new(
        socket2::Domain::for_address(address),
        socket2::Type::STREAM,
        None,
    )?;
    Ok((socket.into(), raw_addr, raw_addr_len))
}

#[cfg(windows)]
fn new_socket(
    address: SocketAddr,
) -> Result<(std::net::TcpStream, SOCKADDR_STORAGE, i32), io::Error> {
    let (raw_addr, raw_addr_len) = socket_addr_to_raw(address);
    let socket = socket2::Socket::new(
        socket2::Domain::for_address(address),
        socket2::Type::STREAM,
        None,
    )?;
    Ok((socket.into(), raw_addr, raw_addr_len))
}

/// An async TCP stream that can use either completion-based or poll-based I/O.
///
/// This is the async version of [`std::net::TcpStream`].
///
/// # Implementation details
///
/// - On Linux with io_uring support, TCP operations use native async syscalls via the async driver.
/// - When io_uring completion is available, operations complete directly.
/// - Poll mode uses nonblocking socket calls and driver readiness notifications.
/// - Registration needs an entered runtime and returns an error without one.
///   Drive async I/O inside a runtime; direct socket queries need no current runtime.
///
/// # Examples
///
/// See "TCP loopback with the Tokio I/O adapter" in
/// `tools/vibeio-check/EXAMPLES.md` for a timeout-bounded exchange that handles
/// partial transfers and flushes buffered writes.
pub struct TcpStream {
    // Deregister before closing the socket (field declaration order).
    handle: InnerRawHandle,
    inner: Arc<std::net::TcpStream>,
}

/// A poll-only variant that always uses readiness-based operations.
///
/// This type is useful when you want to ensure readiness-based I/O is used,
/// for example when integrating with other readiness-based systems.
///
/// # Implementation details
///
/// - Always uses readiness-based I/O through the owning runtime driver.
/// - Can be converted to [`TcpStream`] with adaptive or completion mode.
pub struct PollTcpStream {
    stream: TcpStream,
    write_ready: RefCell<bool>,
    read_ready: RefCell<bool>,
}

impl TcpStream {
    /// Connects to the specified address.
    ///
    /// This is the async version of [`std::net::TcpStream::connect`].
    ///
    /// # Errors
    ///
    /// This function will return an error in the following situations:
    /// - DNS resolution fails
    /// - Connection refused
    /// - Network unreachable
    /// - The runtime is not active
    #[inline]
    pub async fn connect(address: impl ToSocketAddrs) -> Result<Self, io::Error> {
        let addresses = address.to_socket_addrs()?;
        let mut last_error = None;
        for address in addresses {
            match Self::connect_one(address).await {
                Ok(stream) => return Ok(stream),
                Err(err) => last_error = Some(err),
            }
        }
        Err(last_error
            .unwrap_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no addresses")))
    }

    #[inline]
    async fn connect_one(address: SocketAddr) -> Result<Self, io::Error> {
        let (inner, raw_addr, raw_addr_len) = new_socket(address)?;
        let stream = Self::from_std(inner)?;

        let handle = &stream.handle;
        let mut op = ConnectOp::new(handle, raw_addr, raw_addr_len)?;
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

    /// Returns the current state of the TCP_NODELAY option for this socket.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket is not connected.
    #[inline]
    pub fn nodelay(&self) -> Result<bool, io::Error> {
        self.inner.nodelay()
    }

    /// Sets the value of the TCP_NODELAY option for this socket.
    ///
    /// When set, this disables the Nagle algorithm, which means that small
    /// packets are sent immediately rather than being buffered.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket is not connected.
    #[inline]
    pub fn set_nodelay(&self, nodelay: bool) -> Result<(), io::Error> {
        self.inner.set_nodelay(nodelay)
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

    /// Peeks at data from the socket without removing it from the buffer.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket is not connected.
    #[inline]
    pub async fn peek<B: IoBufMut>(&self, buf: B) -> (Result<usize, io::Error>, B) {
        let handle = &self.handle;
        let mut op = RecvOp::new_peek(handle, buf);
        let result = poll_fn(|cx| handle.poll_op(cx, &mut op)).await;
        (result, op.take_bufs())
    }

    /// Creates a new `TcpStream` from a standard library `TcpStream`.
    ///
    /// # Errors
    ///
    /// This function will return an error if registration with the async driver fails.
    #[inline]
    pub fn from_std(inner: std::net::TcpStream) -> Result<Self, io::Error> {
        Self::from_std_with_mode(inner, RegistrationMode::Completion)
    }

    /// Creates a new `TcpStream` from a standard library `TcpStream` with a specific registration mode.
    #[inline]
    pub(crate) fn from_std_with_mode(
        inner: std::net::TcpStream,
        mode: RegistrationMode,
    ) -> Result<Self, io::Error> {
        Self::from_shared_with_mode(Arc::new(inner), mode)
    }

    /// Creates a stream registration that shares ownership of a standard TCP
    /// stream with another I/O path.
    #[inline]
    pub fn from_shared(
        inner: Arc<std::net::TcpStream>,
        mode: RegistrationMode,
    ) -> Result<Self, io::Error> {
        Self::from_shared_with_mode(inner, mode)
    }

    #[inline]
    fn from_shared_with_mode(
        inner: Arc<std::net::TcpStream>,
        mode: RegistrationMode,
    ) -> Result<Self, io::Error> {
        #[cfg(unix)]
        let handle = InnerRawHandle::new_with_mode(
            inner.as_raw_fd(),
            Interest::READABLE | Interest::WRITABLE,
            mode,
        )?;
        #[cfg(windows)]
        let handle = InnerRawHandle::new_with_mode(
            crate::vibeio::fd_inner::RawOsHandle::Socket(inner.as_raw_socket()),
            Interest::READABLE | Interest::WRITABLE,
            mode,
        )?;
        inner.set_nonblocking(!handle.uses_completion())?;
        Ok(Self { inner, handle })
    }

    /// Converts this stream into a poll-only variant.
    ///
    /// The returned `PollTcpStream` will always use readiness-based I/O.
    #[inline]
    pub fn into_poll(self) -> Result<PollTcpStream, io::Error> {
        let mut stream = self;
        stream.handle.rebind_mode(RegistrationMode::Poll)?;
        stream
            .inner
            .set_nonblocking(!stream.handle.uses_completion())?;
        Ok(PollTcpStream {
            stream,
            write_ready: RefCell::new(false),
            read_ready: RefCell::new(false),
        })
    }
}

impl PollTcpStream {
    /// Connects to the specified address using poll-based I/O.
    #[inline]
    pub async fn connect(address: impl ToSocketAddrs) -> Result<Self, io::Error> {
        let addresses = address.to_socket_addrs()?;
        let mut last_error = None;
        for address in addresses {
            match Self::connect_one(address).await {
                Ok(stream) => return Ok(stream),
                Err(err) => last_error = Some(err),
            }
        }
        Err(last_error
            .unwrap_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no addresses")))
    }

    #[inline]
    async fn connect_one(address: SocketAddr) -> Result<Self, io::Error> {
        let (inner, raw_addr, raw_addr_len) = new_socket(address)?;
        let stream = Self::from_std(inner)?;

        let handle = &stream.stream.handle;
        let mut op = ConnectOp::new(handle, raw_addr, raw_addr_len)?;
        poll_fn(move |cx| handle.poll_op(cx, &mut op)).await?;

        Ok(stream)
    }

    /// Creates a new `PollTcpStream` from a standard library `TcpStream`.
    #[inline]
    pub fn from_std(inner: std::net::TcpStream) -> Result<Self, io::Error> {
        Ok(Self {
            stream: TcpStream::from_std_with_mode(inner, RegistrationMode::Poll)?,
            write_ready: RefCell::new(false),
            read_ready: RefCell::new(false),
        })
    }

    /// Creates a poll stream that shares ownership of a standard TCP stream.
    #[inline]
    pub fn from_shared(inner: Arc<std::net::TcpStream>) -> Result<Self, io::Error> {
        Ok(Self {
            stream: TcpStream::from_shared_with_mode(inner, RegistrationMode::Poll)?,
            write_ready: RefCell::new(false),
            read_ready: RefCell::new(false),
        })
    }

    /// Converts this poll stream into an adaptive `TcpStream`.
    #[inline]
    pub fn into_adaptive(self) -> TcpStream {
        self.stream
    }

    /// Converts this poll stream into a completion-based `TcpStream`.
    #[inline]
    pub fn into_completion(self) -> Result<TcpStream, io::Error> {
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

    /// Returns the current state of the TCP_NODELAY option for this socket.
    #[inline]
    pub fn nodelay(&self) -> Result<bool, io::Error> {
        self.stream.nodelay()
    }

    /// Sets the value of the TCP_NODELAY option for this socket.
    #[inline]
    pub fn set_nodelay(&self, nodelay: bool) -> Result<(), io::Error> {
        self.stream.set_nodelay(nodelay)
    }

    /// Shuts down the connection.
    #[inline]
    pub fn shutdown(&self, how: Shutdown) -> Result<(), io::Error> {
        self.stream.shutdown(how)
    }

    /// Peeks at data from the socket without removing it from the buffer.
    ///
    /// This method uses readiness-based I/O and is compatible with `tokio::io`.
    #[inline]
    pub async fn peek(&self, buf: &mut [u8]) -> Result<usize, io::Error> {
        let handle = &self.stream.handle;
        poll_fn(move |cx| {
            // SAFETY: the caller exclusively lends initialized writable bytes
            // for this poll. The local operation only uses synchronous poll I/O
            // and is destroyed before returning, including when it is Pending.
            let buf = unsafe { IoBufTemporaryPoll::new(buf.as_mut_ptr(), buf.len()) };
            let mut op = RecvOp::new_peek(handle, buf);
            handle.poll_op_poll(cx, &mut op)
        })
        .await
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

#[cfg(unix)]
impl AsRawFd for TcpStream {
    #[inline]
    fn as_raw_fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }
}

#[cfg(unix)]
impl AsRawFd for PollTcpStream {
    #[inline]
    fn as_raw_fd(&self) -> RawFd {
        self.stream.inner.as_raw_fd()
    }
}

#[cfg(unix)]
impl IntoRawFd for TcpStream {
    #[inline]
    fn into_raw_fd(self) -> RawFd {
        let Self { handle, inner } = self;
        drop(handle);
        match Arc::try_unwrap(inner) {
            Ok(inner) => inner.into_raw_fd(),
            Err(inner) => inner
                .try_clone()
                .expect("failed to duplicate shared TCP stream")
                .into_raw_fd(),
        }
    }
}

#[cfg(unix)]
impl IntoRawFd for PollTcpStream {
    #[inline]
    fn into_raw_fd(self) -> RawFd {
        self.stream.into_raw_fd()
    }
}

#[cfg(windows)]
impl AsRawSocket for TcpStream {
    #[inline]
    fn as_raw_socket(&self) -> RawSocket {
        self.inner.as_raw_socket()
    }
}

#[cfg(windows)]
impl IntoRawSocket for TcpStream {
    #[inline]
    fn into_raw_socket(self) -> RawSocket {
        let Self { handle, inner } = self;
        drop(handle);
        match Arc::try_unwrap(inner) {
            Ok(inner) => inner.into_raw_socket(),
            Err(inner) => inner
                .try_clone()
                .expect("failed to duplicate shared TCP stream")
                .into_raw_socket(),
        }
    }
}

#[cfg(windows)]
impl AsRawSocket for PollTcpStream {
    #[inline]
    fn as_raw_socket(&self) -> RawSocket {
        self.stream.as_raw_socket()
    }
}

#[cfg(windows)]
impl IntoRawSocket for PollTcpStream {
    #[inline]
    fn into_raw_socket(self) -> RawSocket {
        self.stream.into_raw_socket()
    }
}

impl<'a> AsInnerRawHandle<'a> for TcpStream {
    #[inline]
    fn as_inner_raw_handle(&'a self) -> &'a InnerRawHandle {
        &self.handle
    }
}

impl<'a> AsInnerRawHandle<'a> for PollTcpStream {
    #[inline]
    fn as_inner_raw_handle(&'a self) -> &'a InnerRawHandle {
        self.stream.as_inner_raw_handle()
    }
}

impl AsyncRead for TcpStream {
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

impl TokioAsyncRead for PollTcpStream {
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

impl AsyncWrite for TcpStream {
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

impl TokioAsyncWrite for PollTcpStream {
    #[inline]
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        let this = self.get_mut();
        // SAFETY: the source remains initialized and borrowed for this call.
        // WriteOp only reads it; poll_op_poll rejects completion submission and
        // the local operation cannot retain the pointer after returning Pending.
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
        // SAFETY: these initialized IoSlice regions remain borrowed throughout
        // the synchronous WritevOp poll. Metadata is copied, and the local op is
        // dropped before this call returns; no completion I/O can retain it.
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

impl AsyncReadPoll for PollTcpStream {
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

impl AsyncWritePoll for PollTcpStream {
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

#[cfg(test)]
mod socket_creation_tests {
    use super::*;

    #[test]
    fn cancelled_poll_peek_releases_buffer_without_consuming_data() {
        use crate::vibeio::{Runtime, driver::AnyDriver};
        use std::future::Future;
        use std::io::Write;
        #[cfg(unix)]
        let driver = AnyDriver::new_mio().unwrap();
        #[cfg(windows)]
        let driver = AnyDriver::new_iocp().unwrap();
        Runtime::new(driver).block_on(async {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let socket = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            let (mut peer, _) = listener.accept().unwrap();
            let mut stream = PollTcpStream::from_std(socket).unwrap();
            let mut buffer = [b'_'; 8];
            let mut pending = Box::pin(stream.peek(&mut buffer));
            assert!(
                pending
                    .as_mut()
                    .poll(&mut Context::from_waker(std::task::Waker::noop()))
                    .is_pending()
            );
            drop(pending);
            assert_eq!(buffer, [b'_'; 8]);
            buffer.fill(b'x');
            peer.write_all(b"peek").unwrap();
            crate::vibeio::time::timeout(std::time::Duration::from_secs(5), async {
                let count = stream.peek(&mut buffer).await.unwrap();
                assert!(count > 0 && count <= 4);
                assert_eq!(&buffer[..count], &b"peek"[..count]);
                assert!(buffer[count..].iter().all(|byte| *byte == b'x'));
                let mut received = [0; 4];
                tokio::io::AsyncReadExt::read_exact(&mut stream, &mut received)
                    .await
                    .unwrap();
                assert_eq!(&received, b"peek");
            })
            .await
            .unwrap();
        });
    }

    #[test]
    fn created_socket_is_close_on_exec() {
        let socket = new_socket("127.0.0.1:0".parse().unwrap()).unwrap().0;
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
