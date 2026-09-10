//! rustls session state and the record-level plumbing around it.
//!
//! `TlsIoState` pairs a rustls connection with the socket it speaks over, and
//! everything here operates on that pair under one lock: the initial blocking
//! handshake, moving records in and out of the connection, draining decrypted
//! plaintext into the transport's read queue, and the close-notify shutdown.
//! Keeping the lock discipline in a single module is what lets the reader and
//! writer workers share one session without stepping on each other.

use std::io::{self, Read as _, Write as _};
use std::sync::{Arc, Mutex, Weak};
use std::thread;
use std::time::Duration;

use rustls::{ClientConnection, ServerConnection};

use super::io_targets::StreamKind;
use super::poll::{wait_socket_ready, wait_socket_ready_once, wait_socket_ready_until};
use super::{PendingReadEvent, ServerCore, StreamTransportCore};
use crate::fd_ops;

pub(super) enum TlsConnectionKind {
    Client(ClientConnection),
    Server(ServerConnection),
}

impl TlsConnectionKind {
    pub(super) fn is_handshaking(&self) -> bool {
        match self {
            Self::Client(conn) => conn.is_handshaking(),
            Self::Server(conn) => conn.is_handshaking(),
        }
    }

    pub(super) fn wants_read(&self) -> bool {
        match self {
            Self::Client(conn) => conn.wants_read(),
            Self::Server(conn) => conn.wants_read(),
        }
    }

    pub(super) fn wants_write(&self) -> bool {
        match self {
            Self::Client(conn) => conn.wants_write(),
            Self::Server(conn) => conn.wants_write(),
        }
    }

    pub(super) fn read_tls(&mut self, stream: &mut StreamKind) -> io::Result<usize> {
        match self {
            Self::Client(conn) => conn.read_tls(stream),
            Self::Server(conn) => conn.read_tls(stream),
        }
    }

    pub(super) fn write_tls(&mut self, stream: &mut StreamKind) -> io::Result<usize> {
        match self {
            Self::Client(conn) => conn.write_tls(stream),
            Self::Server(conn) => conn.write_tls(stream),
        }
    }

    pub(super) fn process_new_packets(&mut self) -> Result<(), rustls::Error> {
        match self {
            Self::Client(conn) => conn.process_new_packets().map(|_| ()),
            Self::Server(conn) => conn.process_new_packets().map(|_| ()),
        }
    }

    pub(super) fn reader_read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Client(conn) => conn.reader().read(buf),
            Self::Server(conn) => conn.reader().read(buf),
        }
    }

    pub(super) fn writer_write_all(&mut self, data: &[u8]) -> io::Result<()> {
        match self {
            Self::Client(conn) => conn.writer().write_all(data),
            Self::Server(conn) => conn.writer().write_all(data),
        }
    }

    pub(super) fn send_close_notify(&mut self) {
        match self {
            Self::Client(conn) => conn.send_close_notify(),
            Self::Server(conn) => conn.send_close_notify(),
        }
    }
}

pub(super) struct TlsIoState {
    pub(super) stream: StreamKind,
    pub(super) connection: TlsConnectionKind,
    pub(super) shutdown_timeout: Duration,
}

pub(super) type SharedTlsIoState = Arc<Mutex<TlsIoState>>;

impl TlsIoState {
    #[inline]
    pub(super) fn fd(&self) -> fd_ops::RawFd {
        self.stream.fd()
    }

    pub(super) fn pollable(&self) -> bool {
        self.stream.pollable()
    }

    #[inline]
    pub(super) fn shutdown_close(&self) -> io::Result<()> {
        self.stream.shutdown_close()
    }

    #[inline]
    pub(super) fn read_tls(&mut self) -> io::Result<usize> {
        self.connection.read_tls(&mut self.stream)
    }

    #[inline]
    pub(super) fn write_tls(&mut self) -> io::Result<usize> {
        self.connection.write_tls(&mut self.stream)
    }
}

pub(super) enum TlsReadOutcome {
    Continue,
    Eof,
    ConnectionLost(String),
}

