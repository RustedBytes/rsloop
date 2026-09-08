//! Stream, server, socket, pipe, and TLS transport implementation.
//!
//! This file holds the vocabulary the whole transport shares — the two owner
//! types (`StreamTransportCore`, `ServerCore`), the state they guard, the
//! commands and events passed between threads, and the two pyclasses Python
//! sees. Everything that *acts* on them lives in a submodule:
//!
//! - construction: [`socket_transport`], [`pipe_transport`], [`tls_transport`],
//!   [`server`], with the shared assembly in [`builder`]
//! - the core's own behaviour, split by concern: [`core_events`],
//!   [`core_protocol`], [`core_state`], [`core_write`], and [`server_core`]
//! - the Python surface: [`py_transport`], [`py_server`], and the native fast
//!   streams in [`fast`]
//! - I/O: [`accept`], [`connect`], [`reader`], [`reader_task`], [`writer`], and
//!   the TLS session plumbing in [`tls_session`]
//!
//! Submodules reach back into the private fields declared here, which is why
//! those stay private: the module tree is the encapsulation boundary, not the
//! individual file.

use std::collections::{HashMap, VecDeque};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicUsize, Ordering};
use std::sync::{Arc, Condvar, Mutex, Weak};

use pyo3::prelude::*;

use super::tls::ServerTlsSettings;
use crate::async_event::AsyncEvent;
use crate::engine::LoopCore;
use crate::fd_ops;

mod accept;
mod buffers;
mod builder;
#[cfg(unix)]
mod connect;
mod core_events;
mod core_protocol;
mod core_state;
mod core_write;
mod fast;
mod io_targets;
mod pipe_transport;
mod platform;
mod poll;
mod protocol;
mod py_server;
mod py_transport;
mod reader;
mod reader_task;
mod server;
mod server_core;
mod server_types;
mod socket_transport;
mod stats;
mod tls_session;
mod tls_transport;
mod tuning;
mod worker;
mod write_queue;
mod writer;

#[cfg(unix)]
use accept::run_unix_accept_loop;
use accept::{BlockingAcceptLoop, run_tcp_accept_loop};
pub(crate) use accept::{run_server_accept_task, spawn_accepted_transport_with_py};
use buffers::{OwnedWriteBuffer, ReadBufferPool, WriteBufferPool};
use builder::make_stream_extra;
pub use builder::task_locals_for_loop;
#[cfg(unix)]
pub(crate) use connect::run_connect_watch_task;
pub use fast::{PyFastStreamReader, PyFastStreamWriter, open_connection, start_server};
pub use io_targets::ReaderTarget;
use io_targets::{LazyWriterConfig, TaskedDirectWriter};
pub use pipe_transport::{spawn_read_pipe_transport, spawn_write_pipe_transport};
use protocol::ProtocolCallbacks;
pub(crate) use reader::run_socket_reader_blocking;
use reader::{
    spawn_reader_worker, spawn_socket_reader, spawn_tls_reader_worker, stop_socket_reader,
    stop_socket_reader_nowait,
};
pub(crate) use reader_task::run_tcp_socket_reader_task;
#[cfg(unix)]
pub(crate) use reader_task::run_unix_socket_reader_task;
pub use server::{create_server, tcp_server_listener};
#[cfg(unix)]
pub use server::{remove_unix_socket_if_present, unix_server_listener};
pub use server_types::{AcceptedStream, ServerCreateParams, ServerListener, TransportSpawnContext};
#[cfg(unix)]
use socket_transport::duplicate_unix_direct_writer;
use socket_transport::{duplicate_configured_tcp_stream, tcp_stream_from_owned_socket_fd};
pub use socket_transport::{
    tcp_listener_from_owned_socket_fd, transport_from_socket, transport_from_socket_server_tls,
    transport_from_socket_tls,
};
#[cfg(unix)]
pub use socket_transport::{unix_listener_from_owned_socket_fd, unix_stream_from_owned_socket_fd};
pub use stats::{reset_transport_stats, transport_stats};
pub use tls_transport::{prepare_start_tls_transport, start_tls_transport};
use tuning::{
    DEFAULT_WRITE_BUFFER_HIGH_WATER, DEFAULT_WRITE_BUFFER_LOW_WATER, max_pending_tls_handshakes,
};
use worker::WorkerThread;
use writer::{spawn_tls_writer_worker, spawn_writer_worker};

enum WriterCommand {
    Data(OwnedWriteBuffer),
    WriteEof,
    Close,
    Abort,
    Stop,
}

enum PendingReadEvent {
    Data(Vec<u8>),
    Eof,
    ConnectionLost(Option<String>),
    PauseWriting,
    ResumeWriting,
}

const READ_EVENT_OPEN: u8 = 0;
const READ_EVENT_EOF: u8 = 1;
const READ_EVENT_LOST: u8 = 2;

