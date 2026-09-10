//! Building a transport whose socket is wrapped in a TLS session.
//!
//! The handshake runs to completion *before* the transport is handed back, on
//! the calling thread with the GIL released — so `connection_made` only fires
//! on a connection that is actually usable, and a failed handshake surfaces as
//! an ordinary Python exception. Only after that do the TLS reader and writer
//! workers start.
//!
//! `prepare_start_tls_transport` covers `loop.start_tls()`: it reclaims the
//! plaintext transport's socket (stopping its reader first, so no plaintext
//! reader races the handshake) and returns the pieces a fresh TLS transport is
//! then built from.

use std::collections::HashMap;
#[cfg(unix)]
use std::os::fd::AsRawFd;
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use rustls::{ClientConnection, ServerConnection};

use super::builder::{
    StreamTransportStateConfig, fail_transport_worker_start, make_stream_extra, merge_extra,
    new_py_stream_transport, new_stream_transport_core, stream_transport_state_parts, tcp_family,
};
use super::io_targets::StreamKind;
use super::platform::tcp_stream_raw_fd;
#[cfg(unix)]
use super::platform::unix_raw_fd;
use super::protocol::build_protocol_callbacks;
use super::tls_session::{
    SharedTlsIoState, TlsConnectionKind, TlsIoState, complete_tls_handshake, tls_server_closed,
};
use super::write_queue::channel as writer_channel;
use super::{
    PyStreamTransport, ServerCore, TransportSpawnContext, spawn_tls_reader_worker,
    spawn_tls_writer_worker,
};
use crate::transport::tls::{ClientTlsSettings, ServerTlsSettings, tls_extra};

pub struct PreparedTlsTransport {
    spawn_context: TransportSpawnContext,
    stream: StreamKind,
}

/// Retires plaintext I/O before a TLS handshake is allowed to touch the socket.
pub fn prepare_start_tls_transport(
    py: Python<'_>,
    transport: Py<PyStreamTransport>,
    protocol: Py<PyAny>,
) -> PyResult<PreparedTlsTransport> {
    let (mut spawn_context, stream) = transport.borrow(py).core.upgrade_stream(py)?;
    spawn_context.protocol = protocol;
    Ok(PreparedTlsTransport {
        spawn_context,
        stream,
    })
}

pub fn start_tls_transport(
    py: Python<'_>,
    prepared: PreparedTlsTransport,
    client_tls: Option<ClientTlsSettings>,
    server_tls: Option<ServerTlsSettings>,
) -> PyResult<Py<PyStreamTransport>> {
    let PreparedTlsTransport {
        spawn_context,
        stream,
    } = prepared;
    match (client_tls, server_tls) {
        (Some(tls), None) => spawn_tls_client_transport(py, spawn_context, stream, tls, None, true),
        (None, Some(tls)) => spawn_tls_server_transport(py, spawn_context, stream, tls, None, true),
        _ => Err(PyRuntimeError::new_err("invalid TLS upgrade configuration")),
    }
}

pub(super) fn spawn_tls_client_transport(
    py: Python<'_>,
    spawn_context: TransportSpawnContext,
    stream: StreamKind,
    tls: ClientTlsSettings,
    server: Option<Weak<ServerCore>>,
    call_connection_made: bool,
) -> PyResult<Py<PyStreamTransport>> {
    let connection = ClientConnection::new(tls.config, tls.server_name)
        .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
    spawn_tls_transport(
        py,
        spawn_context,
        stream,
        TlsTransportConfig {
            connection: TlsConnectionKind::Client(connection),
            tls_extra: tls_extra(py, &tls.ssl_context),
            handshake_timeout: tls.handshake_timeout,
            shutdown_timeout: tls.shutdown_timeout,
            server,
            call_connection_made,
        },
    )
}