pub(super) fn tls_server_closed(server: Option<&Weak<ServerCore>>) -> bool {
    server.is_some_and(|server| server.upgrade().is_none_or(|server| server.is_closed()))
}

pub(super) fn complete_tls_handshake(
    tls_state: &SharedTlsIoState,
    timeout: Duration,
    server: Option<&Weak<ServerCore>>,
) -> io::Result<()> {
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if tls_server_closed(server) {
            return Err(io::Error::new(
                io::ErrorKind::Interrupted,
                "TLS handshake cancelled",
            ));
        }
        if std::time::Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "TLS handshake timed out",
            ));
        }

        if tls_handshake_step(tls_state)? {
            return Ok(());
        }
    }
}

pub(super) fn tls_handshake_step(tls_state: &SharedTlsIoState) -> io::Result<bool> {
    let mut state = tls_state.lock().expect("poisoned tls state");
    if !state.connection.is_handshaking() {
        if state.connection.wants_write() {
            flush_tls_io_locked(&mut state)?;
        }
        return Ok(true);
    }

    if state.connection.wants_write() {
        flush_tls_io_locked(&mut state)?;
        return Ok(false);
    }

    if state.connection.wants_read() {
        let fd = state.fd();
        let pollable = state.pollable();
        drop(state);
        continue_tls_handshake_read(tls_state, fd, pollable)?;
        return Ok(false);
    }

    thread::sleep(Duration::from_millis(10));
    Ok(false)
}

pub(super) fn continue_tls_handshake_read(
    tls_state: &SharedTlsIoState,
    fd: fd_ops::RawFd,
    pollable: bool,
) -> io::Result<()> {
    if !wait_socket_ready_once(fd, pollable, true, false)? {
        return Ok(());
    }
    let mut state = tls_state.lock().expect("poisoned tls state");
    let n = state.read_tls()?;
    if n == 0 && state.connection.is_handshaking() {
        return Err(io::Error::new(
            io::ErrorKind::UnexpectedEof,
            "TLS handshake ended before completion",
        ));
    }
    state.connection.process_new_packets().map_err(tls_io_error)
}

