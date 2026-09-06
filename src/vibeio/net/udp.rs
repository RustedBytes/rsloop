//! UDP socket types for async I/O.
//!
//! This module provides:
//! - [`UdpSocket`]: An async UDP socket that can use either completion-based or poll-based I/O.
//!
//! # Implementation details
//!
//! - On Linux with io_uring support, UDP operations use native async syscalls via the async driver.
//! - When io_uring completion is available, operations complete directly.
//! - For platforms without native async support, operations fall back to synchronous std::net calls.
//! - The runtime must be active when calling these types' methods; otherwise they will panic.

use std::cell::RefCell;
use std::future::poll_fn;
use std::io;
use std::mem::ManuallyDrop;
use std::net::{Ipv4Addr, Ipv6Addr, SocketAddr, ToSocketAddrs, UdpSocket as StdUdpSocket};
#[cfg(unix)]
use std::os::fd::{AsRawFd, IntoRawFd, RawFd};
#[cfg(windows)]
use std::os::windows::io::{AsRawSocket, IntoRawSocket, RawSocket};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use mio::Interest;

use crate::vibeio::driver::RegistrationMode;
use crate::vibeio::fd_inner::InnerRawHandle;
use crate::vibeio::io::{
    AsInnerRawHandle, AsyncReadPoll, AsyncWritePoll, IoBuf, IoBufMut, IoBufTemporaryPoll,
};
#[cfg(unix)]
use crate::vibeio::op::{ConnectOp, socket_addr_to_raw};
use crate::vibeio::op::{ReadinessOp, RecvOp, RecvfromOp, SendOp, SendtoOp};

#[cfg(unix)]
#[inline]
async fn connect_one(handle: &InnerRawHandle, address: SocketAddr) -> Result<(), io::Error> {
    let (raw_addr, raw_addr_len) = socket_addr_to_raw(address);

    let mut op = ConnectOp::new(handle, raw_addr, raw_addr_len)?;
    poll_fn(move |cx| handle.poll_op(cx, &mut op)).await
}

/// An async UDP socket that can use either completion-based or poll-based I/O.
///
/// This is the async version of [`std::net::UdpSocket`].
///
/// # Implementation details
///
/// - On Linux with io_uring support, UDP operations use native async syscalls via the async driver.
/// - When io_uring completion is available, operations complete directly.
/// - For platforms without native async support, operations fall back to synchronous std::net calls.
/// - The runtime must be active when calling these methods; otherwise they will panic.
///
/// # Examples
///
/// ```ignore
/// use vibeio::net::UdpSocket;
///
/// let socket = UdpSocket::bind("127.0.0.1:0").await?;
/// socket.connect("127.0.0.1:9000").await?;
/// socket.send(b"hello").await?;
/// ```
pub struct UdpSocket {
    inner: StdUdpSocket,
    handle: ManuallyDrop<InnerRawHandle>,
}

