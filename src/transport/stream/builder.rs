//! Assembling a `StreamTransportCore` from its spawned pieces.
//!
//! Every transport constructor — socket, pipe, TLS — funnels through here, so
//! the wide struct literals for `StreamTransportState` and
//! `StreamTransportCore` exist once rather than per flavour. Callers describe
//! their differences with `StreamTransportStateConfig` and pass the writer
//! channel and any direct/lazy writer they were able to build.
//!
//! `make_stream_extra` produces the `socket`/`sockname`/`peername` entries that
//! `get_extra_info` returns, from a dup of the transport's descriptor.
//!
//! `fail_transport_worker_start` is the unwind path: a core that exists but
//! whose workers could not start must still deliver `connection_lost` to the
//! protocol before the error reaches Python.

use std::collections::{HashMap, VecDeque};
use std::io;
use std::net::TcpStream as StdTcpStream;
use std::os::raw::c_int;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize};
use std::sync::{Arc, Condvar, Mutex, Weak};

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::PyDict;
use pyo3_async_runtimes::TaskLocals;

use super::buffers::{ReadBufferPool, WriteBufferPool};
use super::io_targets::{LazyWriterConfig, TaskedDirectWriter};
use super::protocol::{ProtocolCallbacks, StreamReaderFastPath};
use super::write_queue::WriterSender;
use super::{
    PyStreamTransport, READ_EVENT_OPEN, ServerCore, StreamTransportCore, StreamTransportState,
    StreamWriteBufferState, TransportSpawnContext,
};
use crate::async_event::AsyncEvent;
use crate::engine::LoopCore;
use crate::fd_ops;

pub fn task_locals_for_loop(py: Python<'_>, loop_obj: &Py<PyAny>) -> PyResult<TaskLocals> {
    TaskLocals::new(loop_obj.clone_ref(py).into_bound(py)).copy_context(py)
}

#[inline]
pub(super) fn detached_socket_handle(
    py: Python<'_>,
    socket_obj: &Py<PyAny>,
) -> PyResult<fd_ops::RawFd> {
    socket_obj.call_method0(py, "detach")?.extract(py)
}

pub(super) fn tcp_family(stream: &StdTcpStream) -> c_int {
    #[cfg(windows)]
    use windows_sys::Win32::Networking::WinSock::{AF_INET, AF_INET6};

    match stream.local_addr() {
        #[cfg(unix)]
        Ok(addr) if addr.is_ipv6() => libc::AF_INET6,
        #[cfg(unix)]
        _ => libc::AF_INET,
        #[cfg(windows)]
        Ok(addr) if addr.is_ipv6() => c_int::from(AF_INET6),
        #[cfg(windows)]
        _ => c_int::from(AF_INET),
    }
}
pub(super) struct StreamTransportStateConfig {
    pub(super) io_fd: Option<fd_ops::RawFd>,
    pub(super) runtime_socket_io: bool,
    pub(super) extra: HashMap<String, Py<PyAny>>,
    pub(super) lazy_socket_family: Option<i32>,
    pub(super) reading: bool,
    pub(super) writable: bool,
    pub(super) can_write_eof: bool,
    pub(super) close_on_write_eof: bool,
    pub(super) server: Option<Weak<ServerCore>>,
}

pub(super) struct StreamTransportBuildParts {
    pub(super) loop_core: Arc<LoopCore>,
    pub(super) loop_obj: Py<PyAny>,
    pub(super) state: StreamTransportState,
}

pub(super) fn stream_transport_state_parts(
    spawn_context: TransportSpawnContext,
    callbacks: ProtocolCallbacks,
    config: StreamTransportStateConfig,
) -> StreamTransportBuildParts {
    let TransportSpawnContext {
        loop_core,
        loop_obj,
        protocol,
        context,
        context_needs_run,
    } = spawn_context;

    StreamTransportBuildParts {
        loop_core,
        loop_obj,
        state: StreamTransportState {
            io_fd: config.io_fd,
            runtime_socket_io: config.runtime_socket_io,
            protocol,
            callbacks,
            context,
            context_needs_run,
            extra: config.extra,
            lazy_socket_family: config.lazy_socket_family,
            closing: false,
            read_paused: false,
            read_backpressured: false,
            reading: config.reading,
            writable: config.writable,
            write_eof_requested: false,
            can_write_eof: config.can_write_eof,
            close_on_write_eof: config.close_on_write_eof,
            lost_called: false,
            writer_registered: false,
            write_buffer: StreamWriteBufferState::default(),
            server: config.server,
        },
    }
}