pub(super) fn spawn_tls_server_transport(
    py: Python<'_>,
    spawn_context: TransportSpawnContext,
    stream: StreamKind,
    tls: ServerTlsSettings,
    server: Option<Weak<ServerCore>>,
    call_connection_made: bool,
) -> PyResult<Py<PyStreamTransport>> {
    let connection = ServerConnection::new(tls.config)
        .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
    spawn_tls_transport(
        py,
        spawn_context,
        stream,
        TlsTransportConfig {
            connection: TlsConnectionKind::Server(connection),
            tls_extra: tls_extra(py, &tls.ssl_context),
            handshake_timeout: tls.handshake_timeout,
            shutdown_timeout: tls.shutdown_timeout,
            server,
            call_connection_made,
        },
    )
}

pub(super) struct TlsTransportConfig {
    pub(super) connection: TlsConnectionKind,
    pub(super) tls_extra: HashMap<String, Py<PyAny>>,
    pub(super) handshake_timeout: Duration,
    pub(super) shutdown_timeout: Duration,
    pub(super) server: Option<Weak<ServerCore>>,
    pub(super) call_connection_made: bool,
}

pub(super) fn tls_stream_extra(
    py: Python<'_>,
    stream: &StreamKind,
    extra_tls: HashMap<String, Py<PyAny>>,
) -> PyResult<HashMap<String, Py<PyAny>>> {
    match stream {
        StreamKind::Tcp(stream) => Ok(merge_extra(
            make_stream_extra(py, tcp_stream_raw_fd(stream), tcp_family(stream))?,
            extra_tls,
        )),
        #[cfg(unix)]
        StreamKind::Unix(stream) => Ok(merge_extra(
            make_stream_extra(py, unix_raw_fd(stream.as_raw_fd()), libc::AF_UNIX)?,
            extra_tls,
        )),
    }
}

pub(super) fn tls_io_state(
    stream: StreamKind,
    connection: TlsConnectionKind,
    shutdown_timeout: Duration,
) -> SharedTlsIoState {
    Arc::new(Mutex::new(TlsIoState {
        stream,
        connection,
        shutdown_timeout,
    }))
}

pub(super) fn spawn_tls_transport(
    py: Python<'_>,
    mut spawn_context: TransportSpawnContext,
    stream: StreamKind,
    config: TlsTransportConfig,
) -> PyResult<Py<PyStreamTransport>> {
    let TlsTransportConfig {
        connection,
        tls_extra: extra_tls,
        handshake_timeout,
        shutdown_timeout,
        server,
        call_connection_made,
    } = config;
    let handshake_server = server.clone();
    let callbacks = build_protocol_callbacks(py, &spawn_context.protocol)?;
    spawn_context.context_needs_run &= callbacks.stream_reader_fast_path.is_none();
    let (writer_tx, writer_rx) = writer_channel();
    let stream_fd = stream.fd();
    let tls_state = tls_io_state(stream, connection, shutdown_timeout);

    py.detach(|| complete_tls_handshake(&tls_state, handshake_timeout, handshake_server.as_ref()))
        .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
    if tls_server_closed(handshake_server.as_ref()) {
        return Err(PyRuntimeError::new_err("TLS handshake cancelled"));
    }
    let extra = {
        let state = tls_state.lock().expect("poisoned tls state");
        tls_stream_extra(py, &state.stream, extra_tls)?
    };

    let parts = stream_transport_state_parts(
        spawn_context,
        callbacks,
        StreamTransportStateConfig {
            io_fd: Some(stream_fd),
            runtime_socket_io: true,
            extra,
            lazy_socket_family: None,
            reading: true,
            writable: true,
            can_write_eof: false,
            close_on_write_eof: false,
            server,
        },
    );
    let core = new_stream_transport_core(parts, writer_tx, None, None);

    let transport = new_py_stream_transport(py, &core)?;
    if call_connection_made {
        core.connection_made(transport.clone_ref(py))?;
    }
    if let Some(server) = core.server_ref().and_then(|weak| weak.upgrade()) {
        server.connection_opened();
    }

    if let Err(err) = spawn_tls_reader_worker(Arc::clone(&core), Arc::clone(&tls_state)) {
        return Err(fail_transport_worker_start(py, &core, err));
    }
    if let Err(err) = spawn_tls_writer_worker(Arc::clone(&core), tls_state, writer_rx) {
        return Err(fail_transport_worker_start(py, &core, err));
    }
    Ok(transport)
}