impl UdpSocket {
    /// Creates a new `UdpSocket` which will be bound to the specified address.
    ///
    /// This is the async version of [`std::net::UdpSocket::bind`].
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
        let inner = StdUdpSocket::bind(address)?;
        Self::from_std(inner)
    }

    /// Creates a new `UdpSocket` from a standard library `UdpSocket`.
    ///
    /// # Errors
    ///
    /// This function will return an error if registration with the async driver fails.
    #[inline]
    pub fn from_std(inner: StdUdpSocket) -> Result<Self, io::Error> {
        Self::from_std_with_mode(inner, RegistrationMode::Completion)
    }

    /// Creates a new `UdpSocket` from a standard library `UdpSocket` with a specific registration mode.
    #[inline]
    pub(crate) fn from_std_with_mode(
        inner: StdUdpSocket,
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
        let handle = ManuallyDrop::new(handle);
        Ok(Self { inner, handle })
    }

    /// Converts this socket into a poll-only variant.
    ///
    /// The returned `PollUdpSocket` will always use readiness-based I/O.
    #[inline]
    pub fn into_poll(self) -> Result<PollUdpSocket, io::Error> {
        let mut socket = self;
        socket.handle.rebind_mode(RegistrationMode::Poll)?;
        socket
            .inner
            .set_nonblocking(!socket.handle.uses_completion())?;
        Ok(PollUdpSocket {
            socket,
            read_ready: RefCell::new(false),
            write_ready: RefCell::new(false),
        })
    }

    /// Converts this `UdpSocket` into the standard library `UdpSocket`.
    #[inline]
    pub fn into_std(self) -> StdUdpSocket {
        let mut this = ManuallyDrop::new(self);

        // Safety: `this` will not be dropped, so we must drop the registration
        // handle manually and move out the inner socket.
        unsafe {
            ManuallyDrop::drop(&mut this.handle);
            std::ptr::read(&this.inner)
        }
    }

    /// Returns the local address of this socket.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket is not bound.
    #[inline]
    pub fn local_addr(&self) -> Result<SocketAddr, io::Error> {
        self.inner.local_addr()
    }

    /// Returns the remote address of this socket.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket is not connected.
    #[inline]
    pub fn peer_addr(&self) -> Result<SocketAddr, io::Error> {
        self.inner.peer_addr()
    }

    /// Connects this UDP socket to a remote address.
    ///
    /// This is the async version of [`std::net::UdpSocket::connect`].
    ///
    /// # Errors
    ///
    /// This function will return an error in the following situations:
    /// - DNS resolution fails
    /// - Connection fails
    /// - The runtime is not active
    #[inline]
    pub async fn connect(&mut self, address: impl ToSocketAddrs) -> Result<(), io::Error> {
        let addresses = address.to_socket_addrs()?;
        let mut last_error = None;
        for address in addresses {
            #[cfg(unix)]
            let connect_one_result = connect_one(&self.handle, address).await;
            #[cfg(windows)]
            // Winsock datagram connect only sets the default peer; it does not
            // perform a connection handshake. Keep both registration and socket
            // mode unchanged, including on error. ConnectEx is stream-only.
            // https://learn.microsoft.com/en-us/windows/win32/api/winsock2/nf-winsock2-connect
            let connect_one_result = self.inner.connect(address);
            match connect_one_result {
                Ok(()) => return Ok(()),
                Err(err) => last_error = Some(err),
            }
        }
        Err(last_error
            .unwrap_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no addresses")))
    }

    /// Receives a single datagram message.
    ///
    /// This is the async version of [`std::net::UdpSocket::recv`].
    ///
    /// # Errors
    ///
    /// This function will return an error in the following situations:
    /// - The socket is not bound
    /// - The runtime is not active
    #[inline]
    pub async fn recv<B: IoBufMut>(&self, buf: B) -> (Result<usize, io::Error>, B) {
        let handle = &self.handle;
        let mut op = RecvOp::new(handle, buf);
        let result = poll_fn(|cx| handle.poll_op(cx, &mut op)).await;
        (result, op.take_bufs())
    }

    /// Receives a single datagram message, returning the sender's address.
    ///
    /// This is the async version of [`std::net::UdpSocket::recv_from`].
    ///
    /// # Errors
    ///
    /// This function will return an error in the following situations:
    /// - The socket is not bound
    /// - The runtime is not active
    #[inline]
    pub async fn recv_from<B: IoBufMut>(
        &self,
        buf: B,
    ) -> (Result<(usize, SocketAddr), io::Error>, B) {
        let handle = &self.handle;
        let mut op = RecvfromOp::new(handle, buf);
        let result = poll_fn(|cx| handle.poll_op(cx, &mut op)).await;
        (result, op.take_bufs())
    }

    /// Sends data on a connected socket.
    ///
    /// This is the async version of [`std::net::UdpSocket::send`].
    ///
    /// # Errors
    ///
    /// This function will return an error in the following situations:
    /// - The socket is not connected
    /// - The runtime is not active
    #[inline]
    pub async fn send<B: IoBuf>(&self, buf: B) -> (Result<usize, io::Error>, B) {
        let handle = &self.handle;
        let mut op = SendOp::new(handle, buf);
        let result = poll_fn(|cx| handle.poll_op(cx, &mut op)).await;
        (result, op.take_bufs())
    }

    /// Sends data to the specified address.
    ///
    /// This is the async version of [`std::net::UdpSocket::send_to`].
    ///
    /// # Errors
    ///
    /// This function will return an error in the following situations:
    /// - DNS resolution fails
    /// - The send operation fails
    /// - The runtime is not active
    #[inline]
    pub async fn send_to<B: IoBuf>(
        &self,
        mut buf: B,
        address: impl ToSocketAddrs,
    ) -> (Result<usize, io::Error>, B) {
        let addresses = match address.to_socket_addrs() {
            Ok(addresses) => addresses,
            Err(err) => return (Err(err), buf),
        };
        let mut last_error = None;

        for address in addresses {
            let handle = &self.handle;
            let mut op = SendtoOp::new(handle, buf, address);
            match poll_fn(|cx| handle.poll_op(cx, &mut op)).await {
                Ok(sent) => return (Ok(sent), op.take_bufs()),
                Err(err) => {
                    buf = op.take_bufs();
                    last_error = Some(err);
                }
            }
        }

        (
            Err(last_error
                .unwrap_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "no addresses"))),
            buf,
        )
    }

    /// Receives data without removing it from the socket's receive queue.
    ///
    /// This is the async version of [`std::net::UdpSocket::peek`].
    ///
    /// # Errors
    ///
    /// This function will return an error in the following situations:
    /// - The socket is not bound
    /// - The runtime is not active
    #[inline]
    pub async fn peek<B: IoBufMut>(&self, buf: B) -> (Result<usize, io::Error>, B) {
        let handle = &self.handle;
        let mut op = RecvOp::new_peek(handle, buf);
        let result = poll_fn(|cx| handle.poll_op(cx, &mut op)).await;
        (result, op.take_bufs())
    }

    /// Receives data without removing it from the socket's receive queue,
    /// returning the sender's address.
    ///
    /// This is the async version of [`std::net::UdpSocket::peek_from`].
    ///
    /// # Errors
    ///
    /// This function will return an error in the following situations:
    /// - The socket is not bound
    /// - The runtime is not active
    #[inline]
    pub async fn peek_from<B: IoBufMut>(
        &self,
        buf: B,
    ) -> (Result<(usize, SocketAddr), io::Error>, B) {
        let handle = &self.handle;
        let mut op = RecvfromOp::new_peek(handle, buf);
        let result = poll_fn(|cx| handle.poll_op(cx, &mut op)).await;
        (result, op.take_bufs())
    }

    /// Returns a new `UdpSocket` that shares the same underlying file descriptor.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be cloned.
    #[inline]
    pub fn try_clone(&self) -> Result<Self, io::Error> {
        Self::from_std(self.inner.try_clone()?)
    }

    /// Sets the broadcast flag.
    ///
    /// When set, the socket can send broadcast packets.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be modified.
    #[inline]
    pub fn set_broadcast(&self, broadcast: bool) -> Result<(), io::Error> {
        self.inner.set_broadcast(broadcast)
    }

    /// Returns the current value of the broadcast flag.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be queried.
    #[inline]
    pub fn broadcast(&self) -> Result<bool, io::Error> {
        self.inner.broadcast()
    }

    /// Sets the time-to-live (TTL) value.
    ///
    /// This controls how many hops a packet can traverse before being discarded.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be modified.
    #[inline]
    pub fn set_ttl(&self, ttl: u32) -> Result<(), io::Error> {
        self.inner.set_ttl(ttl)
    }

    /// Returns the current TTL value.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be queried.
    #[inline]
    pub fn ttl(&self) -> Result<u32, io::Error> {
        self.inner.ttl()
    }

    /// Sets the multicast loop flag for IPv4.
    ///
    /// When set, multicast packets are looped back to the local socket.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be modified.
    #[inline]
    pub fn set_multicast_loop_v4(&self, multicast_loop_v4: bool) -> Result<(), io::Error> {
        self.inner.set_multicast_loop_v4(multicast_loop_v4)
    }

    /// Returns the current IPv4 multicast loop flag.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be queried.
    #[inline]
    pub fn multicast_loop_v4(&self) -> Result<bool, io::Error> {
        self.inner.multicast_loop_v4()
    }

    /// Sets the multicast TTL for IPv4.
    ///
    /// This controls how many hops multicast packets can traverse.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be modified.
    #[inline]
    pub fn set_multicast_ttl_v4(&self, multicast_ttl_v4: u32) -> Result<(), io::Error> {
        self.inner.set_multicast_ttl_v4(multicast_ttl_v4)
    }

    /// Returns the current IPv4 multicast TTL.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be queried.
    #[inline]
    pub fn multicast_ttl_v4(&self) -> Result<u32, io::Error> {
        self.inner.multicast_ttl_v4()
    }

    /// Sets the multicast loop flag for IPv6.
    ///
    /// When set, multicast packets are looped back to the local socket.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be modified.
    #[inline]
    pub fn set_multicast_loop_v6(&self, multicast_loop_v6: bool) -> Result<(), io::Error> {
        self.inner.set_multicast_loop_v6(multicast_loop_v6)
    }

    /// Returns the current IPv6 multicast loop flag.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be queried.
    #[inline]
    pub fn multicast_loop_v6(&self) -> Result<bool, io::Error> {
        self.inner.multicast_loop_v6()
    }

    /// Joins a multicast group for IPv4.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be modified.
    #[inline]
    pub fn join_multicast_v4(
        &self,
        multiaddr: &Ipv4Addr,
        interface: &Ipv4Addr,
    ) -> Result<(), io::Error> {
        self.inner.join_multicast_v4(multiaddr, interface)
    }

    /// Joins a multicast group for IPv6.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be modified.
    #[inline]
    pub fn join_multicast_v6(&self, multiaddr: &Ipv6Addr, interface: u32) -> Result<(), io::Error> {
        self.inner.join_multicast_v6(multiaddr, interface)
    }

    /// Leaves a multicast group for IPv4.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be modified.
    #[inline]
    pub fn leave_multicast_v4(
        &self,
        multiaddr: &Ipv4Addr,
        interface: &Ipv4Addr,
    ) -> Result<(), io::Error> {
        self.inner.leave_multicast_v4(multiaddr, interface)
    }

    /// Leaves a multicast group for IPv6.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be modified.
    #[inline]
    pub fn leave_multicast_v6(
        &self,
        multiaddr: &Ipv6Addr,
        interface: u32,
    ) -> Result<(), io::Error> {
        self.inner.leave_multicast_v6(multiaddr, interface)
    }

    /// Takes the pending error from the socket.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be queried.
    #[inline]
    pub fn take_error(&self) -> Result<Option<io::Error>, io::Error> {
        self.inner.take_error()
    }

    /// Sets the read timeout for the socket.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be modified.
    #[inline]
    pub fn set_read_timeout(&self, dur: Option<Duration>) -> Result<(), io::Error> {
        self.inner.set_read_timeout(dur)
    }

    /// Sets the write timeout for the socket.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be modified.
    #[inline]
    pub fn set_write_timeout(&self, dur: Option<Duration>) -> Result<(), io::Error> {
        self.inner.set_write_timeout(dur)
    }

    /// Returns the read timeout for the socket.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be queried.
    #[inline]
    pub fn read_timeout(&self) -> Result<Option<Duration>, io::Error> {
        self.inner.read_timeout()
    }

    /// Returns the write timeout for the socket.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be queried.
    #[inline]
    pub fn write_timeout(&self) -> Result<Option<Duration>, io::Error> {
        self.inner.write_timeout()
    }
}

