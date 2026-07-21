//! TCP stream types for async I/O.
//!
//! This module provides:
//! - [`TcpStream`]: An async TCP stream that can use either completion-based or poll-based I/O.
//! - [`PollTcpStream`]: A poll-only variant that always uses readiness-based operations.
//!
//! # Implementation details
//!
//! - On Linux with io_uring support, TCP operations use native async syscalls via the async driver.
//! - When io_uring completion is available, operations complete directly.
//! - For platforms without native async support, operations fall back to synchronous std::net calls.
//! - The runtime must be active when calling these types' methods; otherwise they will panic.

use std::cell::RefCell;
use std::future::poll_fn;
use std::io::{self, IoSlice};
use std::mem::{ManuallyDrop, MaybeUninit};
use std::net::{Shutdown, SocketAddr, ToSocketAddrs};
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
#[cfg(windows)]
use std::os::windows::io::{AsRawSocket, FromRawSocket, IntoRawSocket, RawSocket};
use std::pin::Pin;
use std::task::{Context, Poll};

use mio::Interest;
use tokio::io::{AsyncRead as TokioAsyncRead, AsyncWrite as TokioAsyncWrite, ReadBuf};

#[cfg(windows)]
use windows_sys::Win32::Networking::WinSock::{
    self, AF_INET, AF_INET6, SOCKADDR, SOCKADDR_IN, SOCKADDR_IN6, SOCKADDR_STORAGE, SOCK_STREAM,
    WSADATA,
};

use crate::io::{
    AsInnerRawHandle, AsyncReadPoll, AsyncWritePoll, IoBuf, IoBufMut, IoBufTemporaryPoll,
    IoVectoredBuf, IoVectoredBufMut, IoVectoredBufTemporaryPoll,
};
use crate::op::{ConnectOp, ReadOp, ReadinessOp, ReadvOp, RecvOp, WriteOp, WritevOp};
use crate::{
    driver::RegistrationMode,
    fd_inner::InnerRawHandle,
    io::{AsyncRead, AsyncWrite},
};

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

            let mut storage = MaybeUninit::<libc::sockaddr_storage>::zeroed();
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

            let mut storage = MaybeUninit::<libc::sockaddr_storage>::zeroed();
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
                AF_INET as _,
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
            sockaddr.Anonymous.sin6_scope_id = address.scope_id() as u32;

            let mut storage = SOCKADDR_STORAGE::default();
            unsafe {
                std::ptr::copy_nonoverlapping(
                    &sockaddr as *const SOCKADDR_IN6 as *const u8,
                    &mut storage as *mut SOCKADDR_STORAGE as *mut u8,
                    std::mem::size_of::<SOCKADDR_IN6>(),
                );
            }
            (
                AF_INET6 as _,
                storage,
                std::mem::size_of::<SOCKADDR_IN6>() as i32,
            )
        }
    }
}

#[cfg(unix)]
fn new_socket(
    address: SocketAddr,
) -> Result<(std::net::TcpStream, libc::sockaddr_storage, libc::socklen_t), io::Error> {
    let (domain, raw_addr, raw_addr_len) = socket_addr_to_raw(address);
    let socket_fd = unsafe { libc::socket(domain, libc::SOCK_STREAM, 0) };
    if socket_fd == -1 {
        return Err(io::Error::last_os_error());
    }
    let stream = unsafe { std::net::TcpStream::from_raw_fd(socket_fd.into_raw_fd()) };
    Ok((stream, raw_addr, raw_addr_len))
}

#[cfg(windows)]
fn new_socket(
    address: SocketAddr,
) -> Result<(std::net::TcpStream, SOCKADDR_STORAGE, i32), io::Error> {
    // 0x202 = MAKEWORD(2, 2)
    let mut wsadata = WSADATA::default();
    if unsafe { WinSock::WSAStartup(0x202, &mut wsadata as *mut WSADATA) } != 0 {
        return Err(io::Error::last_os_error());
    }
    let (domain, raw_addr, raw_addr_len) = socket_addr_to_raw(address);
    let socket = unsafe { WinSock::socket(domain, SOCK_STREAM, 0) };
    if socket == WinSock::INVALID_SOCKET {
        let err = io::Error::last_os_error();
        let _ = unsafe { WinSock::WSACleanup() };
        return Err(err);
    }
    let stream = unsafe { std::net::TcpStream::from_raw_socket(socket as _) };
    Ok((stream, raw_addr, raw_addr_len))
}