struct StreamTransportState {
    io_fd: Option<fd_ops::RawFd>,
    runtime_socket_io: bool,
    protocol: Py<PyAny>,
    callbacks: ProtocolCallbacks,
    context: Py<PyAny>,
    context_needs_run: bool,
    extra: HashMap<String, Py<PyAny>>,
    lazy_socket_family: Option<i32>,
    closing: bool,
    read_paused: bool,
    read_backpressured: bool,
    reading: bool,
    writable: bool,
    write_eof_requested: bool,
    can_write_eof: bool,
    close_on_write_eof: bool,
    lost_called: bool,
    writer_registered: bool,
    write_buffer: StreamWriteBufferState,
    server: Option<Weak<ServerCore>>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct StreamWriteBufferState {
    size: usize,
    high_water: usize,
    low_water: usize,
    protocol_paused: bool,
}

impl Default for StreamWriteBufferState {
    fn default() -> Self {
        Self {
            size: 0,
            high_water: DEFAULT_WRITE_BUFFER_HIGH_WATER,
            low_water: DEFAULT_WRITE_BUFFER_LOW_WATER,
            protocol_paused: false,
        }
    }
}

/// Shared stream owner. I/O paths enqueue events; the loop thread drains them
/// and invokes the Python protocol in order.
pub struct StreamTransportCore {
    loop_core: Arc<LoopCore>,
    loop_obj: Py<PyAny>,
    state: Mutex<StreamTransportState>,
    pending_read_events: Mutex<VecDeque<PendingReadEvent>>,
    read_event_drain: Mutex<VecDeque<PendingReadEvent>>,
    read_coalesce_buffer: Mutex<Vec<u8>>,
    read_buffer_pool: Arc<ReadBufferPool>,
    pending_read_bytes: AtomicUsize,
    read_events_scheduled: AtomicBool,
    read_event_state: AtomicU8,
    reading: AtomicBool,
    detached: AtomicBool,
    writer_tx: write_queue::WriterSender,
    write_buffer_pool: Arc<WriteBufferPool>,
    direct_writer: Option<Mutex<Option<TaskedDirectWriter>>>,
    pending_direct_write: Mutex<Option<OwnedWriteBuffer>>,
    direct_write_scheduled: AtomicBool,
    #[cfg(windows)]
    poll_reader_requested: AtomicBool,
    #[cfg(windows)]
    poll_reader_ready: AtomicBool,
    lazy_writer: Mutex<Option<LazyWriterConfig>>,
    workers: Mutex<Vec<WorkerThread>>,
    // Signaled whenever `read_paused` clears or the transport starts
    // closing, so a paused reader worker resumes immediately instead of
    // sleeping through a poll interval.
    state_cv: Condvar,
    read_state_notify: AsyncEvent,
    // The extra map is fixed at construction; cache the text-mode marker so
    // the per-write hot path avoids a state lock plus hash lookup.
    has_text_encoding: bool,
    #[cfg(windows)]
    server_side: bool,
    coalesce_small_writes: bool,
}

struct ServerState {
    closed: bool,
    serving: bool,
    listeners: Vec<ServerListener>,
}

/// Shared server owner for listeners, accept workers, and active connections.
pub struct ServerCore {
    loop_core: Arc<LoopCore>,
    loop_obj: Py<PyAny>,
    protocol_factory: Py<PyAny>,
    context: Py<PyAny>,
    context_needs_run: bool,
    sockets: Vec<Py<PyAny>>,
    state: Mutex<ServerState>,
    accept_tasks: Mutex<Vec<WorkerThread>>,
    accept_fds: Mutex<Vec<fd_ops::RawFd>>,
    active_connections: AtomicUsize,
    pending_tls_handshakes: AtomicUsize,
    tls_overload_reported: AtomicBool,
    closed_notify: AsyncEvent,
    #[cfg_attr(not(unix), allow(dead_code))]
    cleanup_path: Option<PathBuf>,
    tls: Option<Arc<ServerTlsSettings>>,
}

struct PendingTlsHandshake {
    server: Arc<ServerCore>,
}

impl Drop for PendingTlsHandshake {
    fn drop(&mut self) {
        let previous = self
            .server
            .pending_tls_handshakes
            .fetch_sub(1, Ordering::AcqRel);
        if previous.saturating_sub(1) < max_pending_tls_handshakes() / 2 {
            self.server
                .tls_overload_reported
                .store(false, Ordering::Release);
        }
        self.server.closed_notify.notify_all();
    }
}

/// Python-visible owner of listening sockets and accept tasks.
///
/// Closing a server stops new accepts; `wait_closed()` additionally waits for
/// accept workers and active connections to finish.
#[pyclass(name = "Server", module = "rsloop._loop")]
pub struct PyServer {
    /// Shared listeners, accept workers, and connection counters.
    pub core: Arc<ServerCore>,
}

/// Python-visible asyncio transport for a stream socket or pipe.
#[pyclass(name = "StreamTransport", module = "rsloop._loop")]
pub struct PyStreamTransport {
    /// Shared ordered event queues, protocol state, and I/O workers.
    pub core: Arc<StreamTransportCore>,
}

#[cfg(test)]
pub(super) mod test_support {
    use std::collections::HashMap;
    use std::sync::Arc;