#[cfg(unix)]
impl AsRawFd for UdpSocket {
    #[inline]
    fn as_raw_fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }
}

#[cfg(unix)]
impl IntoRawFd for UdpSocket {
    #[inline]
    fn into_raw_fd(self) -> RawFd {
        self.into_std().into_raw_fd()
    }
}

#[cfg(windows)]
impl AsRawSocket for UdpSocket {
    #[inline]
    fn as_raw_socket(&self) -> RawSocket {
        self.inner.as_raw_socket()
    }
}

#[cfg(windows)]
impl IntoRawSocket for UdpSocket {
    #[inline]
    fn into_raw_socket(self) -> RawSocket {
        self.into_std().into_raw_socket()
    }
}

impl Drop for UdpSocket {
    #[inline]
    fn drop(&mut self) {
        // Safety: The struct is dropped after the handle is dropped.
        unsafe {
            ManuallyDrop::drop(&mut self.handle);
        }
    }
}

impl<'a> AsInnerRawHandle<'a> for UdpSocket {
    #[inline]
    fn as_inner_raw_handle(&'a self) -> &'a InnerRawHandle {
        &self.handle
    }
}

/// A poll-based UDP socket that always uses readiness-based I/O.
///
/// This is the poll-only counterpart to [`UdpSocket`], similar to how
/// [`PollTcpStream`](crate::vibeio::net::PollTcpStream) relates to
/// [`TcpStream`](crate::vibeio::net::TcpStream).
///
/// All I/O operations on this type use readiness-based (poll) I/O,
/// regardless of whether the runtime supports completion-based I/O.
///
/// # Examples
///
/// ```ignore
/// use vibeio::net::UdpSocket;
///
/// let socket = UdpSocket::bind("127.0.0.1:0")?;
/// let poll_socket = socket.into_poll()?;
/// ```
pub struct PollUdpSocket {
    socket: UdpSocket,
    read_ready: RefCell<bool>,
    write_ready: RefCell<bool>,
}

impl PollUdpSocket {
    /// Creates a new `PollUdpSocket` bound to the specified address.
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
        let inner = StdUdpSocket::bind(address)?;
        Self::from_std(inner)
    }

