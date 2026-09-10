//! Plaintext TCP and Unix transports, and the descriptor plumbing they need.
//!
//! `transport_from_socket*` take ownership of a Python socket (via `detach`)
//! and dispatch on its family; `spawn_tcp_transport` / `spawn_unix_transport`
//! are the direct entry points used by the accept path, which already holds a
//! std stream.
//!
//! The conversions below all funnel through socket2 so every descriptor the
//! transport adopts ends up non-blocking (and, for TCP, `TCP_NODELAY`) no
//! matter which route it arrived by. `duplicate_*` produce the second
//! descriptor the direct/lazy writer owns, so the reader and writer halves can
//! be closed independently.

use std::collections::HashMap;
use std::io;
use std::net::{TcpListener as StdTcpListener, TcpStream as StdTcpStream};
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::net::{UnixListener as StdUnixListener, UnixStream as StdUnixStream};
use std::sync::Arc;
use std::sync::Weak;

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;

use super::builder::{
    StreamTransportStateConfig, detached_socket_handle, fail_transport_worker_start,
    new_py_stream_transport, new_stream_transport_core, stream_transport_state_parts, tcp_family,
};
use super::io_targets::LazyWriterTarget;
use super::io_targets::{LazyWriterConfig, ReaderTarget, StreamKind, TaskedDirectWriter};
#[cfg(unix)]
use super::platform::{from_owned_raw_fd, unix_raw_fd};
use super::platform::{socket_from_owned_raw, tcp_stream_raw_fd};
use super::protocol::build_protocol_callbacks;
use super::tls_transport::{spawn_tls_client_transport, spawn_tls_server_transport};
use super::write_queue::channel as writer_channel;
use super::{PyStreamTransport, ServerCore, TransportSpawnContext, spawn_socket_reader};
use crate::fd_ops;
use crate::transport::tls::{ClientTlsSettings, ServerTlsSettings};

pub fn transport_from_socket(
    py: Python<'_>,
    spawn_context: TransportSpawnContext,
    socket_obj: Py<PyAny>,
) -> PyResult<Py<PyStreamTransport>> {
    #[allow(unused_variables)]
    let family = socket_obj.getattr(py, "family")?.extract::<i32>(py)?;
    #[cfg(unix)]
    if family == libc::AF_UNIX {
        let fd = detached_socket_handle(py, &socket_obj)?;
        return spawn_unix_transport(
            py,
            spawn_context,
            unix_stream_from_owned_socket_fd(fd)?,
            None,
        );
    }

    let fd = detached_socket_handle(py, &socket_obj)?;
    spawn_tcp_transport(
        py,
        spawn_context,
        tcp_stream_from_owned_socket_fd(fd)?,
        None,
    )
}

pub fn transport_from_socket_tls(
    py: Python<'_>,
    spawn_context: TransportSpawnContext,
    socket_obj: Py<PyAny>,
    tls: ClientTlsSettings,
) -> PyResult<Py<PyStreamTransport>> {
    #[allow(unused_variables)]
    let family = socket_obj.getattr(py, "family")?.extract::<i32>(py)?;
    #[cfg(unix)]
    if family == libc::AF_UNIX {
        let fd = detached_socket_handle(py, &socket_obj)?;
        return spawn_tls_client_transport(
            py,
            spawn_context,
            StreamKind::Unix(unix_stream_from_owned_socket_fd(fd)?),
            tls,
            None,
            true,
        );
    }

    let fd = detached_socket_handle(py, &socket_obj)?;
    spawn_tls_client_transport(
        py,
        spawn_context,
        StreamKind::Tcp(tcp_stream_from_owned_socket_fd(fd)?),
        tls,
        None,
        true,
    )
}

pub fn transport_from_socket_server_tls(
    py: Python<'_>,
    spawn_context: TransportSpawnContext,
    socket_obj: Py<PyAny>,
    tls: ServerTlsSettings,
) -> PyResult<Py<PyStreamTransport>> {
    #[allow(unused_variables)]
    let family = socket_obj.getattr(py, "family")?.extract::<i32>(py)?;
    #[cfg(unix)]
    if family == libc::AF_UNIX {
        let fd = detached_socket_handle(py, &socket_obj)?;
        return spawn_tls_server_transport(
            py,
            spawn_context,
            StreamKind::Unix(unix_stream_from_owned_socket_fd(fd)?),
            tls,
            None,
            true,
        );
    }

    let fd = detached_socket_handle(py, &socket_obj)?;
    spawn_tls_server_transport(
        py,
        spawn_context,
        StreamKind::Tcp(tcp_stream_from_owned_socket_fd(fd)?),
        tls,
        None,
        true,
    )
}
#[inline]
pub fn tcp_stream_from_owned_socket_fd(fd: fd_ops::RawFd) -> PyResult<StdTcpStream> {
    configured_tcp_stream_from_owned_fd(fd)
}

#[cfg(unix)]
pub fn unix_stream_from_owned_socket_fd(fd: fd_ops::RawFd) -> PyResult<StdUnixStream> {
    let stream = from_owned_raw_fd::<StdUnixStream>(fd)?;
    stream
        .set_nonblocking(true)
        .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
    Ok(stream)
}

pub fn tcp_listener_from_owned_socket_fd(fd: fd_ops::RawFd) -> PyResult<StdTcpListener> {
    configured_tcp_listener_from_owned_fd(fd)
}