    use pyo3::exceptions::PyRuntimeError;
    use pyo3::prelude::*;
    use pyo3::types::{PyBytes, PyDict};

    use super::builder::{
        StreamTransportStateConfig, new_stream_transport_core, stream_transport_state_parts,
    };
    use super::protocol::build_protocol_callbacks;
    use super::write_queue::{WriterReceiver, channel as writer_channel};
    use super::{StreamTransportCore, TransportSpawnContext};
    use crate::engine::LoopCore;

    #[pyclass]
    #[derive(Default)]
    pub(super) struct RecordingProtocol {
        pub(super) events: Vec<&'static str>,
        pub(super) received: Vec<Vec<u8>>,
        pub(super) pause_attempts: usize,
        pub(super) panic_next_pause: bool,
        pub(super) fail_next_data: bool,
        pub(super) keep_open_at_eof: bool,
    }

    #[pymethods]
    impl RecordingProtocol {
        fn connection_made(&mut self, _transport: Py<PyAny>) {
            self.events.push("made");
        }

        fn data_received(&mut self, data: &Bound<'_, PyBytes>) -> PyResult<()> {
            if std::mem::take(&mut self.fail_next_data) {
                return Err(PyRuntimeError::new_err("intentional data_received failure"));
            }
            self.received.push(data.as_bytes().to_vec());
            self.events.push("data");
            Ok(())
        }

        fn eof_received(&mut self) -> bool {
            self.events.push("eof");
            self.keep_open_at_eof
        }

        fn connection_lost(&mut self, _error: Py<PyAny>) {
            self.events.push("lost");
        }

        fn pause_writing(&mut self) {
            self.pause_attempts += 1;
            assert!(
                !std::mem::take(&mut self.panic_next_pause),
                "intentional pause_writing panic"
            );
            self.events.push("pause");
        }

        fn resume_writing(&mut self) {
            self.events.push("resume");
        }
    }

    #[pyclass]
    #[derive(Default)]
    pub(super) struct RecordingExceptionHandler {
        pub(super) messages: Vec<String>,
        pub(super) exception_types: Vec<String>,
    }

    #[pymethods]
    impl RecordingExceptionHandler {
        fn __call__(&mut self, _loop_obj: Py<PyAny>, context: &Bound<'_, PyDict>) -> PyResult<()> {
            let message = context
                .get_item("message")?
                .expect("exception context message")
                .extract::<String>()?;
            let exception = context
                .get_item("exception")?
                .expect("exception context value");
            let exception_type = exception.get_type().name()?.to_string_lossy().into_owned();
            self.messages.push(message);
            self.exception_types.push(exception_type);
            Ok(())
        }
    }

    pub(super) fn install_exception_handler(
        py: Python<'_>,
        loop_core: &LoopCore,
    ) -> Py<RecordingExceptionHandler> {
        let handler =
            Py::new(py, RecordingExceptionHandler::default()).expect("recording exception handler");
        loop_core
            .state
            .lock()
            .expect("loop state")
            .exception_handler = Some(handler.clone_ref(py).into_any());
        handler
    }

    pub(super) fn build_test_core(
        py: Python<'_>,
    ) -> (
        Arc<StreamTransportCore>,
        WriterReceiver,
        Arc<LoopCore>,
        Py<RecordingProtocol>,
    ) {
        let loop_core = LoopCore::new();
        let protocol = Py::new(py, RecordingProtocol::default()).expect("recording protocol");
        let protocol_any = protocol.clone_ref(py).into_any();
        let callbacks =
            build_protocol_callbacks(py, &protocol_any).expect("recording protocol callbacks");
        let parts = stream_transport_state_parts(
            TransportSpawnContext {
                loop_core: Arc::clone(&loop_core),
                loop_obj: py.None(),
                protocol: protocol_any,
                context: py.None(),
                context_needs_run: false,
            },
            callbacks,
            StreamTransportStateConfig {
                io_fd: None,
                runtime_socket_io: false,
                extra: HashMap::new(),
                lazy_socket_family: None,
                reading: false,
                writable: true,
                can_write_eof: false,
                close_on_write_eof: false,
                server: None,
            },
        );
        let (writer_tx, writer_rx) = writer_channel();
        let core = new_stream_transport_core(parts, writer_tx, None, None);
        loop_core.mark_runtime_thread();
        (core, writer_rx, loop_core, protocol)
    }

    pub(super) fn shutdown_test_core(
        core: Arc<StreamTransportCore>,
        writer_rx: WriterReceiver,
        loop_core: Arc<LoopCore>,
    ) {
        loop_core.clear_runtime_thread();
        drop(core);
        drop(writer_rx);
        loop_core.close().expect("close test loop");
    }
}
