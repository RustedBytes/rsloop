//! A networking module for `vibeio`.
//!
//! This module provides async versions of common networking operations:
//! - TCP: [`TcpListener`], [`TcpStream`], [`PollTcpStream`]
//! - UDP: [`UdpSocket`]
//! - Unix domain sockets: [`UnixListener`], [`UnixStream`], [`PollUnixStream`]
//!
//! Implementation notes:
//! - On Linux with io_uring support, some operations use native async syscalls (e.g. `accept4`, `sendto`)
//!   via the async driver. When io_uring completion is available, operations complete directly.
//! - For platforms without native async support, operations either offload to a blocking thread pool
//!   or fall back to synchronous std::net calls.
//! - The runtime must be active when calling these functions; otherwise they will panic.
//!
//! # Examples
//!
//! ## TCP Server
//!
//! ```ignore
//! use vibeio::net::TcpListener;
//!
//! let listener = TcpListener::bind("127.0.0.1:8080").await?;
//! loop {
//!     let (stream, addr) = listener.accept().await?;
//!     println!("Connection from: {}", addr);
//! }
//! ```
//!
//! ## UDP Client
//!
//! ```ignore
//! use vibeio::net::UdpSocket;
//!
//! let socket = UdpSocket::bind("127.0.0.1:0").await?;
//! socket.connect("127.0.0.1:9000").await?;
//! socket.send(b"hello").await?;
//! ```
//!
//! ## Unix Domain Socket
//!
//! ```ignore
//! use vibeio::net::UnixStream;
//!
//! let stream = UnixStream::connect("/tmp/mysocket").await?;
//! ```

mod tcp;
mod udp;
#[cfg(unix)]
mod unix;

pub use tcp::*;
pub use udp::*;
#[cfg(unix)]
pub use unix::*;