pub(super) fn drain_tls_plaintext_locked(
    core: &Arc<StreamTransportCore>,
    state: &mut TlsIoState,
    plaintext: &mut Vec<u8>,
) -> Result<bool, String> {
    let mut saw_data = false;
    loop {
        plaintext.resize(plaintext.capacity(), 0);
        match state.connection.reader_read(plaintext) {
            Ok(0) => break,
            Ok(n) => {
                saw_data = true;
                plaintext.truncate(n);
                let data = std::mem::take(plaintext);
                core.enqueue_pending_read_event(PendingReadEvent::Data(data));
                let Some(next) =
                    core.acquire_read_buffer_blocking(super::tuning::STREAM_READ_BUFFER_SIZE, None)
                else {
                    return Ok(saw_data);
                };
                *plaintext = next;
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
            Err(err) => return Err(err.to_string()),
        }
    }
    Ok(saw_data)
}

pub(super) fn drain_buffered_tls_plaintext(
    core: &Arc<StreamTransportCore>,
    tls_state: &SharedTlsIoState,
    plaintext: &mut Vec<u8>,
) -> TlsReadOutcome {
    let mut state = tls_state.lock().expect("poisoned tls state");
    match drain_tls_plaintext_locked(core, &mut state, plaintext) {
        Ok(true) => TlsReadOutcome::Continue,
        Ok(false) => TlsReadOutcome::Eof,
        Err(err) => TlsReadOutcome::ConnectionLost(err),
    }
}

pub(super) fn read_tls_records(
    core: &Arc<StreamTransportCore>,
    tls_state: &SharedTlsIoState,
    plaintext: &mut Vec<u8>,
) -> TlsReadOutcome {
    let mut state = tls_state.lock().expect("poisoned tls state");
    match state.read_tls() {
        Ok(0) => {
            if let Err(err) = state.connection.process_new_packets().map_err(tls_io_error) {
                return TlsReadOutcome::ConnectionLost(err.to_string());
            }
            match drain_tls_plaintext_locked(core, &mut state, plaintext) {
                Ok(true) => TlsReadOutcome::Continue,
                Ok(false) => TlsReadOutcome::Eof,
                Err(err) => TlsReadOutcome::ConnectionLost(err),
            }
        }
        Ok(_) => {
            if let Err(err) = state.connection.process_new_packets().map_err(tls_io_error) {
                return TlsReadOutcome::ConnectionLost(err.to_string());
            }
            if let Err(err) = flush_tls_io_locked(&mut state) {
                return TlsReadOutcome::ConnectionLost(err.to_string());
            }
            match drain_tls_plaintext_locked(core, &mut state, plaintext) {
                Ok(_) => TlsReadOutcome::Continue,
                Err(err) => TlsReadOutcome::ConnectionLost(err),
            }
        }
        Err(err)
            if err.kind() == io::ErrorKind::WouldBlock
                || err.kind() == io::ErrorKind::Interrupted =>
        {
            TlsReadOutcome::Continue
        }
        Err(err) => TlsReadOutcome::ConnectionLost(err.to_string()),
    }
}

pub(super) fn tls_socket_wait_target(tls_state: &SharedTlsIoState) -> (fd_ops::RawFd, bool) {
    let state = tls_state.lock().expect("poisoned tls state");
    (state.fd(), state.pollable())
}

pub(super) fn close_tls_writer(tls_state: &SharedTlsIoState) -> io::Result<()> {
    let mut state = tls_state.lock().expect("poisoned tls state");
    let shutdown_timeout = state.shutdown_timeout;
    state.connection.send_close_notify();
    let result = flush_tls_close_io_locked(&mut state, shutdown_timeout);
    let close_result = state.shutdown_close();
    result.and(close_result)
}

pub(super) fn abort_tls_writer(tls_state: &SharedTlsIoState) -> io::Result<()> {
    let state = tls_state.lock().expect("poisoned tls state");
    state.shutdown_close()
}

pub(super) fn flush_tls_io_locked(state: &mut TlsIoState) -> io::Result<()> {
    while state.connection.wants_write() {
        match state.write_tls() {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "failed to flush TLS records",
                ));
            }
            Ok(_) => {}
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                wait_socket_ready(state.fd(), state.pollable(), false, true)?;
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

pub(super) fn flush_tls_close_io_locked(
    state: &mut TlsIoState,
    timeout: Duration,
) -> io::Result<()> {
    let deadline = std::time::Instant::now()
        .checked_add(timeout)
        .unwrap_or_else(std::time::Instant::now);
    while state.connection.wants_write() {
        match state.write_tls() {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "failed to flush TLS records",
                ));
            }
            Ok(_) => {}
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                wait_socket_ready_until(state.fd(), state.pollable(), false, true, deadline)?;
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    }
    Ok(())
}

pub(super) fn tls_io_error(err: rustls::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, err.to_string())
}

#[cfg(all(test, unix))]
mod tests {
    use std::fs;
    use std::io::{BufReader, Cursor, Read, Write};
    use std::os::fd::AsRawFd;
    use std::os::unix::net::UnixStream;
    use std::time::Duration;

    use pyo3::Python;
    use rustls::pki_types::ServerName;
    use rustls::{ClientConfig, RootCertStore};

    use super::*;
    use crate::transport::stream::server_core::tests::{build_test_server, shutdown_test_server};

    const CERTFILE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/tls/cert.pem");

    fn client_connection() -> ClientConnection {
        let cert_data = fs::read(CERTFILE).expect("read certificate fixture");
        let certs = rustls_pemfile::certs(&mut BufReader::new(Cursor::new(cert_data)))
            .collect::<Result<Vec<_>, _>>()
            .expect("parse certificate fixture");
        let mut roots = RootCertStore::empty();
        roots
            .add(certs[0].clone())
            .expect("trust certificate fixture");
        let config = ClientConfig::builder_with_protocol_versions(rustls::DEFAULT_VERSIONS)
            .with_root_certificates(roots)
            .with_no_client_auth();
        ClientConnection::new(
            Arc::new(config),
            ServerName::try_from("localhost".to_owned()).expect("valid server name"),
        )
        .expect("create client connection")
    }