/// An async TCP stream that can use either completion-based or poll-based I/O.
///
/// This is the async version of [`std::net::TcpStream`].
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
/// use vibeio::net::TcpStream;
///
/// let mut stream = TcpStream::connect("127.0.0.1:8080").await?;
/// stream.write(b"hello").await.0?;
/// let mut buf = [0u8; 1024];
/// let (read, buf) = stream.read(buf).await;
/// let read = read?;
/// ```
pub struct TcpStream {
    inner: std::net::TcpStream,
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

        #[cfg(unix)]
        let raw_addr_ptr = (&raw_addr as *const libc::sockaddr_storage).cast::<libc::sockaddr>();
        #[cfg(windows)]
        let raw_addr_ptr = (&raw_addr as *const SOCKADDR_STORAGE).cast::<SOCKADDR>();
        let handle = &stream.handle;
        let mut op = ConnectOp::new(handle, raw_addr_ptr, raw_addr_len);
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
        #[cfg(unix)]
        let handle = ManuallyDrop::new(InnerRawHandle::new_with_mode(
            inner.as_raw_fd(),
            Interest::READABLE | Interest::WRITABLE,
            mode,
        )?);
        #[cfg(windows)]
        let handle = ManuallyDrop::new(InnerRawHandle::new_with_mode(
            crate::fd_inner::RawOsHandle::Socket(inner.as_raw_socket()),
            Interest::READABLE | Interest::WRITABLE,
            mode,
        )?);
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

        #[cfg(unix)]
        let raw_addr_ptr = (&raw_addr as *const libc::sockaddr_storage).cast::<libc::sockaddr>();
        #[cfg(windows)]
        let raw_addr_ptr = (&raw_addr as *const SOCKADDR_STORAGE).cast::<SOCKADDR>();

        let handle = &stream.stream.handle;
        let mut op = ConnectOp::new(handle, raw_addr_ptr, raw_addr_len);
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
        let buf = unsafe { IoBufTemporaryPoll::new(buf.as_mut_ptr(), buf.len()) };
        let mut op = RecvOp::new_peek(handle, buf);
        poll_fn(move |cx| handle.poll_op_poll(cx, &mut op)).await
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
        let mut this = ManuallyDrop::new(self);

        // Safety: `this` will not be dropped, so we must drop the registration handle manually.
        // We then move out the inner std stream and transfer its fd ownership to the caller.
        unsafe {
            ManuallyDrop::drop(&mut this.handle);
            std::ptr::read(&this.inner).into_raw_fd()
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
        let mut this = ManuallyDrop::new(self);

        // Safety: `this` will not be dropped, so we must drop the registration handle manually.
        // We then move out the inner std stream and transfer its socket ownership to the caller.
        unsafe {
            ManuallyDrop::drop(&mut this.handle);
            std::ptr::read(&this.inner).into_raw_socket()
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
        // Equivalent to .assume_init_mut() in Rust 1.93.0+
        let unfilled = unsafe { &mut *(buf.unfilled_mut() as *mut [MaybeUninit<u8>] as *mut [u8]) };
        let buf_temp = unsafe { IoBufTemporaryPoll::new(unfilled.as_mut_ptr(), unfilled.len()) };
        let mut op = ReadOp::new(&this.stream.handle, buf_temp);
        match this.stream.handle.poll_op_poll(cx, &mut op) {
            Poll::Ready(Ok(read)) => {
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

impl Drop for TcpStream {
    #[inline]
    fn drop(&mut self) {
        // Safety: The struct is dropped after the handle is dropped.
        unsafe {
            ManuallyDrop::drop(&mut self.handle);
        }
    }
}