    /// Creates a new `PollUdpSocket` from a standard library `UdpSocket`.
    ///
    /// # Errors
    ///
    /// This function will return an error if registration with the async driver fails.
    #[inline]
    pub fn from_std(inner: StdUdpSocket) -> Result<Self, io::Error> {
        Ok(Self {
            socket: UdpSocket::from_std_with_mode(inner, RegistrationMode::Poll)?,
            read_ready: RefCell::new(false),
            write_ready: RefCell::new(false),
        })
    }

    /// Converts this poll socket into an adaptive `UdpSocket`.
    #[inline]
    pub fn into_adaptive(self) -> UdpSocket {
        self.socket
    }

    /// Converts this poll socket into a completion-based `UdpSocket`.
    ///
    /// # Errors
    ///
    /// This function will return an error if the runtime does not support completion-based I/O.
    #[inline]
    pub fn into_completion(self) -> Result<UdpSocket, io::Error> {
        let mut socket = self.socket;
        socket.handle.rebind_mode(RegistrationMode::Completion)?;
        socket
            .inner
            .set_nonblocking(!socket.handle.uses_completion())?;
        Ok(socket)
    }

    /// Connects this UDP socket to a remote address.
    ///
    /// # Errors
    ///
    /// This function will return an error in the following situations:
    /// - DNS resolution fails
    /// - Connection fails
    /// - The runtime is not active
    #[inline]
    pub async fn connect(&mut self, address: impl ToSocketAddrs) -> Result<(), io::Error> {
        self.socket.connect(address).await
    }

    /// Returns the local address of this socket.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket is not bound.
    #[inline]
    pub fn local_addr(&self) -> Result<SocketAddr, io::Error> {
        self.socket.local_addr()
    }

    /// Returns the remote address of this socket.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket is not connected.
    #[inline]
    pub fn peer_addr(&self) -> Result<SocketAddr, io::Error> {
        self.socket.peer_addr()
    }

    /// Receives a single datagram message.
    ///
    /// This is the poll-based version of [`UdpSocket::recv`].
    #[inline]
    pub async fn recv<B: IoBufMut>(&self, buf: B) -> (Result<usize, io::Error>, B) {
        self.socket.recv(buf).await
    }

    /// Receives a single datagram message, returning the sender's address.
    ///
    /// This is the poll-based version of [`UdpSocket::recv_from`].
    #[inline]
    pub async fn recv_from<B: IoBufMut>(
        &self,
        buf: B,
    ) -> (Result<(usize, SocketAddr), io::Error>, B) {
        self.socket.recv_from(buf).await
    }