pub(super) fn new_stream_transport_core(
    parts: StreamTransportBuildParts,
    writer_tx: WriterSender,
    direct_writer: Option<TaskedDirectWriter>,
    lazy_writer: Option<LazyWriterConfig>,
) -> Arc<StreamTransportCore> {
    let has_text_encoding = parts.state.extra.contains_key("text_encoding");
    let server_side = parts.state.server.is_some();
    let native_stream_reader = matches!(
        &parts.state.callbacks.stream_reader_fast_path,
        Some(StreamReaderFastPath::Native { .. })
    );
    // Batching modest writes joins protocol headers and bodies and releases
    // groups of replies together. Coordination-thread readers then need fewer
    // wakeups; local protocol readers also measured better with batching.
    //
    // That trade needs the loop thread to be the scarce resource, which is the
    // case for callback protocols (websockets, aiohttp, ASGI servers), where
    // each message costs real Python work. Native fast streams are the opposite
    // — their per-message work is a few microseconds of Rust, so their peer
    // reader is usually still awake and the staging copy would be pure
    // overhead. Keep those direct except for server replies, which is where
    // header/body joining applies.
    let coalesce_small_writes = server_side || !native_stream_reader;
    let reading = parts.state.reading;
    Arc::new(StreamTransportCore {
        loop_core: parts.loop_core,
        loop_obj: parts.loop_obj,
        state: Mutex::new(parts.state),
        pending_read_events: Mutex::new(VecDeque::with_capacity(
            super::tuning::READ_BUFFER_POOL_LIMIT + 4,
        )),
        read_event_drain: Mutex::new(VecDeque::with_capacity(
            super::tuning::READ_BUFFER_POOL_LIMIT + 4,
        )),
        read_coalesce_buffer: Mutex::new(Vec::new()),
        read_buffer_pool: Arc::new(ReadBufferPool::new()),
        pending_read_bytes: AtomicUsize::new(0),
        read_events_scheduled: AtomicBool::new(false),
        read_event_state: AtomicU8::new(READ_EVENT_OPEN),
        reading: AtomicBool::new(reading),
        detached: AtomicBool::new(false),
        writer_tx,
        write_buffer_pool: Arc::new(WriteBufferPool::new()),
        direct_writer: direct_writer.map(|writer| Mutex::new(Some(writer))),
        pending_direct_write: Mutex::new(None),
        direct_write_scheduled: AtomicBool::new(false),
        #[cfg(windows)]
        poll_reader_requested: AtomicBool::new(false),
        #[cfg(windows)]
        poll_reader_ready: AtomicBool::new(false),
        lazy_writer: Mutex::new(lazy_writer),
        workers: Mutex::new(Vec::new()),
        state_cv: Condvar::new(),
        read_state_notify: AsyncEvent::new(),
        has_text_encoding,
        #[cfg(windows)]
        server_side,
        coalesce_small_writes,
    })
}

pub(super) fn new_py_stream_transport(
    py: Python<'_>,
    core: &Arc<StreamTransportCore>,
) -> PyResult<Py<PyStreamTransport>> {
    Py::new(
        py,
        PyStreamTransport {
            core: Arc::clone(core),
        },
    )
}
pub(super) fn merge_extra(
    mut base: HashMap<String, Py<PyAny>>,
    extra: HashMap<String, Py<PyAny>>,
) -> HashMap<String, Py<PyAny>> {
    base.extend(extra);
    base
}
pub(super) fn fail_transport_worker_start(
    py: Python<'_>,
    core: &Arc<StreamTransportCore>,
    err: io::Error,
) -> PyErr {
    let message = err.to_string();
    core.set_closing();
    core.abort_workers();
    let _ = core.connection_lost_with_py(py, Some(PyRuntimeError::new_err(message.clone())));
    PyRuntimeError::new_err(message)
}
pub(super) fn make_stream_extra(
    py: Python<'_>,
    fd: fd_ops::RawFd,
    family: i32,
) -> PyResult<HashMap<String, Py<PyAny>>> {
    let socket_fd =
        fd_ops::dup_raw_fd(fd).map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
    let socket_mod = py.import("socket")?;
    let kwargs = PyDict::new(py);
    kwargs.set_item("fileno", socket_fd)?;
    let sock = socket_mod.getattr("socket")?.call(
        (family, socket_mod.getattr("SOCK_STREAM")?, 0),
        Some(&kwargs),
    )?;
    sock.call_method1("setblocking", (false,))?;

    let mut extra = HashMap::with_capacity(3);
    extra.insert("socket".to_owned(), sock.clone().unbind().into_any());
    if let Ok(sockname) = sock.call_method0("getsockname") {
        extra.insert("sockname".to_owned(), sockname.unbind().into_any());
    }
    if let Ok(peername) = sock.call_method0("getpeername") {
        extra.insert("peername".to_owned(), peername.unbind().into_any());
    }
    Ok(extra)
}