#[cfg(unix)]
pub fn unix_listener_from_owned_socket_fd(fd: fd_ops::RawFd) -> PyResult<StdUnixListener> {
    let listener = from_owned_raw_fd::<StdUnixListener>(fd)?;
    listener
        .set_nonblocking(true)
        .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
    Ok(listener)
}

pub(super) fn duplicate_configured_tcp_stream(fd: fd_ops::RawFd) -> PyResult<StdTcpStream> {
    let dup = fd_ops::dup_raw_fd(fd).map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
    configured_tcp_stream_from_owned_fd(dup)
}

pub(super) fn configured_tcp_stream_from_owned_fd(fd: fd_ops::RawFd) -> PyResult<StdTcpStream> {
    let socket = socket_from_owned_raw(fd)?;
    socket
        .set_nonblocking(true)
        .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
    socket
        .set_tcp_nodelay(true)
        .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
    Ok(socket.into())
}

pub(super) fn configured_tcp_listener_from_owned_fd(fd: fd_ops::RawFd) -> PyResult<StdTcpListener> {
    let socket = socket_from_owned_raw(fd)?;
    socket
        .set_nonblocking(true)
        .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
    Ok(socket.into())
}

#[cfg(unix)]
pub(super) fn duplicate_unix_direct_writer(raw_fd: fd_ops::RawFd) -> PyResult<StdUnixStream> {
    let writer_fd =
        fd_ops::dup_raw_fd(raw_fd).map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
    let direct_writer = from_owned_raw_fd::<StdUnixStream>(writer_fd)?;
    direct_writer
        .set_nonblocking(true)
        .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
    Ok(direct_writer)
}
pub fn spawn_tcp_transport(
    py: Python<'_>,
    mut spawn_context: TransportSpawnContext,
    stream: StdTcpStream,
    server: Option<Weak<ServerCore>>,
) -> PyResult<Py<PyStreamTransport>> {
    let raw_fd = tcp_stream_raw_fd(&stream);
    let family = tcp_family(&stream);
    let extra = HashMap::new();
    let callbacks = build_protocol_callbacks(py, &spawn_context.protocol)?;
    spawn_context.context_needs_run &= callbacks.stream_reader_fast_path.is_none();
    let stream = Arc::new(stream);
    let (writer_tx, writer_rx) = writer_channel();
    let parts = stream_transport_state_parts(
        spawn_context,
        callbacks,
        StreamTransportStateConfig {
            io_fd: Some(raw_fd),
            runtime_socket_io: true,
            extra,
            lazy_socket_family: Some(family),
            reading: true,
            writable: true,
            can_write_eof: true,
            close_on_write_eof: false,
            server,
        },
    );
    let core = new_stream_transport_core(
        parts,
        writer_tx,
        Some(TaskedDirectWriter::Tcp(Arc::clone(&stream))),
        Some(LazyWriterConfig {
            target: LazyWriterTarget::Tcp(raw_fd),
            writer_rx,
        }),
    );

    let transport = new_py_stream_transport(py, &core)?;
    core.connection_made(transport.clone_ref(py))?;
    if let Some(server) = core.server_ref().and_then(|weak| weak.upgrade()) {
        server.connection_opened();
    }

    if let Err(err) = spawn_socket_reader(raw_fd, Arc::clone(&core), ReaderTarget::Tcp(stream)) {
        return Err(fail_transport_worker_start(
            py,
            &core,
            io::Error::other(err.to_string()),
        ));
    }
    Ok(transport)
}

#[cfg(unix)]
pub fn spawn_unix_transport(
    py: Python<'_>,
    mut spawn_context: TransportSpawnContext,
    stream: StdUnixStream,
    server: Option<Weak<ServerCore>>,
) -> PyResult<Py<PyStreamTransport>> {
    let raw_fd = unix_raw_fd(stream.as_raw_fd());
    let extra = HashMap::new();
    let callbacks = build_protocol_callbacks(py, &spawn_context.protocol)?;
    spawn_context.context_needs_run &= callbacks.stream_reader_fast_path.is_none();
    let direct_writer = duplicate_unix_direct_writer(raw_fd)?;
    let (writer_tx, writer_rx) = writer_channel();
    let parts = stream_transport_state_parts(
        spawn_context,
        callbacks,
        StreamTransportStateConfig {
            io_fd: Some(raw_fd),
            runtime_socket_io: true,
            extra,
            lazy_socket_family: Some(libc::AF_UNIX),
            reading: true,
            writable: true,
            can_write_eof: true,
            close_on_write_eof: false,
            server,
        },
    );
    let core = new_stream_transport_core(
        parts,
        writer_tx,
        Some(TaskedDirectWriter::Unix(direct_writer)),
        Some(LazyWriterConfig {
            target: LazyWriterTarget::Unix(raw_fd),
            writer_rx,
        }),
    );

    let transport = new_py_stream_transport(py, &core)?;
    core.connection_made(transport.clone_ref(py))?;
    if let Some(server) = core.server_ref().and_then(|weak| weak.upgrade()) {
        server.connection_opened();
    }

    if let Err(err) = spawn_socket_reader(raw_fd, Arc::clone(&core), ReaderTarget::Unix(stream)) {
        return Err(fail_transport_worker_start(
            py,
            &core,
            io::Error::other(err.to_string()),
        ));
    }
    Ok(transport)
}