    /// Sends data on a connected socket.
    ///
    /// This is the poll-based version of [`UdpSocket::send`].
    #[inline]
    pub async fn send<B: IoBuf>(&self, buf: B) -> (Result<usize, io::Error>, B) {
        self.socket.send(buf).await
    }

    /// Sends data to the specified address.
    ///
    /// This is the poll-based version of [`UdpSocket::send_to`].
    #[inline]
    pub async fn send_to<B: IoBuf>(
        &self,
        buf: B,
        address: impl ToSocketAddrs,
    ) -> (Result<usize, io::Error>, B) {
        self.socket.send_to(buf, address).await
    }

    /// Receives data without removing it from the socket's receive queue.
    ///
    /// This is the poll-based version of [`UdpSocket::peek`].
    #[inline]
    pub async fn peek<B: IoBufMut>(&self, buf: B) -> (Result<usize, io::Error>, B) {
        self.socket.peek(buf).await
    }

    /// Receives data without removing it from the socket's receive queue,
    /// returning the sender's address.
    ///
    /// This is the poll-based version of [`UdpSocket::peek_from`].
    #[inline]
    pub async fn peek_from<B: IoBufMut>(
        &self,
        buf: B,
    ) -> (Result<(usize, SocketAddr), io::Error>, B) {
        self.socket.peek_from(buf).await
    }

    /// Returns a new `PollUdpSocket` that shares the same underlying file descriptor.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be cloned.
    #[inline]
    pub fn try_clone(&self) -> Result<Self, io::Error> {
        Ok(Self {
            socket: self.socket.try_clone()?,
            read_ready: RefCell::new(false),
            write_ready: RefCell::new(false),
        })
    }

    /// Sets the broadcast flag.
    ///
    /// When set, the socket can send broadcast packets.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be modified.
    #[inline]
    pub fn set_broadcast(&self, broadcast: bool) -> Result<(), io::Error> {
        self.socket.set_broadcast(broadcast)
    }

    /// Returns the current value of the broadcast flag.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be queried.
    #[inline]
    pub fn broadcast(&self) -> Result<bool, io::Error> {
        self.socket.broadcast()
    }

    /// Sets the time-to-live (TTL) value.
    ///
    /// This controls how many hops a packet can traverse before being discarded.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be modified.
    #[inline]
    pub fn set_ttl(&self, ttl: u32) -> Result<(), io::Error> {
        self.socket.set_ttl(ttl)
    }

    /// Returns the current TTL value.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be queried.
    #[inline]
    pub fn ttl(&self) -> Result<u32, io::Error> {
        self.socket.ttl()
    }

    /// Sets the multicast loop flag for IPv4.
    ///
    /// When set, multicast packets are looped back to the local socket.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be modified.
    #[inline]
    pub fn set_multicast_loop_v4(&self, multicast_loop_v4: bool) -> Result<(), io::Error> {
        self.socket.set_multicast_loop_v4(multicast_loop_v4)
    }

    /// Returns the current IPv4 multicast loop flag.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be queried.
    #[inline]
    pub fn multicast_loop_v4(&self) -> Result<bool, io::Error> {
        self.socket.multicast_loop_v4()
    }

    /// Sets the multicast TTL for IPv4.
    ///
    /// This controls how many hops multicast packets can traverse.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be modified.
    #[inline]
    pub fn set_multicast_ttl_v4(&self, multicast_ttl_v4: u32) -> Result<(), io::Error> {
        self.socket.set_multicast_ttl_v4(multicast_ttl_v4)
    }

    /// Returns the current IPv4 multicast TTL.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be queried.
    #[inline]
    pub fn multicast_ttl_v4(&self) -> Result<u32, io::Error> {
        self.socket.multicast_ttl_v4()
    }

    /// Sets the multicast loop flag for IPv6.
    ///
    /// When set, multicast packets are looped back to the local socket.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be modified.
    #[inline]
    pub fn set_multicast_loop_v6(&self, multicast_loop_v6: bool) -> Result<(), io::Error> {
        self.socket.set_multicast_loop_v6(multicast_loop_v6)
    }

    /// Returns the current IPv6 multicast loop flag.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be queried.
    #[inline]
    pub fn multicast_loop_v6(&self) -> Result<bool, io::Error> {
        self.socket.multicast_loop_v6()
    }

    /// Joins a multicast group for IPv4.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be modified.
    #[inline]
    pub fn join_multicast_v4(
        &self,
        multiaddr: &Ipv4Addr,
        interface: &Ipv4Addr,
    ) -> Result<(), io::Error> {
        self.socket.join_multicast_v4(multiaddr, interface)
    }

    /// Joins a multicast group for IPv6.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be modified.
    #[inline]
    pub fn join_multicast_v6(&self, multiaddr: &Ipv6Addr, interface: u32) -> Result<(), io::Error> {
        self.socket.join_multicast_v6(multiaddr, interface)
    }

    /// Leaves a multicast group for IPv4.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be modified.
    #[inline]
    pub fn leave_multicast_v4(
        &self,
        multiaddr: &Ipv4Addr,
        interface: &Ipv4Addr,
    ) -> Result<(), io::Error> {
        self.socket.leave_multicast_v4(multiaddr, interface)
    }