    fn client_tls_state(stream: UnixStream) -> SharedTlsIoState {
        Arc::new(Mutex::new(TlsIoState {
            stream: StreamKind::Unix(stream),
            connection: TlsConnectionKind::Client(client_connection()),
            shutdown_timeout: Duration::from_millis(50),
        }))
    }

    #[test]
    fn handshake_timeout_and_closed_server_cancel_before_io() {
        let (stream, _peer) = UnixStream::pair().expect("Unix socketpair");
        let tls_state = client_tls_state(stream);
        let err = complete_tls_handshake(&tls_state, Duration::ZERO, None)
            .expect_err("zero timeout should fail before handshake I/O");
        assert_eq!(err.kind(), io::ErrorKind::TimedOut);

        crate::initialize_python_for_tests();
        Python::attach(|py| {
            let (server, loop_core) = build_test_server(py);
            let weak = Arc::downgrade(&server);
            server.close();
            let err = complete_tls_handshake(&tls_state, Duration::from_secs(1), Some(&weak))
                .expect_err("closed server should cancel handshake");
            assert_eq!(err.kind(), io::ErrorKind::Interrupted);
            shutdown_test_server(server, loop_core);
        });
    }

    #[test]
    fn server_closed_detection_handles_live_closed_and_dropped_servers() {
        assert!(!tls_server_closed(None));
        crate::initialize_python_for_tests();
        Python::attach(|py| {
            let (server, loop_core) = build_test_server(py);
            let weak = Arc::downgrade(&server);
            assert!(!tls_server_closed(Some(&weak)));
            server.close();
            assert!(tls_server_closed(Some(&weak)));
            shutdown_test_server(server, Arc::clone(&loop_core));
            assert!(tls_server_closed(Some(&weak)));
            drop(loop_core);
        });
    }

    #[test]
    fn handshake_read_reports_eof_before_completion() {
        let (stream, peer) = UnixStream::pair().expect("Unix socketpair");
        let fd = i64::from(stream.as_raw_fd());
        let tls_state = client_tls_state(stream);
        drop(peer);

        let err = continue_tls_handshake_read(&tls_state, fd, true)
            .expect_err("closed peer should end handshake");
        assert_eq!(err.kind(), io::ErrorKind::UnexpectedEof);
    }

    #[test]
    fn malformed_peer_records_become_invalid_data_errors() {
        let (stream, mut peer) = UnixStream::pair().expect("Unix socketpair");
        peer.set_read_timeout(Some(Duration::from_secs(1)))
            .expect("peer read timeout");
        let tls_state = client_tls_state(stream);

        assert!(!tls_handshake_step(&tls_state).expect("flush client hello"));
        let mut hello = [0_u8; 4096];
        assert!(peer.read(&mut hello).expect("read client hello") > 0);
        peer.write_all(&[0_u8; 16])
            .expect("write malformed TLS record");

        let err = tls_handshake_step(&tls_state).expect_err("malformed TLS should fail");
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
    }

    #[test]
    fn abort_closes_the_socket_and_error_conversion_keeps_tls_context() {
        let (stream, mut peer) = UnixStream::pair().expect("Unix socketpair");
        peer.set_read_timeout(Some(Duration::from_secs(1)))
            .expect("peer read timeout");
        let expected_fd = i64::from(stream.as_raw_fd());
        let tls_state = client_tls_state(stream);
        assert_eq!(tls_socket_wait_target(&tls_state), (expected_fd, true));

        abort_tls_writer(&tls_state).expect("abort TLS writer");
        let mut byte = [0_u8; 1];
        assert_eq!(peer.read(&mut byte).expect("observe socket shutdown"), 0);

        let err = tls_io_error(rustls::Error::General("bad peer record".to_owned()));
        assert_eq!(err.kind(), io::ErrorKind::InvalidData);
        assert!(err.to_string().contains("bad peer record"));
    }
}
