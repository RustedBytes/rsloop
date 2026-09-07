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
//! - Poll mode uses nonblocking socket calls and driver readiness notifications,
//!   not a blocking-pool fallback. Binding and ToSocketAddrs resolution are
//!   synchronous setup operations; prefer resolved addresses if DNS may block.
//! - Register sockets and drive async I/O inside a runtime. Missing-runtime
//!   registration returns an error; direct address/option queries need no current runtime.
//!
//! # Examples
//!
//! ## TCP Server
//!
//! See "TCP loopback with the Tokio I/O adapter" in
//! `tools/vibeio-check/EXAMPLES.md` for a finite client/server exchange.
//!
//! ## UDP Client
//!
//! See "UDP loopback exchange" in `tools/vibeio-check/EXAMPLES.md` for an
//! executable example with owned buffers, ephemeral ports and a timeout.
//!
//! ## Unix Domain Socket
//!
//! See "Unix socket exchange and path cleanup" in
//! `tools/vibeio-check/EXAMPLES.md` for an executable, Unix-gated example.

#[inline]
fn try_io_ready<T>(
    ready: &std::cell::RefCell<bool>,
    not_ready: &'static str,
    operation: impl FnOnce() -> std::io::Result<T>,
) -> std::io::Result<T> {
    if !*ready.borrow() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::WouldBlock,
            not_ready,
        ));
    }
    let result = operation();
    if result
        .as_ref()
        .is_err_and(|error| error.kind() == std::io::ErrorKind::WouldBlock)
    {
        *ready.borrow_mut() = false;
    }
    result
}

#[cfg(test)]
mod readiness_tests {
    #[test]
    fn only_would_block_clears_readiness_and_callbacks_run_unborrowed() {
        use std::{
            cell::RefCell,
            io::{self, ErrorKind},
        };
        let ready = RefCell::new(true);
        for kind in [
            ErrorKind::Interrupted,
            ErrorKind::ConnectionReset,
            ErrorKind::InvalidInput,
        ] {
            let result: io::Result<()> = super::try_io_ready(&ready, "not ready", || {
                assert!(ready.try_borrow_mut().is_ok());
                Err(kind.into())
            });
            assert_eq!(result.unwrap_err().kind(), kind);
            assert!(*ready.borrow());
        }
        assert_eq!(
            super::try_io_ready(&ready, "not ready", || Ok(42)).unwrap(),
            42
        );
        let result: io::Result<()> =
            super::try_io_ready(&ready, "not ready", || Err(ErrorKind::WouldBlock.into()));
        assert_eq!(result.unwrap_err().kind(), ErrorKind::WouldBlock);
        assert!(!*ready.borrow());
        let result: io::Result<()> =
            super::try_io_ready(&ready, "not ready", || panic!("not ready callback"));
        assert_eq!(result.unwrap_err().kind(), ErrorKind::WouldBlock);
    }
}

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

#[cfg(test)]
mod registration_tests {
    #[test]
    fn socket_registration_without_runtime_returns_an_error() {
        use std::io::ErrorKind;
        assert!(crate::vibeio::executor::current_driver().is_none());
        let udp = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        assert!(
            matches!(super::UdpSocket::from_std(udp), Err(error) if error.kind() == ErrorKind::NotConnected)
        );
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        assert!(
            matches!(super::TcpListener::from_std(listener), Err(error) if error.kind() == ErrorKind::NotConnected)
        );
        #[cfg(unix)]
        {
            let (stream, _peer) = std::os::unix::net::UnixStream::pair().unwrap();
            assert!(
                matches!(super::UnixStream::from_std(stream), Err(error) if error.kind() == ErrorKind::NotConnected)
            );
        }
    }
}

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
