//! Transports over pipes and other non-socket file objects.
//!
//! `connect_read_pipe` / `connect_write_pipe` and the subprocess stdio plumbing
//! land here. A pipe is half-duplex from the transport's point of view, so the
//! unused direction is given a real but inert worker: a read pipe writes to
//! `io::sink()`, and a write pipe never starts a reader. The descriptor is
//! duplicated so closing the transport does not close the caller's file object.

use std::collections::HashMap;
use std::fs;
use std::io;
use std::sync::Arc;

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;

use super::builder::{
    StreamTransportStateConfig, fail_transport_worker_start, new_py_stream_transport,
    new_stream_transport_core, stream_transport_state_parts,
};
use super::io_targets::{ReaderTarget, WriterTarget};
#[cfg(not(windows))]
use super::platform::from_owned_raw_fd;
#[cfg(windows)]
use super::platform::from_owned_raw_handle;
use super::protocol::build_protocol_callbacks;
use super::write_queue::{WriterReceiver, channel as writer_channel};
use super::{
    PyStreamTransport, StreamTransportCore, TransportSpawnContext, spawn_reader_worker,
    spawn_writer_worker,
};
use crate::fd_ops;

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
pub fn spawn_read_pipe_transport(
    py: Python<'_>,
    spawn_context: TransportSpawnContext,
    pipe_obj: Py<PyAny>,
) -> PyResult<Py<PyStreamTransport>> {
    let file = pipe_file_from_obj(py, &pipe_obj)?;
    let (core, transport, writer_rx) = pipe_transport_core(
        py,
        spawn_context,
        pipe_extra(py, &pipe_obj, None),
        PipeTransportMode::Read,
    )?;
    core.connection_made(transport.clone_ref(py))?;
    if let Err(err) = spawn_reader_worker(Arc::clone(&core), ReaderTarget::File(file)) {
        return Err(fail_transport_worker_start(py, &core, err));
    }
    if let Err(err) =
        spawn_writer_worker(Arc::clone(&core), WriterTarget::Sink(io::sink()), writer_rx)
    {
        return Err(fail_transport_worker_start(py, &core, err));
    }
    Ok(transport)
}

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
pub fn spawn_write_pipe_transport(
    py: Python<'_>,
    spawn_context: TransportSpawnContext,
    pipe_obj: Py<PyAny>,
    extra_entries: Option<HashMap<String, Py<PyAny>>>,
) -> PyResult<Py<PyStreamTransport>> {
    let file = pipe_file_from_obj(py, &pipe_obj)?;
    let (core, transport, writer_rx) = pipe_transport_core(
        py,
        spawn_context,
        pipe_extra(py, &pipe_obj, extra_entries),
        PipeTransportMode::Write,
    )?;
    core.connection_made(transport.clone_ref(py))?;
    if let Err(err) = spawn_writer_worker(Arc::clone(&core), WriterTarget::File(file), writer_rx) {
        return Err(fail_transport_worker_start(py, &core, err));
    }
    Ok(transport)
}

pub(super) enum PipeTransportMode {
    Read,
    Write,
}

impl PipeTransportMode {
    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "PipeTransportMode")
    )]
    pub(super) fn reading(&self) -> bool {
        matches!(self, Self::Read)
    }

    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "PipeTransportMode")
    )]
    pub(super) fn writable(&self) -> bool {
        matches!(self, Self::Write)
    }
}

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
pub(super) fn pipe_transport_core(
    py: Python<'_>,
    spawn_context: TransportSpawnContext,
    extra: HashMap<String, Py<PyAny>>,
    mode: PipeTransportMode,
) -> PyResult<(
    Arc<StreamTransportCore>,
    Py<PyStreamTransport>,
    WriterReceiver,
)> {
    let callbacks = build_protocol_callbacks(py, &spawn_context.protocol)?;
    let (writer_tx, writer_rx) = writer_channel();
    let reading = mode.reading();
    let writable = mode.writable();
    let parts = stream_transport_state_parts(
        spawn_context,
        callbacks,
        StreamTransportStateConfig {
            io_fd: None,
            runtime_socket_io: false,
            extra,
            lazy_socket_family: None,
            reading,
            writable,
            can_write_eof: writable,
            close_on_write_eof: writable,
            server: None,
        },
    );

    let core = new_stream_transport_core(parts, writer_tx, None, None);
    let transport = new_py_stream_transport(py, &core)?;
    Ok((core, transport, writer_rx))
}

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
pub(super) fn pipe_extra(
    py: Python<'_>,
    pipe_obj: &Py<PyAny>,
    extra_entries: Option<HashMap<String, Py<PyAny>>>,
) -> HashMap<String, Py<PyAny>> {
    let mut extra = HashMap::with_capacity(2 + extra_entries.as_ref().map_or(0, HashMap::len));
    extra.insert("pipe".to_owned(), pipe_obj.clone_ref(py));
    extra.insert("file".to_owned(), pipe_obj.clone_ref(py));
    if let Some(extra_entries) = extra_entries {
        extra.extend(extra_entries);
    }
    extra
}

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
#[cfg(not(windows))]
pub(super) fn pipe_file_from_obj(py: Python<'_>, pipe_obj: &Py<PyAny>) -> PyResult<fs::File> {
    let fd = fd_ops::fileobj_to_fd(py, pipe_obj.bind(py))?;
    let dup = fd_ops::dup_raw_fd(fd).map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
    from_owned_raw_fd(dup)
}

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
#[cfg(windows)]
pub(super) fn pipe_file_from_obj(py: Python<'_>, pipe_obj: &Py<PyAny>) -> PyResult<fs::File> {
    let fd = fd_ops::fileobj_to_fd(py, pipe_obj.bind(py))?;
    let handle = fd_ops::duplicate_handle_from_fd(fd)
        .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
    Ok(from_owned_raw_handle(handle.cast()))
}