    /// Leaves a multicast group for IPv6.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be modified.
    #[inline]
    pub fn leave_multicast_v6(
        &self,
        multiaddr: &Ipv6Addr,
        interface: u32,
    ) -> Result<(), io::Error> {
        self.socket.leave_multicast_v6(multiaddr, interface)
    }

    /// Takes the pending error from the socket.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be queried.
    #[inline]
    pub fn take_error(&self) -> Result<Option<io::Error>, io::Error> {
        self.socket.take_error()
    }

    /// Sets the read timeout for the socket.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be modified.
    #[inline]
    pub fn set_read_timeout(&self, dur: Option<Duration>) -> Result<(), io::Error> {
        self.socket.set_read_timeout(dur)
    }

    /// Sets the write timeout for the socket.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be modified.
    #[inline]
    pub fn set_write_timeout(&self, dur: Option<Duration>) -> Result<(), io::Error> {
        self.socket.set_write_timeout(dur)
    }

    /// Returns the read timeout for the socket.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be queried.
    #[inline]
    pub fn read_timeout(&self) -> Result<Option<Duration>, io::Error> {
        self.socket.read_timeout()
    }

    /// Returns the write timeout for the socket.
    ///
    /// # Errors
    ///
    /// This function will return an error if the underlying socket cannot be queried.
    #[inline]
    pub fn write_timeout(&self) -> Result<Option<Duration>, io::Error> {
        self.socket.write_timeout()
    }

