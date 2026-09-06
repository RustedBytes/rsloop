//! A networking module for `vibeio`.
//!
//! This module provides async versions of common networking operations:
//! - TCP: [`TcpListener`], [`TcpStream`], [`PollTcpStream`]
//! - UDP: [`UdpSocket`]
#![cfg_attr(
    unix,
    doc = "- Unix domain sockets: [`UnixListener`], [`UnixStream`], [`PollUnixStream`]"
)]
#![cfg_attr(
    not(unix),
    doc = "- Unix domain socket wrappers are available on Unix targets only."
)]
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
// Public runtime API; not every embedding uses this re-export.
#[allow(unused_imports)]
pub use udp::*;
#[cfg(unix)]
pub use unix::*;

#[cfg(all(test, unix))]
mod ownership_tests {
    use super::*;
    use crate::vibeio::{
        driver::AnyDriver,
        executor::{Runtime, current_driver},
    };
    use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd};
    use std::sync::Arc;

    fn transfer(socket: impl IntoRawFd) -> OwnedFd {
        // SAFETY: IntoRawFd transfers sole ownership; reclaim it immediately.
        unsafe { OwnedFd::from_raw_fd(socket.into_raw_fd()) }
    }

    #[test]
    fn socket_conversions_release_registrations_and_preserve_live_sockets() {
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
            .extend((0..6).map(|index| Ok(mio::Token(index))));
        Runtime::new(driver).block_on(async {
            let udp = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
            let address = udp.local_addr().unwrap();
            let fd = udp.as_raw_fd();
            let udp = UdpSocket::from_std(udp).unwrap().into_std();
            assert_eq!(udp.as_raw_fd(), fd);
            assert_eq!(udp.local_addr().unwrap(), address);

            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let address = listener.local_addr().unwrap();
            let fd = listener.as_raw_fd();
            let listener =
                std::net::TcpListener::from(transfer(TcpListener::from_std(listener).unwrap()));
            assert_eq!(listener.as_raw_fd(), fd);
            assert_eq!(listener.local_addr().unwrap(), address);
            let client = std::net::TcpStream::connect(address).unwrap();
            let (peer, _) = listener.accept().unwrap();
            let fd = client.as_raw_fd();
            let client = std::net::TcpStream::from(transfer(TcpStream::from_std(client).unwrap()));
            assert_eq!(client.as_raw_fd(), fd);
            assert_eq!(client.peer_addr().unwrap(), address);

            let shared = Arc::new(client);
            let duplicate = std::net::TcpStream::from(transfer(
                TcpStream::from_shared(
                    shared.clone(),
                    crate::vibeio::driver::RegistrationMode::Completion,
                )
                .unwrap(),
            ));
            assert_ne!(duplicate.as_raw_fd(), shared.as_raw_fd());
            assert_eq!(duplicate.peer_addr().unwrap(), address);
            drop(duplicate);
            assert_eq!(Arc::strong_count(&shared), 1);
            assert_eq!(shared.peer_addr().unwrap(), address);
            drop((shared, peer, listener, udp));

            let (stream, peer) = std::os::unix::net::UnixStream::pair().unwrap();
            let fd = stream.as_raw_fd();
            let stream = std::os::unix::net::UnixStream::from(transfer(
                UnixStream::from_std(stream).unwrap(),
            ));
            assert_eq!(stream.as_raw_fd(), fd);
            assert!(stream.peer_addr().is_ok());
            drop((stream, peer));

            let path = std::env::temp_dir().join(format!(
                "vibeio-socket-{}-{}",
                std::process::id(),
                std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap()
                    .as_nanos()
            ));
            let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
            struct Cleanup(std::path::PathBuf);
            impl Drop for Cleanup {
                fn drop(&mut self) {
                    let _ = std::fs::remove_file(&self.0);
                }
            }
            let _cleanup = Cleanup(path.clone());
            let fd = listener.as_raw_fd();
            let listener = std::os::unix::net::UnixListener::from(transfer(
                UnixListener::from_std(listener).unwrap(),
            ));
            assert_eq!(listener.as_raw_fd(), fd);
            assert_eq!(
                listener.local_addr().unwrap().as_pathname(),
                Some(path.as_path())
            );
            drop(listener);

            let driver = current_driver().unwrap();
            let AnyDriver::Mock(mock) = driver.as_ref() else {
                unreachable!()
            };
            assert_eq!(
                *mock.registrations.as_ref().unwrap().deregistered.borrow(),
                (0..6).map(mio::Token).collect::<Vec<_>>()
            );
        });
    }
}