    /// Polls to receive a single datagram message from the socket.
    ///
    /// This is the poll-based counterpart to [`UdpSocket::recv`].
    #[inline]
    pub fn poll_recv(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<Result<usize, io::Error>> {
        let this = self.get_mut();
        let handle = &this.socket.handle;
        let buf_temp = unsafe { IoBufTemporaryPoll::new(buf.as_mut_ptr(), buf.len()) };
        let mut op = RecvOp::new(handle, buf_temp);
        handle.poll_op_poll(cx, &mut op)
    }

    /// Polls to receive a single datagram message, returning the sender's address.
    ///
    /// This is the poll-based counterpart to [`UdpSocket::recv_from`].
    #[inline]
    pub fn poll_recv_from(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<Result<(usize, SocketAddr), io::Error>> {
        let this = self.get_mut();
        let handle = &this.socket.handle;
        let buf_temp = unsafe { IoBufTemporaryPoll::new(buf.as_mut_ptr(), buf.len()) };
        let mut op = RecvfromOp::new(handle, buf_temp);
        handle.poll_op_poll(cx, &mut op)
    }

    /// Polls to send data on a connected socket.
    ///
    /// This is the poll-based counterpart to [`UdpSocket::send`].
    #[inline]
    pub fn poll_send(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        let this = self.get_mut();
        let handle = &this.socket.handle;
        let buf_temp = unsafe { IoBufTemporaryPoll::new(buf.as_ptr() as *mut u8, buf.len()) };
        let mut op = SendOp::new(handle, buf_temp);
        handle.poll_op_poll(cx, &mut op)
    }

    /// Polls to send data to the specified address.
    ///
    /// This is the poll-based counterpart to [`UdpSocket::send_to`].
    #[inline]
    pub fn poll_send_to(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
        target: SocketAddr,
    ) -> Poll<Result<usize, io::Error>> {
        let this = self.get_mut();
        let handle = &this.socket.handle;
        let buf_temp = unsafe { IoBufTemporaryPoll::new(buf.as_ptr() as *mut u8, buf.len()) };
        let mut op = SendtoOp::new(handle, buf_temp, target);
        handle.poll_op_poll(cx, &mut op)
    }

    /// Polls to peek at data from the socket without removing it.
    ///
    /// This is the poll-based counterpart to [`UdpSocket::peek`].
    #[inline]
    pub fn poll_peek(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<Result<usize, io::Error>> {
        let this = self.get_mut();
        let handle = &this.socket.handle;
        let buf_temp = unsafe { IoBufTemporaryPoll::new(buf.as_mut_ptr(), buf.len()) };
        let mut op = RecvOp::new_peek(handle, buf_temp);
        handle.poll_op_poll(cx, &mut op)
    }

    /// Polls to peek at data from the socket without removing it,
    /// returning the sender's address.
    ///
    /// This is the poll-based counterpart to [`UdpSocket::peek_from`].
    #[inline]
    pub fn poll_peek_from(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut [u8],
    ) -> Poll<Result<(usize, SocketAddr), io::Error>> {
        let this = self.get_mut();
        let handle = &this.socket.handle;
        let buf_temp = unsafe { IoBufTemporaryPoll::new(buf.as_mut_ptr(), buf.len()) };
        let mut op = RecvfromOp::new_peek(handle, buf_temp);
        handle.poll_op_poll(cx, &mut op)
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

impl AsyncReadPoll for PollUdpSocket {
    #[inline]
    fn poll_readable(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if *self.read_ready.borrow() {
            return Poll::Ready(Ok(()));
        }
        let poll = self
            .socket
            .handle
            .poll_op_poll(cx, &mut ReadinessOp::new_readable(&self.socket.handle))?;
        *self.read_ready.borrow_mut() = true;
        poll.map(Ok)
    }
}

impl AsyncWritePoll for PollUdpSocket {
    #[inline]
    fn poll_writable(&self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        if *self.write_ready.borrow() {
            return Poll::Ready(Ok(()));
        }
        let poll = self
            .socket
            .handle
            .poll_op_poll(cx, &mut ReadinessOp::new_writable(&self.socket.handle))?;
        *self.write_ready.borrow_mut() = true;
        poll.map(Ok)
    }
}

impl<'a> AsInnerRawHandle<'a> for PollUdpSocket {
    #[inline]
    fn as_inner_raw_handle(&'a self) -> &'a InnerRawHandle {
        self.socket.as_inner_raw_handle()
    }
}

#[cfg(unix)]
impl AsRawFd for PollUdpSocket {
    #[inline]
    fn as_raw_fd(&self) -> RawFd {
        self.socket.inner.as_raw_fd()
    }
}

#[cfg(unix)]
impl IntoRawFd for PollUdpSocket {
    #[inline]
    fn into_raw_fd(self) -> RawFd {
        self.socket.into_std().into_raw_fd()
    }
}

#[cfg(windows)]
impl AsRawSocket for PollUdpSocket {
    #[inline]
    fn as_raw_socket(&self) -> RawSocket {
        self.socket.inner.as_raw_socket()
    }
}

#[cfg(windows)]
impl IntoRawSocket for PollUdpSocket {
    #[inline]
    fn into_raw_socket(self) -> RawSocket {
        self.socket.into_std().into_raw_socket()
    }
}

#[cfg(test)]
mod tests {
    use std::io::{self as std_io};
    use std::net::SocketAddr;
    use std::pin::Pin;

    use crate::vibeio::driver::AnyDriver;

    use super::{PollUdpSocket, UdpSocket};

    #[cfg(windows)]
    #[test]
    fn udp_connect_preserves_registration_on_success_and_error() {
        use crate::vibeio::driver::RegistrationMode;
        use std::future::Future;
        use std::task::{Context, Poll, Waker};

        let runtime = crate::vibeio::executor::Runtime::new(AnyDriver::new_iocp().unwrap());
        runtime.block_on(async {
            for mode in [RegistrationMode::Poll, RegistrationMode::Completion] {
                let inner = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
                let mut socket = UdpSocket::from_std_with_mode(inner, mode).unwrap();
                let token = socket.handle.token;
                let completion = socket.handle.uses_completion();
                let peer = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
                let peer_addr = peer.local_addr().unwrap();
                let mut cx = Context::from_waker(Waker::noop());

                // There must be no pending future with temporarily changed modes.
                {
                    let mut connect = std::pin::pin!(socket.connect(peer_addr));
                    assert!(matches!(
                        connect.as_mut().poll(&mut cx),
                        Poll::Ready(Ok(()))
                    ));
                }
                assert_eq!(socket.handle.token, token);
                assert_eq!(socket.handle.uses_completion(), completion);
                assert_eq!(socket.peer_addr().unwrap(), peer_addr);

                // An IPv6 peer cannot be assigned to this IPv4 socket.
                {
                    let invalid = "[::1]:12345".parse::<SocketAddr>().unwrap();
                    let mut connect = std::pin::pin!(socket.connect(invalid));
                    assert!(matches!(
                        connect.as_mut().poll(&mut cx),
                        Poll::Ready(Err(_))
                    ));
                }
                assert_eq!(socket.handle.token, token);
                assert_eq!(socket.handle.uses_completion(), completion);

                socket.connect(peer_addr).await.unwrap();
                assert_eq!(socket.send(b"ping".to_vec()).await.0.unwrap(), 4);
                peer.set_read_timeout(Some(std::time::Duration::from_secs(5)))
                    .unwrap();
                let mut buf = [0; 4];
                assert_eq!(peer.recv(&mut buf).unwrap(), 4);
                assert_eq!(&buf, b"ping");
            }
        });
    }

    #[inline]
    fn try_bind_udp(address: SocketAddr) -> Option<UdpSocket> {
        match UdpSocket::bind(address) {
            Ok(socket) => Some(socket),
            Err(err) if err.kind() == std_io::ErrorKind::PermissionDenied => None,
            Err(err) => panic!("udp socket should bind: {err}"),
        }
    }

    #[inline]
    fn try_bind_poll_udp(address: SocketAddr) -> Option<PollUdpSocket> {
        match PollUdpSocket::bind(address) {
            Ok(socket) => Some(socket),
            Err(err) if err.kind() == std_io::ErrorKind::PermissionDenied => None,
            Err(err) => panic!("poll udp socket should bind: {err}"),
        }
    }

    #[test]
    fn udp_send_recv_and_peek_variants_work() {
        let runtime = crate::vibeio::executor::Runtime::new(
            #[cfg(unix)]
            AnyDriver::new_mio().expect("mio driver should initialize"),
            #[cfg(windows)]
            AnyDriver::new_iocp().expect("iocp driver should initialize"),
        );
        runtime.block_on(async {
            let address = "127.0.0.1:0"
                .parse::<SocketAddr>()
                .expect("address should parse");

            let Some(mut server) = try_bind_udp(address) else {
                return;
            };
            let Some(mut client) = try_bind_udp(address) else {
                return;
            };

            let server_addr = server.local_addr().expect("server local_addr should work");
            let client_addr = client.local_addr().expect("client local_addr should work");

            let sent = client
                .send_to(b"ping".to_vec(), server_addr)
                .await
                .0
                .expect("send_to should succeed");
            assert_eq!(sent, 4);

            let peek_from_buf = vec![0u8; 16];
            let (data, peek_from_buf) = server.peek_from(peek_from_buf).await;
            let (peeked, from_peek) = data.expect("peek_from should succeed");
            assert_eq!(&peek_from_buf[..peeked], b"ping");
            assert_eq!(from_peek, client_addr);

            let recv_from_buf = vec![0u8; 16];
            let (data, recv_from_buf) = server.recv_from(recv_from_buf).await;
            let (read, from_read) = data.expect("recv_from should succeed");
            assert_eq!(&recv_from_buf[..read], b"ping");
            assert_eq!(from_read, client_addr);

            server
                .connect(client_addr)
                .await
                .expect("server connect should work");
            client
                .connect(server_addr)
                .await
                .expect("client connect should work");

            let sent = client
                .send(b"echo".to_vec())
                .await
                .0
                .expect("send should succeed");
            assert_eq!(sent, 4);

            let peek_buf = vec![0u8; 16];
            let (peeked, peek_buf) = server.peek(peek_buf).await;
            let peeked = peeked.expect("peek should succeed");
            assert_eq!(&peek_buf[..peeked], b"echo");

            let recv_buf = vec![0u8; 16];
            let (read, recv_buf) = server.recv(recv_buf).await;
            let read = read.expect("recv should succeed");
            assert_eq!(&recv_buf[..read], b"echo");
        });
    }

    #[test]
    fn poll_udp_send_recv_and_peek_variants_work() {
        let runtime = crate::vibeio::executor::Runtime::new(
            #[cfg(unix)]
            AnyDriver::new_mio().expect("mio driver should initialize"),
            #[cfg(windows)]
            AnyDriver::new_iocp().expect("iocp driver should initialize"),
        );
        runtime.block_on(async {
            let address = "127.0.0.1:0"
                .parse::<SocketAddr>()
                .expect("address should parse");

            let Some(server) = try_bind_poll_udp(address) else {
                return;
            };
            let Some(mut client) = try_bind_poll_udp(address) else {
                return;
            };

            let server_addr = server.local_addr().expect("server local_addr should work");
            let client_addr = client.local_addr().expect("client local_addr should work");

            // poll_send_to
            let sent = Pin::new(&mut client)
                .send_to(b"ping".to_vec(), server_addr)
                .await
                .0
                .expect("send_to should succeed");
            assert_eq!(sent, 4);

            // poll_peek_from
            let peek_from_buf = vec![0u8; 16];
            let (data, peek_from_buf) = server.peek_from(peek_from_buf).await;
            let (peeked, from_peek) = data.expect("peek_from should succeed");
            assert_eq!(&peek_from_buf[..peeked], b"ping");
            assert_eq!(from_peek, client_addr);

            // poll_recv_from
            let recv_from_buf = vec![0u8; 16];
            let (data, recv_from_buf) = server.recv_from(recv_from_buf).await;
            let (read, from_read) = data.expect("recv_from should succeed");
            assert_eq!(&recv_from_buf[..read], b"ping");
            assert_eq!(from_read, client_addr);

            // Connect for connected send/recv
            let mut server = server;
            server
                .connect(client_addr)
                .await
                .expect("server connect should work");
            client
                .connect(server_addr)
                .await
                .expect("client connect should work");

            // poll_send
            let sent = Pin::new(&mut client)
                .send(b"echo".to_vec())
                .await
                .0
                .expect("send should succeed");
            assert_eq!(sent, 4);

            // poll_peek
            let peek_buf = vec![0u8; 16];
            let (peeked, peek_buf) = server.peek(peek_buf).await;
            let peeked = peeked.expect("peek should succeed");
            assert_eq!(&peek_buf[..peeked], b"echo");

            // poll_recv
            let recv_buf = vec![0u8; 16];
            let (read, recv_buf) = server.recv(recv_buf).await;
            let read = read.expect("recv should succeed");
            assert_eq!(&recv_buf[..read], b"echo");
        });
    }

    #[test]
    fn poll_udp_into_poll_roundtrip() {
        let runtime = crate::vibeio::executor::Runtime::new(
            #[cfg(unix)]
            AnyDriver::new_mio().expect("mio driver should initialize"),
            #[cfg(windows)]
            AnyDriver::new_iocp().expect("iocp driver should initialize"),
        );
        runtime.block_on(async {
            let address = "127.0.0.1:0"
                .parse::<SocketAddr>()
                .expect("address should parse");

            let Some(socket) = try_bind_udp(address) else {
                return;
            };
            let poll_socket = socket.into_poll().expect("into_poll should work");
            let _adaptive = poll_socket.into_adaptive();
        });
    }
}
