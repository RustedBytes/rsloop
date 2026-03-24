use std::collections::{HashMap, VecDeque};
use std::fs;
use std::io::{self, Read, Write as _};
use std::net::{Shutdown, TcpListener as StdTcpListener, TcpStream as StdTcpStream};
#[cfg(target_os = "linux")]
use std::os::fd::OwnedFd;
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::raw::c_int;
#[cfg(unix)]
use std::os::unix::net::{UnixListener as StdUnixListener, UnixStream as StdUnixStream};
#[cfg(windows)]
use std::os::windows::io::{
    AsRawHandle, AsRawSocket, FromRawHandle, FromRawSocket, IntoRawSocket, RawSocket,
};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::sync::{Arc, Mutex, Weak};
use std::thread;
use std::time::Duration;

#[cfg(target_os = "linux")]
use compio::net::PollFd;
#[cfg(target_os = "linux")]
use compio::time::sleep as compio_sleep;
use pyo3::exceptions::{PyRuntimeError, PyTimeoutError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::{PyByteArray, PyByteArrayMethods, PyBytes, PyDict, PySlice, PyString, PyTuple};
use pyo3_async_runtimes::TaskLocals;
use rustls::{ClientConnection, ServerConnection};
use socket2::Socket;
#[cfg(windows)]
use tokio::io::AsyncReadExt;
#[cfg(windows)]
use vibeio::net::{PollTcpStream as VibePollTcpStream, TcpListener as VibeTcpListener};

use crate::async_event::AsyncEvent;
use crate::context::{
    ensure_running_loop, run_in_context, run_in_context_noargs, run_in_context_onearg,
};
use crate::fast_streams::{PyFastStreamProtocol, PyFastStreamReader};
use crate::fd_ops;
use crate::loop_core::{LoopCommand, LoopCore};
use crate::python_names;
use crate::tls::{tls_extra, ClientTlsSettings, ServerTlsSettings};

enum WriterCommand {
    Data(OwnedWriteBuffer),
    WriteEof,
    Close,
    Abort,
    Stop,
}

enum PendingReadEvent {
    Data(Box<[u8]>),
    Eof,
    ConnectionLost(Option<String>),
    PauseWriting,
    ResumeWriting,
}

const DEFAULT_WRITE_BUFFER_HIGH_WATER: usize = 64 * 1024;
const DEFAULT_WRITE_BUFFER_LOW_WATER: usize = DEFAULT_WRITE_BUFFER_HIGH_WATER / 4;
const MAX_PENDING_READ_COALESCE_BYTES: usize = 256 * 1024;

struct OwnedWriteBuffer {
    bytes: Box<[u8]>,
    offset: usize,
}

enum PendingReadBuffer {
    Boxed(Box<[u8]>),
    Vec(Vec<u8>),
}

impl PendingReadBuffer {
    #[inline]
    fn len(&self) -> usize {
        match self {
            Self::Boxed(data) => data.len(),
            Self::Vec(data) => data.len(),
        }
    }

    #[inline]
    fn as_slice(&self) -> &[u8] {
        match self {
            Self::Boxed(data) => data,
            Self::Vec(data) => data,
        }
    }

    fn extend(&mut self, data: Box<[u8]>) {
        match self {
            Self::Boxed(existing) => {
                let mut combined = Vec::with_capacity(existing.len() + data.len());
                combined.extend_from_slice(existing);
                combined.extend_from_slice(&data);
                *self = Self::Vec(combined);
            }
            Self::Vec(existing) => existing.extend_from_slice(&data),
        }
    }
}

impl OwnedWriteBuffer {
    #[inline]
    fn from_slice(data: &[u8]) -> Self {
        Self {
            bytes: Box::<[u8]>::from(data),
            offset: 0,
        }
    }

    #[inline]
    fn remaining(&self) -> &[u8] {
        &self.bytes[self.offset..]
    }

    #[inline]
    fn advance(&mut self, written: usize) {
        self.offset += written;
    }

    #[inline]
    fn is_empty(&self) -> bool {
        self.offset == self.bytes.len()
    }
}

pub enum ServerListener {
    Tcp(StdTcpListener),
    #[cfg(unix)]
    Unix(StdUnixListener),
}

pub struct TransportSpawnContext {
    pub loop_core: Arc<LoopCore>,
    pub loop_obj: Py<PyAny>,
    pub protocol: Py<PyAny>,
    pub context: Py<PyAny>,
    pub context_needs_run: bool,
}

pub struct ServerCreateParams {
    pub loop_core: Arc<LoopCore>,
    pub loop_obj: Py<PyAny>,
    pub protocol_factory: Py<PyAny>,
    pub context: Py<PyAny>,
    pub context_needs_run: bool,
    pub sockets: Vec<Py<PyAny>>,
    pub listeners: Vec<ServerListener>,
    pub cleanup_path: Option<PathBuf>,
    pub tls: Option<Arc<ServerTlsSettings>>,
}

struct ProtocolCallbacks {
    connection_made: Py<PyAny>,
    data_received: Option<Py<PyAny>>,
    eof_received: Option<Py<PyAny>>,
    connection_lost: Py<PyAny>,
    pause_writing: Py<PyAny>,
    resume_writing: Py<PyAny>,
    get_buffer: Option<Py<PyAny>>,
    buffer_updated: Option<Py<PyAny>>,
    stream_reader_fast_path: Option<StreamReaderFastPath>,
}

enum StreamReaderFastPath {
    Native {
        protocol: Py<PyFastStreamProtocol>,
        reader: Py<PyFastStreamReader>,
    },
    Generic {
        protocol: Option<Py<PyAny>>,
        reader: Py<PyAny>,
        buffer: Py<PyAny>,
        limit: usize,
    },
}

impl StreamReaderFastPath {
    fn clone_ref(&self, py: Python<'_>) -> Self {
        match self {
            Self::Native { protocol, reader } => Self::Native {
                protocol: protocol.clone_ref(py),
                reader: reader.clone_ref(py),
            },
            Self::Generic {
                protocol,
                reader,
                buffer,
                limit,
            } => Self::Generic {
                protocol: protocol.as_ref().map(|value| value.clone_ref(py)),
                reader: reader.clone_ref(py),
                buffer: buffer.clone_ref(py),
                limit: *limit,
            },
        }
    }

    fn connection_made(&self, py: Python<'_>, transport: Py<PyStreamTransport>) -> PyResult<bool> {
        match self {
            Self::Native { protocol, .. } => {
                PyFastStreamProtocol::handle_connection_made(
                    protocol.clone_ref(py),
                    py,
                    transport.into_any(),
                )?;
                Ok(true)
            }
            Self::Generic {
                protocol, reader, ..
            } => {
                let has_client_connected_cb = protocol.as_ref().is_some_and(|protocol| {
                    protocol
                        .bind(py)
                        .getattr("_client_connected_cb")
                        .map(|value| !value.is_none())
                        .unwrap_or(true)
                });
                if has_client_connected_cb {
                    return Ok(false);
                }

                reader
                    .bind(py)
                    .setattr("_transport", transport.clone_ref(py).into_any())?;
                if let Some(protocol) = protocol.as_ref() {
                    protocol
                        .bind(py)
                        .setattr("_transport", transport.into_any())?;
                }
                Ok(true)
            }
        }
    }

    fn feed_data(&self, py: Python<'_>, data: &[u8]) -> PyResult<()> {
        match self {
            Self::Native { reader, .. } => reader.borrow_mut(py).feed_data_internal(py, data),
            Self::Generic {
                reader,
                buffer,
                limit,
                ..
            } => {
                if data.is_empty() {
                    return Ok(());
                }

                let reader = reader.bind(py);
                let buffer = buffer.bind(py).cast::<PyByteArray>()?;
                if reader.getattr("_eof")?.extract::<bool>()? {
                    return Err(PyRuntimeError::new_err("feed_data after feed_eof"));
                }

                let start = buffer.len();
                let end = start + data.len();
                buffer.resize(end)?;
                unsafe {
                    buffer.as_bytes_mut()[start..end].copy_from_slice(data);
                }

                let waiter = reader.getattr("_waiter")?;
                if !waiter.is_none() {
                    reader.setattr("_waiter", py.None())?;
                    if !waiter.call_method0("cancelled")?.extract::<bool>()? {
                        waiter.call_method1("set_result", (py.None(),))?;
                    }
                }

                let transport = reader.getattr("_transport")?;
                let paused = reader.getattr("_paused")?.extract::<bool>()?;
                if !transport.is_none() && !paused && end > 2 * limit {
                    match transport.call_method0(python_names::pause_reading(py)) {
                        Ok(_) => {
                            reader.setattr("_paused", true)?;
                        }
                        Err(err)
                            if err
                                .is_instance_of::<pyo3::exceptions::PyNotImplementedError>(py) =>
                        {
                            reader.setattr("_transport", py.None())?;
                        }
                        Err(err) => return Err(err),
                    }
                }

                Ok(())
            }
        }
    }

    fn feed_eof(&self, py: Python<'_>) -> PyResult<()> {
        match self {
            Self::Native { reader, .. } => reader.borrow_mut(py).feed_eof_internal(py),
            Self::Generic { reader, .. } => {
                let reader = reader.bind(py);
                reader.setattr("_eof", true)?;
                let waiter = reader.getattr("_waiter")?;
                if !waiter.is_none() {
                    reader.setattr("_waiter", py.None())?;
                    if !waiter.call_method0("cancelled")?.extract::<bool>()? {
                        waiter.call_method1("set_result", (py.None(),))?;
                    }
                }
                Ok(())
            }
        }
    }

    fn connection_lost(&self, py: Python<'_>, exc: Option<PyErr>) -> PyResult<()> {
        match self {
            Self::Native { protocol, .. } => protocol.borrow_mut(py).handle_connection_lost(
                py,
                exc.map(|err| err.value(py).clone().unbind().into_any()),
            ),
            Self::Generic {
                protocol, reader, ..
            } => {
                let Some(protocol) = protocol.as_ref() else {
                    return Ok(());
                };

                let protocol = protocol.bind(py);
                protocol.setattr("_connection_lost", true)?;

                match exc {
                    Some(err) => {
                        let err_value = err.value(py).clone().unbind().into_any();
                        reader
                            .bind(py)
                            .setattr("_exception", err_value.clone_ref(py))?;
                        let waiter = reader.bind(py).getattr("_waiter")?;
                        if !waiter.is_none() {
                            reader.bind(py).setattr("_waiter", py.None())?;
                            if !waiter.call_method0("cancelled")?.extract::<bool>()? {
                                waiter.call_method1("set_exception", (err_value.clone_ref(py),))?;
                            }
                        }
                        let closed = protocol.getattr("_closed")?;
                        if !closed.call_method0("done")?.extract::<bool>()? {
                            closed.call_method1("set_exception", (err_value.clone_ref(py),))?;
                        }
                        if protocol.getattr("_paused")?.extract::<bool>()? {
                            let waiters = protocol.getattr("_drain_waiters")?;
                            for waiter in waiters.try_iter()? {
                                let waiter = waiter?;
                                if !waiter.call_method0("done")?.extract::<bool>()? {
                                    waiter.call_method1(
                                        "set_exception",
                                        (err_value.clone_ref(py),),
                                    )?;
                                }
                            }
                        }
                    }
                    None => {
                        self.feed_eof(py)?;
                        let closed = protocol.getattr("_closed")?;
                        if !closed.call_method0("done")?.extract::<bool>()? {
                            closed.call_method1("set_result", (py.None(),))?;
                        }
                        if protocol.getattr("_paused")?.extract::<bool>()? {
                            let waiters = protocol.getattr("_drain_waiters")?;
                            for waiter in waiters.try_iter()? {
                                let waiter = waiter?;
                                if !waiter.call_method0("done")?.extract::<bool>()? {
                                    waiter.call_method1("set_result", (py.None(),))?;
                                }
                            }
                        }
                    }
                }

                protocol.setattr("_transport", py.None())?;
                protocol.setattr("_task", py.None())?;
                Ok(())
            }
        }
    }

    fn eof_received(&self, py: Python<'_>) -> PyResult<bool> {
        match self {
            Self::Native { reader, .. } => {
                reader.borrow_mut(py).feed_eof_internal(py)?;
                Ok(true)
            }
            Self::Generic { .. } => {
                self.feed_eof(py)?;
                Ok(true)
            }
        }
    }
}

struct StreamTransportState {
    io_fd: Option<fd_ops::RawFd>,
    runtime_socket_io: bool,
    protocol: Py<PyAny>,
    callbacks: ProtocolCallbacks,
    context: Py<PyAny>,
    context_needs_run: bool,
    extra: HashMap<String, Py<PyAny>>,
    closing: bool,
    read_paused: bool,
    reading: bool,
    writable: bool,
    write_eof_requested: bool,
    can_write_eof: bool,
    close_on_write_eof: bool,
    lost_called: bool,
    writer_registered: bool,
    write_buffer_size: usize,
    write_buffer_high_water: usize,
    write_buffer_low_water: usize,
    protocol_write_paused: bool,
    detached: bool,
    server: Option<Weak<ServerCore>>,
}

pub struct StreamTransportCore {
    loop_core: Arc<LoopCore>,
    loop_obj: Py<PyAny>,
    state: Mutex<StreamTransportState>,
    pending_read_events: Mutex<VecDeque<PendingReadEvent>>,
    read_events_scheduled: AtomicBool,
    writer_tx: Sender<WriterCommand>,
    direct_writer: Option<Mutex<TaskedDirectWriter>>,
    lazy_writer: Mutex<Option<LazyWriterConfig>>,
    workers: Mutex<Vec<WorkerThread>>,
}

struct ServerState {
    closed: bool,
    serving: bool,
    listeners: Vec<ServerListener>,
}

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
    closed_notify: AsyncEvent,
    cleanup_path: Option<PathBuf>,
    tls: Option<Arc<ServerTlsSettings>>,
}

#[pyclass(name = "Server", module = "rsloop._loop")]
pub struct PyServer {
    pub core: Arc<ServerCore>,
}

#[pyclass(name = "StreamTransport", module = "rsloop._loop")]
pub struct PyStreamTransport {
    pub core: Arc<StreamTransportCore>,
}

enum TaskedDirectWriter {
    Tcp(StdTcpStream),
    #[cfg(unix)]
    Unix(StdUnixStream),
}

impl TaskedDirectWriter {
    fn shutdown_close(&self) -> io::Result<()> {
        match self {
            Self::Tcp(stream) => shutdown_tcp_stream(stream, Shutdown::Both),
            #[cfg(unix)]
            Self::Unix(stream) => shutdown_unix_stream(stream, Shutdown::Both),
        }
    }
}

pub enum ReaderTarget {
    File(std::fs::File),
    Tcp(StdTcpStream),
    #[cfg(unix)]
    Unix(StdUnixStream),
}

impl ReaderTarget {
    fn fd(&self) -> fd_ops::RawFd {
        match self {
            Self::File(file) => file_raw_fd(file),
            Self::Tcp(stream) => tcp_stream_raw_fd(stream),
            #[cfg(unix)]
            Self::Unix(stream) => unix_raw_fd(stream.as_raw_fd()),
        }
    }

    #[cfg(windows)]
    fn pollable(&self) -> bool {
        !matches!(self, Self::File(_))
    }

    #[cfg(not(windows))]
    fn pollable(&self) -> bool {
        true
    }
}

impl Read for ReaderTarget {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::File(file) => file.read(buf),
            Self::Tcp(stream) => stream.read(buf),
            #[cfg(unix)]
            Self::Unix(stream) => stream.read(buf),
        }
    }
}

enum WriterTarget {
    File(std::fs::File),
    Tcp(StdTcpStream),
    #[cfg(unix)]
    Unix(StdUnixStream),
    Sink(io::Sink),
}

struct LazyWriterConfig {
    target: WriterTarget,
    writer_rx: Receiver<WriterCommand>,
}

impl WriterTarget {
    fn fd(&self) -> Option<fd_ops::RawFd> {
        match self {
            Self::File(file) => Some(file_raw_fd(file)),
            Self::Tcp(stream) => Some(tcp_stream_raw_fd(stream)),
            #[cfg(unix)]
            Self::Unix(stream) => Some(unix_raw_fd(stream.as_raw_fd())),
            Self::Sink(_) => None,
        }
    }

    #[cfg(windows)]
    fn pollable(&self) -> bool {
        !matches!(self, Self::File(_))
    }

    #[cfg(not(windows))]
    fn pollable(&self) -> bool {
        true
    }

    fn shutdown_write(&self) -> io::Result<()> {
        match self {
            Self::Tcp(stream) => shutdown_tcp_stream(stream, Shutdown::Write),
            #[cfg(unix)]
            Self::Unix(stream) => shutdown_unix_stream(stream, Shutdown::Write),
            Self::File(_) | Self::Sink(_) => Ok(()),
        }
    }

    fn shutdown_close(&self) -> io::Result<()> {
        match self {
            Self::Tcp(stream) => shutdown_tcp_stream(stream, Shutdown::Both),
            #[cfg(unix)]
            Self::Unix(stream) => shutdown_unix_stream(stream, Shutdown::Both),
            Self::File(_) | Self::Sink(_) => Ok(()),
        }
    }
}

enum StreamKind {
    Tcp(StdTcpStream),
    #[cfg(unix)]
    Unix(StdUnixStream),
}

impl StreamKind {
    fn fd(&self) -> fd_ops::RawFd {
        match self {
            Self::Tcp(stream) => tcp_stream_raw_fd(stream),
            #[cfg(unix)]
            Self::Unix(stream) => unix_raw_fd(stream.as_raw_fd()),
        }
    }

    #[cfg(windows)]
    fn pollable(&self) -> bool {
        true
    }

    #[cfg(not(windows))]
    fn pollable(&self) -> bool {
        true
    }

    fn shutdown_close(&self) -> io::Result<()> {
        match self {
            Self::Tcp(stream) => shutdown_tcp_stream(stream, Shutdown::Both),
            #[cfg(unix)]
            Self::Unix(stream) => shutdown_unix_stream(stream, Shutdown::Both),
        }
    }
}

impl Read for StreamKind {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Tcp(stream) => stream.read(buf),
            #[cfg(unix)]
            Self::Unix(stream) => stream.read(buf),
        }
    }
}

impl io::Write for StreamKind {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Tcp(stream) => stream.write(buf),
            #[cfg(unix)]
            Self::Unix(stream) => stream.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Tcp(stream) => stream.flush(),
            #[cfg(unix)]
            Self::Unix(stream) => stream.flush(),
        }
    }
}

enum TlsConnectionKind {
    Client(ClientConnection),
    Server(ServerConnection),
}

impl TlsConnectionKind {
    fn is_handshaking(&self) -> bool {
        match self {
            Self::Client(conn) => conn.is_handshaking(),
            Self::Server(conn) => conn.is_handshaking(),
        }
    }

    fn wants_read(&self) -> bool {
        match self {
            Self::Client(conn) => conn.wants_read(),
            Self::Server(conn) => conn.wants_read(),
        }
    }

    fn wants_write(&self) -> bool {
        match self {
            Self::Client(conn) => conn.wants_write(),
            Self::Server(conn) => conn.wants_write(),
        }
    }

    fn read_tls(&mut self, stream: &mut StreamKind) -> io::Result<usize> {
        match self {
            Self::Client(conn) => conn.read_tls(stream),
            Self::Server(conn) => conn.read_tls(stream),
        }
    }

    fn write_tls(&mut self, stream: &mut StreamKind) -> io::Result<usize> {
        match self {
            Self::Client(conn) => conn.write_tls(stream),
            Self::Server(conn) => conn.write_tls(stream),
        }
    }

    fn process_new_packets(&mut self) -> Result<(), rustls::Error> {
        match self {
            Self::Client(conn) => conn.process_new_packets().map(|_| ()),
            Self::Server(conn) => conn.process_new_packets().map(|_| ()),
        }
    }

    fn reader_read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Client(conn) => conn.reader().read(buf),
            Self::Server(conn) => conn.reader().read(buf),
        }
    }

    fn writer_write_all(&mut self, data: &[u8]) -> io::Result<()> {
        match self {
            Self::Client(conn) => conn.writer().write_all(data),
            Self::Server(conn) => conn.writer().write_all(data),
        }
    }

    fn send_close_notify(&mut self) {
        match self {
            Self::Client(conn) => conn.send_close_notify(),
            Self::Server(conn) => conn.send_close_notify(),
        }
    }
}

struct TlsIoState {
    stream: StreamKind,
    connection: TlsConnectionKind,
    shutdown_timeout: Duration,
}

impl TlsIoState {
    fn fd(&self) -> fd_ops::RawFd {
        self.stream.fd()
    }

    fn pollable(&self) -> bool {
        self.stream.pollable()
    }

    fn shutdown_close(&self) -> io::Result<()> {
        self.stream.shutdown_close()
    }

    fn read_tls(&mut self) -> io::Result<usize> {
        self.connection.read_tls(&mut self.stream)
    }

    fn write_tls(&mut self) -> io::Result<usize> {
        self.connection.write_tls(&mut self.stream)
    }
}

impl io::Write for WriterTarget {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::File(file) => file.write(buf),
            Self::Tcp(stream) => stream.write(buf),
            #[cfg(unix)]
            Self::Unix(stream) => stream.write(buf),
            Self::Sink(sink) => sink.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::File(file) => file.flush(),
            Self::Tcp(stream) => stream.flush(),
            #[cfg(unix)]
            Self::Unix(stream) => stream.flush(),
            Self::Sink(sink) => sink.flush(),
        }
    }
}

struct WorkerThread {
    stop: Arc<AtomicBool>,
    join: thread::JoinHandle<()>,
}

impl WorkerThread {
    fn spawn(name: String, task: impl FnOnce(Arc<AtomicBool>) + Send + 'static) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let join = thread::Builder::new()
            .name(name)
            .spawn(move || task(thread_stop))
            .expect("failed to spawn stream worker");
        Self { stop, join }
    }

    fn abort(self) {
        self.stop.store(true, Ordering::Release);
        let _ = self.join.join();
    }
}

impl StreamTransportCore {
    fn close_extra_socket_with_py(&self, py: Python<'_>) {
        let socket = self
            .state
            .lock()
            .expect("poisoned transport state")
            .extra
            .get("socket")
            .map(|value| value.clone_ref(py));
        if let Some(socket) = socket {
            let _ = socket.bind(py).call_method0("close");
        }
    }

    fn register_worker(&self, worker: WorkerThread) {
        self.workers
            .lock()
            .expect("poisoned transport workers")
            .push(worker);
    }

    fn ensure_writer_worker(self: &Arc<Self>) {
        let lazy = self
            .lazy_writer
            .lock()
            .expect("poisoned lazy writer")
            .take();
        let Some(LazyWriterConfig { target, writer_rx }) = lazy else {
            return;
        };
        spawn_writer_worker(Arc::clone(self), target, writer_rx);
    }

    fn server_ref(&self) -> Option<Weak<ServerCore>> {
        self.state
            .lock()
            .expect("poisoned transport state")
            .server
            .as_ref()
            .cloned()
    }

    fn call_in_loop_context<T>(
        &self,
        f: impl for<'py> FnOnce(Python<'py>) -> PyResult<T>,
    ) -> PyResult<T> {
        Python::attach(|py| {
            if !self.loop_core.on_runtime_thread() {
                ensure_running_loop(py, &self.loop_obj)?;
            }
            f(py)
        })
    }

    fn enqueue_pending_read_event(self: &Arc<Self>, event: PendingReadEvent) {
        profiling::scope!("StreamTransportCore::enqueue_pending_read_event");
        self.pending_read_events
            .lock()
            .expect("poisoned pending read queue")
            .push_back(event);

        if !self.read_events_scheduled.swap(true, Ordering::AcqRel)
            && self
                .loop_core
                .send_command(LoopCommand::ScheduleStreamTransportRead(Arc::clone(self)))
                .is_err()
        {
            self.read_events_scheduled.store(false, Ordering::Release);
        }
    }

    pub(crate) fn drain_pending_read_events_with_py(&self, py: Python<'_>) -> PyResult<()> {
        profiling::scope!("StreamTransportCore::drain_pending_read_events_with_py");
        let mut pending_data: Option<PendingReadBuffer> = None;
        let mut drained = VecDeque::new();
        loop {
            {
                let mut queue = self
                    .pending_read_events
                    .lock()
                    .expect("poisoned pending read queue");
                if queue.is_empty() {
                    self.read_events_scheduled.store(false, Ordering::Release);
                    return Ok(());
                }

                std::mem::swap(&mut drained, &mut *queue);
            }

            while let Some(event) = drained.pop_front() {
                match event {
                    PendingReadEvent::Data(data) => {
                        profiling::scope!("stream.pending.data");
                        match pending_data.as_mut() {
                            Some(buffer)
                                if buffer.len() + data.len() <= MAX_PENDING_READ_COALESCE_BYTES =>
                            {
                                buffer.extend(data);
                            }
                            Some(_) => {
                                if let Err(err) =
                                    self.flush_pending_data_with_py(py, &mut pending_data)
                                {
                                    let _ = self.report_error_with_py(
                                        py,
                                        err,
                                        "stream data_received callback failed",
                                    );
                                    let _ = self.connection_lost_with_py(py, None);
                                    self.read_events_scheduled.store(false, Ordering::Release);
                                    return Ok(());
                                }
                                pending_data = Some(PendingReadBuffer::Boxed(data));
                            }
                            None => pending_data = Some(PendingReadBuffer::Boxed(data)),
                        }
                    }
                    PendingReadEvent::Eof => {
                        profiling::scope!("stream.pending.eof");
                        if let Err(err) = self.flush_pending_data_with_py(py, &mut pending_data) {
                            let _ = self.report_error_with_py(
                                py,
                                err,
                                "stream data_received callback failed",
                            );
                            let _ = self.connection_lost_with_py(py, None);
                            self.read_events_scheduled.store(false, Ordering::Release);
                            return Ok(());
                        }
                        match self.eof_received_with_py(py) {
                            Ok(true) => {
                                self.read_events_scheduled.store(false, Ordering::Release);
                                return Ok(());
                            }
                            Ok(false) => {
                                self.set_closing();
                                let _ = self.writer_tx.send(WriterCommand::Close);
                                self.read_events_scheduled.store(false, Ordering::Release);
                                return Ok(());
                            }
                            Err(err) => {
                                let _ = self.report_error_with_py(
                                    py,
                                    err,
                                    "stream eof_received callback failed",
                                );
                                let _ = self.connection_lost_with_py(py, None);
                                self.read_events_scheduled.store(false, Ordering::Release);
                                return Ok(());
                            }
                        }
                    }
                    PendingReadEvent::ConnectionLost(message) => {
                        profiling::scope!("stream.pending.connection_lost");
                        if let Err(err) = self.flush_pending_data_with_py(py, &mut pending_data) {
                            let _ = self.report_error_with_py(
                                py,
                                err,
                                "stream data_received callback failed",
                            );
                            let _ = self.connection_lost_with_py(py, None);
                            self.read_events_scheduled.store(false, Ordering::Release);
                            return Ok(());
                        }
                        let err = message.map(PyRuntimeError::new_err);
                        let _ = self.connection_lost_with_py(py, err);
                        self.read_events_scheduled.store(false, Ordering::Release);
                        return Ok(());
                    }
                    PendingReadEvent::PauseWriting => {
                        profiling::scope!("stream.pending.pause_writing");
                        if let Err(err) = self.flush_pending_data_with_py(py, &mut pending_data) {
                            let _ = self.report_error_with_py(
                                py,
                                err,
                                "stream data_received callback failed",
                            );
                            let _ = self.connection_lost_with_py(py, None);
                            self.read_events_scheduled.store(false, Ordering::Release);
                            return Ok(());
                        }
                        self.pause_writing_with_py(py)?;
                    }
                    PendingReadEvent::ResumeWriting => {
                        profiling::scope!("stream.pending.resume_writing");
                        if let Err(err) = self.flush_pending_data_with_py(py, &mut pending_data) {
                            let _ = self.report_error_with_py(
                                py,
                                err,
                                "stream data_received callback failed",
                            );
                            let _ = self.connection_lost_with_py(py, None);
                            self.read_events_scheduled.store(false, Ordering::Release);
                            return Ok(());
                        }
                        self.resume_writing_with_py(py)?;
                    }
                }
            }

            if let Err(err) = self.flush_pending_data_with_py(py, &mut pending_data) {
                let _ = self.report_error_with_py(py, err, "stream data_received callback failed");
                let _ = self.connection_lost_with_py(py, None);
                self.read_events_scheduled.store(false, Ordering::Release);
                return Ok(());
            }
        }
    }

    fn call_protocol_method0(
        &self,
        py: Python<'_>,
        callback: &Py<PyAny>,
        context: &Py<PyAny>,
        context_needs_run: bool,
    ) -> PyResult<Py<PyAny>> {
        run_in_context_noargs(py, context, context_needs_run, callback)
    }

    fn call_protocol_method1(
        &self,
        py: Python<'_>,
        callback: &Py<PyAny>,
        context: &Py<PyAny>,
        context_needs_run: bool,
        arg: Py<PyAny>,
    ) -> PyResult<Py<PyAny>> {
        run_in_context_onearg(py, context, context_needs_run, callback, arg.bind(py))
    }

    fn flush_pending_data_with_py(
        &self,
        py: Python<'_>,
        pending_data: &mut Option<PendingReadBuffer>,
    ) -> PyResult<()> {
        let Some(data) = pending_data.take() else {
            return Ok(());
        };

        if self.is_closing_or_lost() {
            return Ok(());
        }

        self.data_received_with_py(py, data.as_slice())
    }

    fn report_error_with_py(&self, py: Python<'_>, err: PyErr, message: &str) -> PyResult<()> {
        let context = PyDict::new(py);
        context.set_item("message", message)?;
        context.set_item("exception", err.value(py))?;
        self.loop_core
            .call_exception_handler(py, Some(&self.loop_obj), context.unbind().into_any())
    }

    pub fn connection_made(&self, transport: Py<PyStreamTransport>) -> PyResult<()> {
        profiling::scope!("StreamTransportCore::connection_made");
        self.call_in_loop_context(|py| {
            let (callback, fast_path, context, context_needs_run) = {
                let state = self.state.lock().expect("poisoned transport state");
                (
                    state.callbacks.connection_made.clone_ref(py),
                    state
                        .callbacks
                        .stream_reader_fast_path
                        .as_ref()
                        .map(|value| value.clone_ref(py)),
                    state.context.clone_ref(py),
                    state.context_needs_run,
                )
            };
            if let Some(fast_path) = fast_path.as_ref() {
                if fast_path.connection_made(py, transport.clone_ref(py))? {
                    return Ok(());
                }
            }
            self.call_protocol_method1(
                py,
                &callback,
                &context,
                context_needs_run,
                transport.into_any(),
            )?;
            Ok(())
        })
    }

    pub fn data_received(&self, data: &[u8]) -> PyResult<()> {
        self.call_in_loop_context(|py| self.data_received_with_py(py, data))
    }

    pub fn eof_received(&self) -> PyResult<bool> {
        self.call_in_loop_context(|py| self.eof_received_with_py(py))
    }

    pub fn connection_lost(self: &Arc<Self>, exc: Option<PyErr>) -> PyResult<()> {
        if !self.loop_core.on_runtime_thread() {
            self.enqueue_pending_read_event(PendingReadEvent::ConnectionLost(
                exc.map(|err| Python::attach(|py| err.value(py).to_string())),
            ));
            return Ok(());
        }

        self.call_in_loop_context(|py| self.connection_lost_with_py(py, exc))
    }

    fn data_received_with_py(&self, py: Python<'_>, data: &[u8]) -> PyResult<()> {
        profiling::scope!("StreamTransportCore::data_received_with_py");
        let fast_path = {
            let state = self.state.lock().expect("poisoned transport state");
            state
                .callbacks
                .stream_reader_fast_path
                .as_ref()
                .map(|value| value.clone_ref(py))
        };

        if let Some(fast_path) = fast_path.as_ref() {
            return fast_path.feed_data(py, data);
        }

        let (data_received, get_buffer, buffer_updated, context, context_needs_run) = {
            let state = self.state.lock().expect("poisoned transport state");
            (
                state
                    .callbacks
                    .data_received
                    .as_ref()
                    .map(|value| value.clone_ref(py)),
                state
                    .callbacks
                    .get_buffer
                    .as_ref()
                    .map(|value| value.clone_ref(py)),
                state
                    .callbacks
                    .buffer_updated
                    .as_ref()
                    .map(|value| value.clone_ref(py)),
                state.context.clone_ref(py),
                state.context_needs_run,
            )
        };

        if let (Some(get_buffer), Some(buffer_updated)) =
            (get_buffer.as_ref(), buffer_updated.as_ref())
        {
            let args = PyTuple::new(py, [data.len()])?.unbind();
            let buffer_obj = run_in_context(py, &context, context_needs_run, get_buffer, &args)?;
            let memoryview = unsafe {
                Bound::from_owned_ptr_or_err(
                    py,
                    pyo3::ffi::PyMemoryView_FromObject(buffer_obj.bind(py).as_ptr()),
                )
            }?;
            memoryview.set_item(
                PySlice::new(py, 0, data.len() as isize, 1),
                PyBytes::new(py, data),
            )?;
            let updated_args = PyTuple::new(py, [data.len()])?.unbind();
            run_in_context(
                py,
                &context,
                context_needs_run,
                buffer_updated,
                &updated_args,
            )?;
            return Ok(());
        }

        if let Some(data_received) = data_received.as_ref() {
            self.call_protocol_method1(
                py,
                data_received,
                &context,
                context_needs_run,
                PyBytes::new(py, data).unbind().into_any(),
            )?;
        }
        Ok(())
    }

    fn eof_received_with_py(&self, py: Python<'_>) -> PyResult<bool> {
        profiling::scope!("StreamTransportCore::eof_received_with_py");
        let (callback, fast_path, context, context_needs_run) = {
            let state = self.state.lock().expect("poisoned transport state");
            (
                state
                    .callbacks
                    .eof_received
                    .as_ref()
                    .map(|value| value.clone_ref(py)),
                state
                    .callbacks
                    .stream_reader_fast_path
                    .as_ref()
                    .map(|value| value.clone_ref(py)),
                state.context.clone_ref(py),
                state.context_needs_run,
            )
        };
        if let Some(fast_path) = fast_path.as_ref() {
            return fast_path.eof_received(py);
        }
        let Some(callback) = callback else {
            return Ok(false);
        };
        let result = self.call_protocol_method0(py, &callback, &context, context_needs_run)?;
        result.bind(py).is_truthy()
    }

    fn connection_lost_with_py(&self, py: Python<'_>, exc: Option<PyErr>) -> PyResult<()> {
        profiling::scope!("StreamTransportCore::connection_lost_with_py");
        let (callback, fast_path, context, context_needs_run, server) = {
            let mut state = self.state.lock().expect("poisoned transport state");
            if state.detached {
                state.lost_called = true;
                return Ok(());
            }
            if state.lost_called {
                return Ok(());
            }
            state.lost_called = true;
            state.closing = true;
            state.write_buffer_size = 0;
            state.protocol_write_paused = false;
            (
                state.callbacks.connection_lost.clone_ref(py),
                state
                    .callbacks
                    .stream_reader_fast_path
                    .as_ref()
                    .map(|value| value.clone_ref(py)),
                state.context.clone_ref(py),
                state.context_needs_run,
                state.server.as_ref().cloned(),
            )
        };

        if let Some(fast_path) = fast_path.as_ref() {
            fast_path.connection_lost(py, exc)?;
        } else {
            let arg = exc
                .map(|err| err.value(py).clone().unbind().into_any())
                .unwrap_or_else(|| py.None());
            self.call_protocol_method1(py, &callback, &context, context_needs_run, arg)?;
        }

        self.close_extra_socket_with_py(py);

        if let Some(server) = server.and_then(|weak| weak.upgrade()) {
            server.connection_lost();
        }
        Ok(())
    }

    fn set_protocol(&self, py: Python<'_>, protocol: Py<PyAny>) -> PyResult<()> {
        let callbacks = build_protocol_callbacks(py, &protocol)?;
        let mut state = self.state.lock().expect("poisoned transport state");
        state.protocol = protocol;
        state.callbacks = callbacks;
        Ok(())
    }

    fn get_protocol(&self, py: Python<'_>) -> Py<PyAny> {
        self.state
            .lock()
            .expect("poisoned transport state")
            .protocol
            .clone_ref(py)
    }

    fn get_extra(&self, py: Python<'_>, name: &str) -> Option<Py<PyAny>> {
        self.state
            .lock()
            .expect("poisoned transport state")
            .extra
            .get(name)
            .map(|value| value.clone_ref(py))
    }

    fn set_closing(&self) {
        self.state.lock().expect("poisoned transport state").closing = true;
    }

    fn runtime_socket_fd(&self) -> Option<fd_ops::RawFd> {
        let state = self.state.lock().expect("poisoned transport state");
        if state.runtime_socket_io {
            state.io_fd
        } else {
            None
        }
    }

    fn detach_underlying_stream(&self, py: Python<'_>) {
        self.close_extra_socket_with_py(py);
        let mut state = self.state.lock().expect("poisoned transport state");
        state.detached = true;
        state.closing = true;
        state.reading = false;
        state.writable = false;
    }

    fn is_closing_or_lost(&self) -> bool {
        let state = self.state.lock().expect("poisoned transport state");
        state.closing || state.lost_called
    }

    fn mark_write_eof(&self) {
        self.state
            .lock()
            .expect("poisoned transport state")
            .write_eof_requested = true;
    }

    fn is_closing(&self) -> bool {
        self.state.lock().expect("poisoned transport state").closing
    }

    fn can_write_eof(&self) -> bool {
        self.state
            .lock()
            .expect("poisoned transport state")
            .can_write_eof
    }

    fn pause_reading(&self) {
        let mut state = self.state.lock().expect("poisoned transport state");
        state.read_paused = true;
        state.reading = false;
    }

    fn resume_reading(&self) {
        let mut state = self.state.lock().expect("poisoned transport state");
        state.read_paused = false;
        state.reading = true;
    }

    fn is_reading(&self) -> bool {
        self.state.lock().expect("poisoned transport state").reading
    }

    fn wait_until_readable(&self) {
        loop {
            let paused = {
                self.state
                    .lock()
                    .expect("poisoned transport state")
                    .read_paused
            };
            if !paused || self.is_closing() {
                return;
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn is_writable(&self) -> bool {
        self.state
            .lock()
            .expect("poisoned transport state")
            .writable
    }

    fn get_write_buffer_size(&self) -> usize {
        self.state
            .lock()
            .expect("poisoned transport state")
            .write_buffer_size
    }

    fn get_write_buffer_limits(&self) -> (usize, usize) {
        let state = self.state.lock().expect("poisoned transport state");
        (state.write_buffer_low_water, state.write_buffer_high_water)
    }

    fn set_write_buffer_limits(
        self: &Arc<Self>,
        high: Option<usize>,
        low: Option<usize>,
    ) -> PyResult<()> {
        let (should_pause, should_resume) = {
            let mut state = self.state.lock().expect("poisoned transport state");
            let high = match (high, low) {
                (Some(high), _) => high,
                (None, Some(low)) => 4 * low,
                (None, None) => DEFAULT_WRITE_BUFFER_HIGH_WATER,
            };
            let low = low.unwrap_or(high / 4);

            if !(high >= low) {
                return Err(PyValueError::new_err(format!(
                    "high ({high:?}) must be >= low ({low:?}) must be >= 0"
                )));
            }

            state.write_buffer_high_water = high;
            state.write_buffer_low_water = low;

            let should_pause = state.write_buffer_size > state.write_buffer_high_water
                && !state.protocol_write_paused;
            let should_resume = state.protocol_write_paused
                && state.write_buffer_size <= state.write_buffer_low_water;

            if should_pause {
                state.protocol_write_paused = true;
            } else if should_resume {
                state.protocol_write_paused = false;
            }

            (should_pause, should_resume)
        };

        if should_pause {
            self.notify_pause_writing();
        } else if should_resume {
            self.notify_resume_writing();
        }

        Ok(())
    }

    fn write_backpressure_active(&self) -> bool {
        self.state
            .lock()
            .expect("poisoned transport state")
            .writer_registered
    }

    fn set_write_backpressure_active(&self, active: bool) {
        self.state
            .lock()
            .expect("poisoned transport state")
            .writer_registered = active;
    }

    fn close_on_write_eof(&self) -> bool {
        self.state
            .lock()
            .expect("poisoned transport state")
            .close_on_write_eof
    }

    fn try_direct_tasked_write(&self, data: &[u8]) -> io::Result<usize> {
        let Some(writer) = &self.direct_writer else {
            return Err(io::Error::other("not direct-tasked"));
        };
        let mut writer = writer.lock().expect("poisoned direct tasked writer");
        match &mut *writer {
            TaskedDirectWriter::Tcp(stream) => stream.write(data),
            #[cfg(unix)]
            TaskedDirectWriter::Unix(stream) => stream.write(data),
        }
    }

    fn fail_write(self: &Arc<Self>, err: Option<io::Error>) {
        if self.is_closing() {
            return;
        }

        self.set_closing();
        self.set_write_backpressure_active(false);
        self.clear_write_buffer(false);
        self.enqueue_pending_read_event(PendingReadEvent::ConnectionLost(
            err.map(|err| err.to_string()),
        ));
        let _ = self.writer_tx.send(WriterCommand::Stop);
    }

    fn queue_write(self: &Arc<Self>, data: OwnedWriteBuffer) -> io::Result<()> {
        let should_pause = self.record_write_buffer_enqueued(data.remaining().len());
        self.ensure_writer_worker();
        if should_pause {
            self.notify_pause_writing();
        }
        if self.writer_tx.send(WriterCommand::Data(data)).is_err() {
            self.clear_write_buffer(false);
            self.fail_write(None);
        }
        Ok(())
    }

    fn try_write_bytes(self: &Arc<Self>, data: &[u8]) -> io::Result<()> {
        if self.direct_writer.is_some() && !self.write_backpressure_active() {
            match self.try_direct_tasked_write(data) {
                Ok(written) if written == data.len() => return Ok(()),
                Ok(written) => {
                    let mut pending = OwnedWriteBuffer::from_slice(data);
                    pending.advance(written);
                    self.set_write_backpressure_active(true);
                    return self.queue_write(pending);
                }
                Err(err)
                    if matches!(
                        err.kind(),
                        io::ErrorKind::WouldBlock | io::ErrorKind::Interrupted
                    ) =>
                {
                    self.set_write_backpressure_active(true);
                    return self.queue_write(OwnedWriteBuffer::from_slice(data));
                }
                Err(err) => {
                    self.fail_write(Some(err));
                    return Ok(());
                }
            }
        }

        self.queue_write(OwnedWriteBuffer::from_slice(data))
    }

    pub async fn wait_readable(self: &Arc<Self>) -> io::Result<()> {
        Err(io::Error::other(
            "transport readiness is not used in std transport mode",
        ))
    }

    pub async fn wait_writable(self: &Arc<Self>) -> io::Result<()> {
        Err(io::Error::other(
            "transport readiness is not used in std transport mode",
        ))
    }

    pub fn handle_read_ready_with_py(self: &Arc<Self>, _py: Python<'_>) {}

    pub fn handle_write_ready_with_py(self: &Arc<Self>, _py: Python<'_>) {}

    fn upgrade_stream(
        self: &Arc<Self>,
        py: Python<'_>,
    ) -> PyResult<(TransportSpawnContext, StreamKind)> {
        let protocol = self.get_protocol(py);
        let context = self
            .state
            .lock()
            .expect("poisoned transport state")
            .context
            .clone_ref(py);
        let context_needs_run = self
            .state
            .lock()
            .expect("poisoned transport state")
            .context_needs_run;
        let socket = self
            .get_extra(py, "socket")
            .ok_or_else(|| PyRuntimeError::new_err("transport does not expose a socket"))?;
        let fd = fd_ops::dup_raw_fd(socket.bind(py).call_method0("fileno")?.extract()?)
            .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;

        self.detach_underlying_stream(py);
        let _ = self.writer_tx.send(WriterCommand::Stop);
        if let Some(fd) = self.runtime_socket_fd() {
            let _ = self
                .loop_core
                .send_command(LoopCommand::StopSocketReader(fd));
        }
        for worker in self
            .workers
            .lock()
            .expect("poisoned transport workers")
            .drain(..)
        {
            worker.abort();
        }

        let family = socket.bind(py).getattr("family")?.extract::<i32>()?;
        #[cfg(unix)]
        let stream = if family == libc::AF_UNIX {
            StreamKind::Unix(unix_stream_from_socket_fd(fd)?)
        } else {
            StreamKind::Tcp(tcp_stream_from_socket_fd(fd)?)
        };
        #[cfg(not(unix))]
        let stream = StreamKind::Tcp(tcp_stream_from_socket_fd(fd)?);

        Ok((
            TransportSpawnContext {
                loop_core: Arc::clone(&self.loop_core),
                loop_obj: self.loop_obj.clone_ref(py),
                protocol,
                context,
                context_needs_run,
            },
            stream,
        ))
    }

    fn pause_writing_with_py(&self, py: Python<'_>) -> PyResult<()> {
        let (callback, context, context_needs_run) = {
            let state = self.state.lock().expect("poisoned transport state");
            (
                state.callbacks.pause_writing.clone_ref(py),
                state.context.clone_ref(py),
                state.context_needs_run,
            )
        };

        if let Err(err) = self.call_protocol_method0(py, &callback, &context, context_needs_run) {
            self.report_error_with_py(py, err, "protocol.pause_writing() failed")?;
        }
        Ok(())
    }

    fn resume_writing_with_py(&self, py: Python<'_>) -> PyResult<()> {
        let (callback, context, context_needs_run) = {
            let state = self.state.lock().expect("poisoned transport state");
            (
                state.callbacks.resume_writing.clone_ref(py),
                state.context.clone_ref(py),
                state.context_needs_run,
            )
        };

        if let Err(err) = self.call_protocol_method0(py, &callback, &context, context_needs_run) {
            self.report_error_with_py(py, err, "protocol.resume_writing() failed")?;
        }
        Ok(())
    }

    fn notify_pause_writing(self: &Arc<Self>) {
        if self.loop_core.on_runtime_thread() {
            let _ = self.call_in_loop_context(|py| self.pause_writing_with_py(py));
            return;
        }

        self.enqueue_pending_read_event(PendingReadEvent::PauseWriting);
    }

    fn notify_resume_writing(self: &Arc<Self>) {
        if self.loop_core.on_runtime_thread() {
            let _ = self.call_in_loop_context(|py| self.resume_writing_with_py(py));
            return;
        }

        self.enqueue_pending_read_event(PendingReadEvent::ResumeWriting);
    }

    fn record_write_buffer_enqueued(&self, len: usize) -> bool {
        if len == 0 {
            return false;
        }

        let mut state = self.state.lock().expect("poisoned transport state");
        state.write_buffer_size = state.write_buffer_size.saturating_add(len);
        if state.write_buffer_size > state.write_buffer_high_water && !state.protocol_write_paused {
            state.protocol_write_paused = true;
            return true;
        }

        false
    }

    fn record_write_buffer_drained(self: &Arc<Self>, len: usize) {
        if len == 0 {
            return;
        }

        let should_resume = {
            let mut state = self.state.lock().expect("poisoned transport state");
            state.write_buffer_size = state.write_buffer_size.saturating_sub(len);
            if state.protocol_write_paused
                && state.write_buffer_size <= state.write_buffer_low_water
            {
                state.protocol_write_paused = false;
                true
            } else {
                false
            }
        };

        if should_resume {
            self.notify_resume_writing();
        }
    }

    fn clear_write_buffer(self: &Arc<Self>, resume_protocol: bool) {
        let should_resume = {
            let mut state = self.state.lock().expect("poisoned transport state");
            let should_resume = resume_protocol && state.protocol_write_paused;
            state.write_buffer_size = 0;
            state.protocol_write_paused = false;
            should_resume
        };

        if should_resume {
            self.notify_resume_writing();
        }
    }
}

impl ServerCore {
    fn close_python_sockets(&self) {
        let _ = Python::try_attach(|py| -> PyResult<()> {
            for socket in &self.sockets {
                let _ = socket.bind(py).call_method0("close");
            }
            Ok(())
        });
    }

    fn report_error(&self, err: PyErr, message: &str) {
        let _ = Python::try_attach(|py| -> PyResult<()> {
            let context = PyDict::new(py);
            context.set_item("message", message)?;
            context.set_item("exception", err.value(py))?;
            self.loop_core.call_exception_handler(
                py,
                Some(&self.loop_obj),
                context.unbind().into_any(),
            )
        });
    }

    fn create_protocol_with_py(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        ensure_running_loop(py, &self.loop_obj)?;
        let callback = self.protocol_factory.bind(py).clone().unbind();
        let args = PyTuple::empty(py).unbind();
        run_in_context(py, &self.context, self.context_needs_run, &callback, &args)
    }

    fn locals(&self, py: Python<'_>) -> PyResult<TaskLocals> {
        task_locals_for_loop(py, &self.loop_obj)
    }

    fn is_closed(&self) -> bool {
        self.state.lock().expect("poisoned server state").closed
    }

    fn is_serving(&self) -> bool {
        let state = self.state.lock().expect("poisoned server state");
        state.serving && !state.closed
    }

    fn connection_opened(&self) {
        self.active_connections.fetch_add(1, Ordering::SeqCst);
    }

    fn connection_lost(&self) {
        self.active_connections.fetch_sub(1, Ordering::SeqCst);
        self.closed_notify.notify_all();
    }

    fn close(&self) {
        {
            let mut state = self.state.lock().expect("poisoned server state");
            if state.closed {
                return;
            }
            state.closed = true;
            state.serving = false;
            state.listeners.clear();
        }

        self.close_python_sockets();

        for task in self
            .accept_tasks
            .lock()
            .expect("poisoned accept tasks")
            .drain(..)
        {
            task.abort();
        }
        for fd in self
            .accept_fds
            .lock()
            .expect("poisoned accept fds")
            .drain(..)
        {
            let _ = self
                .loop_core
                .send_command(LoopCommand::StopServerAccept(fd));
        }

        if let Some(path) = &self.cleanup_path {
            let _ = fs::remove_file(path);
        }

        self.closed_notify.notify_all();
    }

    pub fn spawn_accept_tasks(self: &Arc<Self>) {
        let listeners = {
            let mut state = self.state.lock().expect("poisoned server state");
            if state.closed || state.serving {
                return;
            }
            state.serving = true;
            std::mem::take(&mut state.listeners)
        };

        #[cfg(target_os = "linux")]
        {
            if self.tls.is_some() {
                let mut tasks = self.accept_tasks.lock().expect("poisoned accept tasks");
                for listener in listeners {
                    let server = Arc::clone(self);
                    let task = match listener {
                        ServerListener::Tcp(listener) => {
                            WorkerThread::spawn("rsloop-tcp-accept".to_owned(), move |stop| {
                                run_tcp_accept_loop(server, listener, stop)
                            })
                        }
                        #[cfg(unix)]
                        ServerListener::Unix(listener) => {
                            WorkerThread::spawn("rsloop-unix-accept".to_owned(), move |stop| {
                                run_unix_accept_loop(server, listener, stop)
                            })
                        }
                    };
                    tasks.push(task);
                }
                return;
            }

            let mut accept_fds = self.accept_fds.lock().expect("poisoned accept fds");
            for listener in listeners {
                let fd = match &listener {
                    ServerListener::Tcp(listener) => tcp_listener_raw_fd(listener),
                    #[cfg(unix)]
                    ServerListener::Unix(listener) => unix_raw_fd(listener.as_raw_fd()),
                };
                accept_fds.push(fd);
                let _ = self.loop_core.send_command(LoopCommand::StartServerAccept {
                    fd,
                    server: Arc::clone(self),
                    listener,
                });
            }
            return;
        }

        #[cfg(not(target_os = "linux"))]
        let mut tasks = self.accept_tasks.lock().expect("poisoned accept tasks");
        #[cfg(not(target_os = "linux"))]
        for listener in listeners {
            let server = Arc::clone(self);
            let task = match listener {
                ServerListener::Tcp(listener) => {
                    WorkerThread::spawn("rsloop-tcp-accept".to_owned(), move |stop| {
                        run_tcp_accept_loop(server, listener, stop)
                    })
                }
                #[cfg(unix)]
                ServerListener::Unix(listener) => {
                    WorkerThread::spawn("rsloop-unix-accept".to_owned(), move |stop| {
                        run_unix_accept_loop(server, listener, stop)
                    })
                }
            };
            tasks.push(task);
        }
    }
}

#[pymethods]
impl PyStreamTransport {
    fn write(&self, py: Python<'_>, data: &Bound<'_, PyAny>) -> PyResult<()> {
        if self.core.is_closing() {
            return Ok(());
        }
        if !self.core.is_writable() {
            return Err(PyRuntimeError::new_err("transport is not writable"));
        }

        let borrowed_bytes;
        let converted = if let Some(encoding) = self.core.get_extra(py, "text_encoding") {
            if data.is_instance_of::<PyString>() {
                let errors = self
                    .core
                    .get_extra(py, "text_errors")
                    .unwrap_or_else(|| PyString::new(py, "strict").unbind().into_any());
                data.call_method1("encode", (encoding, errors))?
            } else {
                py.import("builtins")?.getattr("bytes")?.call1((data,))?
            }
        } else if let Ok(bytes) = data.cast::<PyBytes>() {
            borrowed_bytes = bytes;
            self.core
                .try_write_bytes(borrowed_bytes.as_bytes())
                .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
            return Ok(());
        } else {
            py.import("builtins")?.getattr("bytes")?.call1((data,))?
        };
        let bytes = converted.cast::<PyBytes>()?;
        self.core
            .try_write_bytes(bytes.as_bytes())
            .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
        Ok(())
    }

    fn writelines(&self, py: Python<'_>, seq: &Bound<'_, PyAny>) -> PyResult<()> {
        for item in seq.try_iter()? {
            self.write(py, &item?)?;
        }
        Ok(())
    }

    fn close(&self) -> PyResult<()> {
        self.core.set_closing();
        if let Some(fd) = self.core.runtime_socket_fd() {
            let _ = self
                .core
                .loop_core
                .send_command(LoopCommand::StopSocketReader(fd));
        }
        if self.core.direct_writer.is_none() {
            let _ = self.core.writer_tx.send(WriterCommand::Close);
            return Ok(());
        }
        if !self.core.write_backpressure_active() {
            if let Some(writer) = &self.core.direct_writer {
                let writer = writer.lock().expect("poisoned direct tasked writer");
                let _ = writer.shutdown_close();
            }
            let _ = self.core.writer_tx.send(WriterCommand::Stop);
            let _ = self.core.connection_lost(None);
            return Ok(());
        }

        let _ = self.core.writer_tx.send(WriterCommand::Close);
        Ok(())
    }

    fn abort(&self) -> PyResult<()> {
        self.core.set_closing();
        if let Some(fd) = self.core.runtime_socket_fd() {
            let _ = self
                .core
                .loop_core
                .send_command(LoopCommand::StopSocketReader(fd));
        }
        if self.core.direct_writer.is_none() {
            let _ = self.core.writer_tx.send(WriterCommand::Abort);
            return Ok(());
        }
        if let Some(writer) = &self.core.direct_writer {
            let writer = writer.lock().expect("poisoned direct tasked writer");
            let _ = writer.shutdown_close();
        }
        let _ = self.core.writer_tx.send(WriterCommand::Abort);
        let _ = self.core.connection_lost(None);
        Ok(())
    }

    fn is_closing(&self) -> bool {
        self.core.is_closing()
    }

    fn can_write_eof(&self) -> bool {
        self.core.can_write_eof()
    }

    fn write_eof(&self) -> PyResult<()> {
        if !self.core.can_write_eof() {
            return Err(PyRuntimeError::new_err(
                "transport does not support write_eof",
            ));
        }
        self.core.mark_write_eof();
        if self.core.direct_writer.is_some() && !self.core.write_backpressure_active() {
            if let Some(writer) = &self.core.direct_writer {
                let writer = writer.lock().expect("poisoned direct tasked writer");
                match &*writer {
                    TaskedDirectWriter::Tcp(stream) => {
                        let _ = shutdown_tcp_stream(stream, Shutdown::Write);
                    }
                    #[cfg(unix)]
                    TaskedDirectWriter::Unix(stream) => {
                        let _ = shutdown_unix_stream(stream, Shutdown::Write);
                    }
                }
            }
            if self.core.close_on_write_eof() {
                let _ = self.core.connection_lost(None);
            }
            return Ok(());
        }
        let _ = self.core.writer_tx.send(WriterCommand::WriteEof);
        Ok(())
    }

    #[pyo3(signature=(name, default=None))]
    fn get_extra_info(&self, py: Python<'_>, name: &str, default: Option<Py<PyAny>>) -> Py<PyAny> {
        self.core
            .get_extra(py, name)
            .unwrap_or_else(|| default.unwrap_or_else(|| py.None()))
    }

    fn get_protocol(&self, py: Python<'_>) -> Py<PyAny> {
        self.core.get_protocol(py)
    }

    fn set_protocol(&self, protocol: Py<PyAny>) {
        Python::attach(|py| self.core.set_protocol(py, protocol))
            .expect("failed to update transport protocol");
    }

    fn pause_reading(&self) {
        self.core.pause_reading();
    }

    fn resume_reading(&self) {
        self.core.resume_reading();
    }

    fn is_reading(&self) -> bool {
        self.core.is_reading()
    }

    fn get_write_buffer_size(&self) -> usize {
        self.core.get_write_buffer_size()
    }

    fn get_write_buffer_limits(&self) -> (usize, usize) {
        self.core.get_write_buffer_limits()
    }

    fn set_write_buffer_limits(&self, high: Option<usize>, low: Option<usize>) -> PyResult<()> {
        self.core.set_write_buffer_limits(high, low)
    }

    fn __repr__(&self) -> String {
        format!("<StreamTransport closing={}>", self.is_closing())
    }
}

#[pymethods]
impl PyServer {
    fn close(&self) {
        self.core.close();
    }

    fn is_serving(&self) -> bool {
        self.core.is_serving()
    }

    fn get_loop(&self, py: Python<'_>) -> Py<PyAny> {
        self.core.loop_obj.clone_ref(py)
    }

    fn start_serving<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let locals = self.core.locals(py)?;
        let core = Arc::clone(&self.core);
        pyo3_async_runtimes::async_std::future_into_py_with_locals(py, locals, async move {
            core.spawn_accept_tasks();
            Ok(Python::attach(|py| py.None()))
        })
    }

    fn wait_closed<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let locals = self.core.locals(py)?;
        let core = Arc::clone(&self.core);
        pyo3_async_runtimes::async_std::future_into_py_with_locals(py, locals, async move {
            loop {
                if core.is_closed() && core.active_connections.load(Ordering::SeqCst) == 0 {
                    return Ok(Python::attach(|py| py.None()));
                }
                let wait = core.closed_notify.listen();
                if core.is_closed() && core.active_connections.load(Ordering::SeqCst) == 0 {
                    return Ok(Python::attach(|py| py.None()));
                }
                let _ = wait.await;
            }
        })
    }

    fn serve_forever<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let locals = self.core.locals(py)?;
        let core = Arc::clone(&self.core);
        pyo3_async_runtimes::async_std::future_into_py_with_locals(py, locals, async move {
            core.spawn_accept_tasks();
            loop {
                if core.is_closed() {
                    return Ok(Python::attach(|py| py.None()));
                }
                let wait = core.closed_notify.listen();
                if core.is_closed() {
                    return Ok(Python::attach(|py| py.None()));
                }
                let _ = wait.await;
            }
        })
    }

    #[getter]
    fn sockets(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let tuple = PyTuple::new(
            py,
            self.core
                .sockets
                .iter()
                .map(|socket| socket.clone_ref(py))
                .collect::<Vec<_>>(),
        )?;
        Ok(tuple.unbind().into_any())
    }

    fn __repr__(&self) -> String {
        format!(
            "<Server serving={} closed={}>",
            self.core.is_serving(),
            self.core.is_closed()
        )
    }
}

pub fn task_locals_for_loop(py: Python<'_>, loop_obj: &Py<PyAny>) -> PyResult<TaskLocals> {
    TaskLocals::new(loop_obj.clone_ref(py).into_bound(py)).copy_context(py)
}

#[cfg(unix)]
fn file_raw_fd(file: &std::fs::File) -> fd_ops::RawFd {
    file.as_raw_fd() as fd_ops::RawFd
}

#[cfg(windows)]
fn file_raw_fd(file: &std::fs::File) -> fd_ops::RawFd {
    file.as_raw_handle() as isize as fd_ops::RawFd
}

#[cfg(unix)]
fn tcp_stream_raw_fd(stream: &StdTcpStream) -> fd_ops::RawFd {
    stream.as_raw_fd() as fd_ops::RawFd
}

#[cfg(windows)]
fn tcp_stream_raw_fd(stream: &StdTcpStream) -> fd_ops::RawFd {
    stream.as_raw_socket() as fd_ops::RawFd
}

#[cfg(unix)]
fn tcp_listener_raw_fd(listener: &StdTcpListener) -> fd_ops::RawFd {
    listener.as_raw_fd() as fd_ops::RawFd
}

#[cfg(windows)]
fn tcp_listener_raw_fd(listener: &StdTcpListener) -> fd_ops::RawFd {
    listener.as_raw_socket() as fd_ops::RawFd
}

#[cfg(unix)]
fn unix_raw_fd(fd: std::os::fd::RawFd) -> fd_ops::RawFd {
    fd as fd_ops::RawFd
}

#[cfg(unix)]
fn raw_fd_for_std(fd: fd_ops::RawFd) -> PyResult<std::os::fd::RawFd> {
    fd.try_into()
        .map_err(|_| PyRuntimeError::new_err("fd out of range"))
}

#[cfg(unix)]
fn socket_from_owned_raw(fd: fd_ops::RawFd) -> PyResult<Socket> {
    let fd: i32 = fd
        .try_into()
        .map_err(|_| PyRuntimeError::new_err("socket fd out of range"))?;
    Ok(unsafe { Socket::from_raw_fd(fd) })
}

#[cfg(windows)]
fn socket_from_owned_raw(fd: fd_ops::RawFd) -> PyResult<Socket> {
    let fd: RawSocket = fd
        .try_into()
        .map_err(|_| PyRuntimeError::new_err("socket handle out of range"))?;
    Ok(unsafe { Socket::from_raw_socket(fd) })
}

fn detached_socket_handle(py: Python<'_>, socket_obj: &Py<PyAny>) -> PyResult<fd_ops::RawFd> {
    socket_obj.call_method0(py, "detach")?.extract(py)
}

fn tcp_family(stream: &StdTcpStream) -> c_int {
    #[cfg(windows)]
    use windows_sys::Win32::Networking::WinSock::{AF_INET, AF_INET6};

    match stream.local_addr() {
        #[cfg(unix)]
        Ok(addr) if addr.is_ipv6() => libc::AF_INET6,
        #[cfg(unix)]
        _ => libc::AF_INET,
        #[cfg(windows)]
        Ok(addr) if addr.is_ipv6() => AF_INET6 as c_int,
        #[cfg(windows)]
        _ => AF_INET as c_int,
    }
}

pub fn transport_from_socket(
    py: Python<'_>,
    loop_core: Arc<LoopCore>,
    loop_obj: Py<PyAny>,
    protocol: Py<PyAny>,
    context: Py<PyAny>,
    context_needs_run: bool,
    socket_obj: Py<PyAny>,
) -> PyResult<Py<PyStreamTransport>> {
    profiling::scope!("stream.transport_from_socket");
    let spawn_context = TransportSpawnContext {
        loop_core,
        loop_obj,
        protocol,
        context,
        context_needs_run,
    };
    let family = socket_obj.getattr(py, "family")?.extract::<i32>(py)?;
    #[cfg(unix)]
    if family == libc::AF_UNIX {
        let fd = detached_socket_handle(py, &socket_obj)?;
        return spawn_unix_transport(py, spawn_context, unix_stream_from_socket_fd(fd)?, None);
    }

    let fd = detached_socket_handle(py, &socket_obj)?;
    spawn_tcp_transport(py, spawn_context, tcp_stream_from_socket_fd(fd)?, None)
}

pub fn transport_from_socket_tls(
    py: Python<'_>,
    loop_core: Arc<LoopCore>,
    loop_obj: Py<PyAny>,
    protocol: Py<PyAny>,
    context: Py<PyAny>,
    context_needs_run: bool,
    socket_obj: Py<PyAny>,
    tls: ClientTlsSettings,
) -> PyResult<Py<PyStreamTransport>> {
    profiling::scope!("stream.transport_from_socket_tls");
    let spawn_context = TransportSpawnContext {
        loop_core,
        loop_obj,
        protocol,
        context,
        context_needs_run,
    };
    let family = socket_obj.getattr(py, "family")?.extract::<i32>(py)?;
    #[cfg(unix)]
    if family == libc::AF_UNIX {
        let fd = detached_socket_handle(py, &socket_obj)?;
        return spawn_tls_client_transport(
            py,
            spawn_context,
            StreamKind::Unix(unix_stream_from_socket_fd(fd)?),
            tls,
            None,
            true,
        );
    }

    let fd = detached_socket_handle(py, &socket_obj)?;
    spawn_tls_client_transport(
        py,
        spawn_context,
        StreamKind::Tcp(tcp_stream_from_socket_fd(fd)?),
        tls,
        None,
        true,
    )
}

pub fn transport_from_socket_server_tls(
    py: Python<'_>,
    loop_core: Arc<LoopCore>,
    loop_obj: Py<PyAny>,
    protocol: Py<PyAny>,
    context: Py<PyAny>,
    context_needs_run: bool,
    socket_obj: Py<PyAny>,
    tls: ServerTlsSettings,
) -> PyResult<Py<PyStreamTransport>> {
    profiling::scope!("stream.transport_from_socket_server_tls");
    let spawn_context = TransportSpawnContext {
        loop_core,
        loop_obj,
        protocol,
        context,
        context_needs_run,
    };
    let family = socket_obj.getattr(py, "family")?.extract::<i32>(py)?;
    #[cfg(unix)]
    if family == libc::AF_UNIX {
        let fd = detached_socket_handle(py, &socket_obj)?;
        return spawn_tls_server_transport(
            py,
            spawn_context,
            StreamKind::Unix(unix_stream_from_socket_fd(fd)?),
            tls,
            None,
            true,
        );
    }

    let fd = detached_socket_handle(py, &socket_obj)?;
    spawn_tls_server_transport(
        py,
        spawn_context,
        StreamKind::Tcp(tcp_stream_from_socket_fd(fd)?),
        tls,
        None,
        true,
    )
}

pub fn start_tls_transport(
    py: Python<'_>,
    transport: Py<PyStreamTransport>,
    protocol: Py<PyAny>,
    client_tls: Option<ClientTlsSettings>,
    server_tls: Option<ServerTlsSettings>,
) -> PyResult<Py<PyStreamTransport>> {
    profiling::scope!("stream.start_tls_transport");
    let (mut spawn_context, stream) = transport.borrow(py).core.upgrade_stream(py)?;
    spawn_context.protocol = protocol;
    match (client_tls, server_tls) {
        (Some(tls), None) => spawn_tls_client_transport(py, spawn_context, stream, tls, None, true),
        (None, Some(tls)) => spawn_tls_server_transport(py, spawn_context, stream, tls, None, true),
        _ => Err(PyRuntimeError::new_err("invalid TLS upgrade configuration")),
    }
}

#[cfg(not(windows))]
pub fn spawn_read_pipe_transport(
    py: Python<'_>,
    loop_core: Arc<LoopCore>,
    loop_obj: Py<PyAny>,
    protocol: Py<PyAny>,
    context: Py<PyAny>,
    context_needs_run: bool,
    pipe_obj: Py<PyAny>,
) -> PyResult<Py<PyStreamTransport>> {
    let fd = fd_ops::fileobj_to_fd(py, pipe_obj.bind(py))?;
    let dup = fd_ops::dup_raw_fd(fd).map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
    let file = unsafe { std::fs::File::from_raw_fd(raw_fd_for_std(dup)?) };
    let callbacks = build_protocol_callbacks(py, &protocol)?;
    let mut extra = HashMap::with_capacity(2);
    extra.insert("pipe".to_owned(), pipe_obj.clone_ref(py));
    extra.insert("file".to_owned(), pipe_obj.clone_ref(py));

    let (writer_tx, writer_rx) = mpsc::channel();
    let core = Arc::new(StreamTransportCore {
        loop_core,
        loop_obj,
        state: Mutex::new(StreamTransportState {
            io_fd: None,
            runtime_socket_io: false,
            protocol,
            callbacks,
            context,
            context_needs_run,
            extra,
            closing: false,
            read_paused: false,
            reading: true,
            writable: false,
            write_eof_requested: false,
            can_write_eof: false,
            close_on_write_eof: false,
            lost_called: false,
            writer_registered: false,
            write_buffer_size: 0,
            write_buffer_high_water: DEFAULT_WRITE_BUFFER_HIGH_WATER,
            write_buffer_low_water: DEFAULT_WRITE_BUFFER_LOW_WATER,
            protocol_write_paused: false,
            detached: false,
            server: None,
        }),
        pending_read_events: Mutex::new(VecDeque::new()),
        read_events_scheduled: AtomicBool::new(false),
        writer_tx,
        direct_writer: None,
        lazy_writer: Mutex::new(None),
        workers: Mutex::new(Vec::new()),
    });

    let transport = Py::new(
        py,
        PyStreamTransport {
            core: Arc::clone(&core),
        },
    )?;
    core.connection_made(transport.clone_ref(py))?;
    spawn_reader_worker(Arc::clone(&core), ReaderTarget::File(file));
    spawn_writer_worker(core, WriterTarget::Sink(io::sink()), writer_rx);
    Ok(transport)
}

#[cfg(windows)]
pub fn spawn_read_pipe_transport(
    py: Python<'_>,
    loop_core: Arc<LoopCore>,
    loop_obj: Py<PyAny>,
    protocol: Py<PyAny>,
    context: Py<PyAny>,
    context_needs_run: bool,
    pipe_obj: Py<PyAny>,
) -> PyResult<Py<PyStreamTransport>> {
    let fd = fd_ops::fileobj_to_fd(py, pipe_obj.bind(py))?;
    let handle = fd_ops::duplicate_handle_from_fd(fd)
        .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
    let file = unsafe { std::fs::File::from_raw_handle(handle as _) };
    let callbacks = build_protocol_callbacks(py, &protocol)?;
    let mut extra = HashMap::with_capacity(2);
    extra.insert("pipe".to_owned(), pipe_obj.clone_ref(py));
    extra.insert("file".to_owned(), pipe_obj.clone_ref(py));

    let (writer_tx, writer_rx) = mpsc::channel();
    let core = Arc::new(StreamTransportCore {
        loop_core,
        loop_obj,
        state: Mutex::new(StreamTransportState {
            io_fd: None,
            runtime_socket_io: false,
            protocol,
            callbacks,
            context,
            context_needs_run,
            extra,
            closing: false,
            read_paused: false,
            reading: true,
            writable: false,
            write_eof_requested: false,
            can_write_eof: false,
            close_on_write_eof: false,
            lost_called: false,
            writer_registered: false,
            write_buffer_size: 0,
            write_buffer_high_water: DEFAULT_WRITE_BUFFER_HIGH_WATER,
            write_buffer_low_water: DEFAULT_WRITE_BUFFER_LOW_WATER,
            protocol_write_paused: false,
            detached: false,
            server: None,
        }),
        pending_read_events: Mutex::new(VecDeque::new()),
        read_events_scheduled: AtomicBool::new(false),
        writer_tx,
        direct_writer: None,
        lazy_writer: Mutex::new(None),
        workers: Mutex::new(Vec::new()),
    });

    let transport = Py::new(
        py,
        PyStreamTransport {
            core: Arc::clone(&core),
        },
    )?;
    core.connection_made(transport.clone_ref(py))?;
    spawn_reader_worker(Arc::clone(&core), ReaderTarget::File(file));
    spawn_writer_worker(core, WriterTarget::Sink(io::sink()), writer_rx);
    Ok(transport)
}

#[cfg(not(windows))]
pub fn spawn_write_pipe_transport(
    py: Python<'_>,
    spawn_context: TransportSpawnContext,
    pipe_obj: Py<PyAny>,
    extra_entries: Option<HashMap<String, Py<PyAny>>>,
) -> PyResult<Py<PyStreamTransport>> {
    let TransportSpawnContext {
        loop_core,
        loop_obj,
        protocol,
        context,
        context_needs_run,
    } = spawn_context;
    let fd = fd_ops::fileobj_to_fd(py, pipe_obj.bind(py))?;
    let dup = fd_ops::dup_raw_fd(fd).map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
    let file = unsafe { std::fs::File::from_raw_fd(raw_fd_for_std(dup)?) };
    let callbacks = build_protocol_callbacks(py, &protocol)?;
    let mut extra = HashMap::with_capacity(2 + extra_entries.as_ref().map_or(0, HashMap::len));
    extra.insert("pipe".to_owned(), pipe_obj.clone_ref(py));
    extra.insert("file".to_owned(), pipe_obj.clone_ref(py));
    if let Some(extra_entries) = extra_entries {
        extra.extend(extra_entries);
    }

    let (writer_tx, writer_rx) = mpsc::channel();
    let core = Arc::new(StreamTransportCore {
        loop_core,
        loop_obj,
        state: Mutex::new(StreamTransportState {
            io_fd: None,
            runtime_socket_io: false,
            protocol,
            callbacks,
            context,
            context_needs_run,
            extra,
            closing: false,
            read_paused: false,
            reading: false,
            writable: true,
            write_eof_requested: false,
            can_write_eof: true,
            close_on_write_eof: true,
            lost_called: false,
            writer_registered: false,
            write_buffer_size: 0,
            write_buffer_high_water: DEFAULT_WRITE_BUFFER_HIGH_WATER,
            write_buffer_low_water: DEFAULT_WRITE_BUFFER_LOW_WATER,
            protocol_write_paused: false,
            detached: false,
            server: None,
        }),
        pending_read_events: Mutex::new(VecDeque::new()),
        read_events_scheduled: AtomicBool::new(false),
        writer_tx,
        direct_writer: None,
        lazy_writer: Mutex::new(None),
        workers: Mutex::new(Vec::new()),
    });

    let transport = Py::new(
        py,
        PyStreamTransport {
            core: Arc::clone(&core),
        },
    )?;
    core.connection_made(transport.clone_ref(py))?;
    spawn_writer_worker(core, WriterTarget::File(file), writer_rx);
    Ok(transport)
}

#[cfg(windows)]
pub fn spawn_write_pipe_transport(
    py: Python<'_>,
    spawn_context: TransportSpawnContext,
    pipe_obj: Py<PyAny>,
    extra_entries: Option<HashMap<String, Py<PyAny>>>,
) -> PyResult<Py<PyStreamTransport>> {
    let TransportSpawnContext {
        loop_core,
        loop_obj,
        protocol,
        context,
        context_needs_run,
    } = spawn_context;
    let fd = fd_ops::fileobj_to_fd(py, pipe_obj.bind(py))?;
    let handle = fd_ops::duplicate_handle_from_fd(fd)
        .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
    let file = unsafe { std::fs::File::from_raw_handle(handle as _) };
    let callbacks = build_protocol_callbacks(py, &protocol)?;
    let mut extra = HashMap::with_capacity(2 + extra_entries.as_ref().map_or(0, HashMap::len));
    extra.insert("pipe".to_owned(), pipe_obj.clone_ref(py));
    extra.insert("file".to_owned(), pipe_obj.clone_ref(py));
    if let Some(extra_entries) = extra_entries {
        extra.extend(extra_entries);
    }

    let (writer_tx, writer_rx) = mpsc::channel();
    let core = Arc::new(StreamTransportCore {
        loop_core,
        loop_obj,
        state: Mutex::new(StreamTransportState {
            io_fd: None,
            runtime_socket_io: false,
            protocol,
            callbacks,
            context,
            context_needs_run,
            extra,
            closing: false,
            read_paused: false,
            reading: false,
            writable: true,
            write_eof_requested: false,
            can_write_eof: true,
            close_on_write_eof: true,
            lost_called: false,
            writer_registered: false,
            write_buffer_size: 0,
            write_buffer_high_water: DEFAULT_WRITE_BUFFER_HIGH_WATER,
            write_buffer_low_water: DEFAULT_WRITE_BUFFER_LOW_WATER,
            protocol_write_paused: false,
            detached: false,
            server: None,
        }),
        pending_read_events: Mutex::new(VecDeque::new()),
        read_events_scheduled: AtomicBool::new(false),
        writer_tx,
        direct_writer: None,
        lazy_writer: Mutex::new(None),
        workers: Mutex::new(Vec::new()),
    });

    let transport = Py::new(
        py,
        PyStreamTransport {
            core: Arc::clone(&core),
        },
    )?;
    core.connection_made(transport.clone_ref(py))?;
    spawn_writer_worker(core, WriterTarget::File(file), writer_rx);
    Ok(transport)
}

pub fn tcp_stream_from_socket_fd(fd: fd_ops::RawFd) -> PyResult<StdTcpStream> {
    duplicate_configured_tcp_stream(fd)
}

#[cfg(unix)]
pub fn unix_stream_from_socket_fd(fd: fd_ops::RawFd) -> PyResult<StdUnixStream> {
    let dup = fd_ops::dup_raw_fd(fd).map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
    let stream = unsafe { StdUnixStream::from_raw_fd(raw_fd_for_std(dup)?) };
    stream
        .set_nonblocking(true)
        .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
    Ok(stream)
}

pub fn tcp_listener_from_socket_fd(fd: fd_ops::RawFd) -> PyResult<StdTcpListener> {
    duplicate_configured_tcp_listener(fd)
}

#[cfg(unix)]
pub fn unix_listener_from_socket_fd(fd: fd_ops::RawFd) -> PyResult<StdUnixListener> {
    let dup = fd_ops::dup_raw_fd(fd).map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
    let listener = unsafe { StdUnixListener::from_raw_fd(raw_fd_for_std(dup)?) };
    listener
        .set_nonblocking(true)
        .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
    Ok(listener)
}

fn duplicate_configured_tcp_stream(fd: fd_ops::RawFd) -> PyResult<StdTcpStream> {
    let dup = fd_ops::dup_raw_fd(fd).map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
    let socket = socket_from_owned_raw(dup)?;
    socket
        .set_nonblocking(true)
        .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
    let _ = socket.set_tcp_nodelay(true);
    Ok(socket.into())
}

fn duplicate_configured_tcp_listener(fd: fd_ops::RawFd) -> PyResult<StdTcpListener> {
    let dup = fd_ops::dup_raw_fd(fd).map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
    let socket = socket_from_owned_raw(dup)?;
    socket
        .set_nonblocking(true)
        .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
    Ok(socket.into())
}

pub fn spawn_tcp_transport(
    py: Python<'_>,
    spawn_context: TransportSpawnContext,
    stream: StdTcpStream,
    server: Option<Weak<ServerCore>>,
) -> PyResult<Py<PyStreamTransport>> {
    let TransportSpawnContext {
        loop_core,
        loop_obj,
        protocol,
        context,
        context_needs_run,
    } = spawn_context;
    let raw_fd = tcp_stream_raw_fd(&stream);
    let extra = make_stream_extra(py, raw_fd, tcp_family(&stream))?;
    let callbacks = build_protocol_callbacks(py, &protocol)?;
    let context_needs_run = context_needs_run && callbacks.stream_reader_fast_path.is_none();
    let direct_writer = duplicate_configured_tcp_stream(raw_fd)?;
    let writer = stream
        .try_clone()
        .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
    let (writer_tx, writer_rx) = mpsc::channel();
    let core = Arc::new(StreamTransportCore {
        loop_core,
        loop_obj,
        state: Mutex::new(StreamTransportState {
            io_fd: Some(raw_fd),
            runtime_socket_io: true,
            protocol,
            callbacks,
            context,
            context_needs_run,
            extra,
            closing: false,
            read_paused: false,
            reading: true,
            writable: true,
            write_eof_requested: false,
            can_write_eof: true,
            close_on_write_eof: false,
            lost_called: false,
            writer_registered: false,
            write_buffer_size: 0,
            write_buffer_high_water: DEFAULT_WRITE_BUFFER_HIGH_WATER,
            write_buffer_low_water: DEFAULT_WRITE_BUFFER_LOW_WATER,
            protocol_write_paused: false,
            detached: false,
            server,
        }),
        pending_read_events: Mutex::new(VecDeque::new()),
        read_events_scheduled: AtomicBool::new(false),
        writer_tx,
        direct_writer: Some(Mutex::new(TaskedDirectWriter::Tcp(direct_writer))),
        lazy_writer: Mutex::new(Some(LazyWriterConfig {
            target: WriterTarget::Tcp(writer),
            writer_rx,
        })),
        workers: Mutex::new(Vec::new()),
    });

    let transport = Py::new(
        py,
        PyStreamTransport {
            core: Arc::clone(&core),
        },
    )?;
    core.connection_made(transport.clone_ref(py))?;
    if let Some(server) = core.server_ref().and_then(|weak| weak.upgrade()) {
        server.connection_opened();
    }

    #[cfg(target_os = "linux")]
    let _ = core.loop_core.send_command(LoopCommand::StartSocketReader {
        fd: raw_fd,
        core: Arc::clone(&core),
        reader: ReaderTarget::Tcp(stream),
    });
    #[cfg(not(target_os = "linux"))]
    spawn_reader_worker(Arc::clone(&core), ReaderTarget::Tcp(stream));
    Ok(transport)
}

#[cfg(unix)]
pub fn spawn_unix_transport(
    py: Python<'_>,
    spawn_context: TransportSpawnContext,
    stream: StdUnixStream,
    server: Option<Weak<ServerCore>>,
) -> PyResult<Py<PyStreamTransport>> {
    let TransportSpawnContext {
        loop_core,
        loop_obj,
        protocol,
        context,
        context_needs_run,
    } = spawn_context;
    let raw_fd = unix_raw_fd(stream.as_raw_fd());
    let extra = make_stream_extra(py, raw_fd, libc::AF_UNIX)?;
    let callbacks = build_protocol_callbacks(py, &protocol)?;
    let context_needs_run = context_needs_run && callbacks.stream_reader_fast_path.is_none();
    let writer_fd =
        fd_ops::dup_raw_fd(raw_fd).map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
    let direct_writer = unsafe { StdUnixStream::from_raw_fd(raw_fd_for_std(writer_fd)?) };
    direct_writer
        .set_nonblocking(true)
        .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
    let writer = stream
        .try_clone()
        .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
    let (writer_tx, writer_rx) = mpsc::channel();
    let core = Arc::new(StreamTransportCore {
        loop_core,
        loop_obj,
        state: Mutex::new(StreamTransportState {
            io_fd: Some(raw_fd),
            runtime_socket_io: true,
            protocol,
            callbacks,
            context,
            context_needs_run,
            extra,
            closing: false,
            read_paused: false,
            reading: true,
            writable: true,
            write_eof_requested: false,
            can_write_eof: true,
            close_on_write_eof: false,
            lost_called: false,
            writer_registered: false,
            write_buffer_size: 0,
            write_buffer_high_water: DEFAULT_WRITE_BUFFER_HIGH_WATER,
            write_buffer_low_water: DEFAULT_WRITE_BUFFER_LOW_WATER,
            protocol_write_paused: false,
            detached: false,
            server,
        }),
        pending_read_events: Mutex::new(VecDeque::new()),
        read_events_scheduled: AtomicBool::new(false),
        writer_tx,
        direct_writer: Some(Mutex::new(TaskedDirectWriter::Unix(direct_writer))),
        lazy_writer: Mutex::new(Some(LazyWriterConfig {
            target: WriterTarget::Unix(writer),
            writer_rx,
        })),
        workers: Mutex::new(Vec::new()),
    });

    let transport = Py::new(
        py,
        PyStreamTransport {
            core: Arc::clone(&core),
        },
    )?;
    core.connection_made(transport.clone_ref(py))?;
    if let Some(server) = core.server_ref().and_then(|weak| weak.upgrade()) {
        server.connection_opened();
    }

    #[cfg(target_os = "linux")]
    let _ = core.loop_core.send_command(LoopCommand::StartSocketReader {
        fd: raw_fd,
        core: Arc::clone(&core),
        reader: ReaderTarget::Unix(stream),
    });
    #[cfg(not(target_os = "linux"))]
    spawn_reader_worker(Arc::clone(&core), ReaderTarget::Unix(stream));
    Ok(transport)
}

fn merge_extra(
    mut base: HashMap<String, Py<PyAny>>,
    extra: HashMap<String, Py<PyAny>>,
) -> HashMap<String, Py<PyAny>> {
    base.extend(extra);
    base
}

fn spawn_tls_client_transport(
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
        TlsConnectionKind::Client(connection),
        tls_extra(py, &tls.ssl_context),
        tls.handshake_timeout,
        tls.shutdown_timeout,
        server,
        call_connection_made,
    )
}

fn spawn_tls_server_transport(
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
        TlsConnectionKind::Server(connection),
        tls_extra(py, &tls.ssl_context),
        tls.handshake_timeout,
        tls.shutdown_timeout,
        server,
        call_connection_made,
    )
}

fn spawn_tls_transport(
    py: Python<'_>,
    spawn_context: TransportSpawnContext,
    stream: StreamKind,
    connection: TlsConnectionKind,
    tls_extra: HashMap<String, Py<PyAny>>,
    handshake_timeout: Duration,
    shutdown_timeout: Duration,
    server: Option<Weak<ServerCore>>,
    call_connection_made: bool,
) -> PyResult<Py<PyStreamTransport>> {
    let TransportSpawnContext {
        loop_core,
        loop_obj,
        protocol,
        context,
        context_needs_run,
    } = spawn_context;
    let extra = match &stream {
        StreamKind::Tcp(stream) => merge_extra(
            make_stream_extra(py, tcp_stream_raw_fd(stream), tcp_family(stream))?,
            tls_extra,
        ),
        #[cfg(unix)]
        StreamKind::Unix(stream) => merge_extra(
            make_stream_extra(py, unix_raw_fd(stream.as_raw_fd()), libc::AF_UNIX)?,
            tls_extra,
        ),
    };
    let callbacks = build_protocol_callbacks(py, &protocol)?;
    let context_needs_run = context_needs_run && callbacks.stream_reader_fast_path.is_none();
    let (writer_tx, writer_rx) = mpsc::channel();
    let stream_fd = stream.fd();
    let tls_state = Arc::new(Mutex::new(TlsIoState {
        stream,
        connection,
        shutdown_timeout,
    }));

    py.detach(|| complete_tls_handshake(&tls_state, handshake_timeout))
        .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;

    let core = Arc::new(StreamTransportCore {
        loop_core,
        loop_obj,
        state: Mutex::new(StreamTransportState {
            io_fd: Some(stream_fd),
            runtime_socket_io: true,
            protocol,
            callbacks,
            context,
            context_needs_run,
            extra,
            closing: false,
            read_paused: false,
            reading: true,
            writable: true,
            write_eof_requested: false,
            can_write_eof: false,
            close_on_write_eof: false,
            lost_called: false,
            writer_registered: false,
            write_buffer_size: 0,
            write_buffer_high_water: DEFAULT_WRITE_BUFFER_HIGH_WATER,
            write_buffer_low_water: DEFAULT_WRITE_BUFFER_LOW_WATER,
            protocol_write_paused: false,
            detached: false,
            server,
        }),
        pending_read_events: Mutex::new(VecDeque::new()),
        read_events_scheduled: AtomicBool::new(false),
        writer_tx,
        direct_writer: None,
        lazy_writer: Mutex::new(None),
        workers: Mutex::new(Vec::new()),
    });

    let transport = Py::new(
        py,
        PyStreamTransport {
            core: Arc::clone(&core),
        },
    )?;
    if call_connection_made {
        core.connection_made(transport.clone_ref(py))?;
    }
    if let Some(server) = core.server_ref().and_then(|weak| weak.upgrade()) {
        server.connection_opened();
    }

    spawn_tls_reader_worker(Arc::clone(&core), Arc::clone(&tls_state));
    spawn_tls_writer_worker(core, tls_state, writer_rx);
    Ok(transport)
}

pub fn create_server(py: Python<'_>, params: ServerCreateParams) -> PyResult<Py<PyServer>> {
    profiling::scope!("stream.create_server");
    let ServerCreateParams {
        loop_core,
        loop_obj,
        protocol_factory,
        context,
        context_needs_run,
        sockets,
        listeners,
        cleanup_path,
        tls,
    } = params;
    let accept_tasks = Vec::with_capacity(listeners.len());
    Py::new(
        py,
        PyServer {
            core: Arc::new(ServerCore {
                loop_core,
                loop_obj,
                protocol_factory,
                context,
                context_needs_run,
                sockets,
                state: Mutex::new(ServerState {
                    closed: false,
                    serving: false,
                    listeners,
                }),
                accept_tasks: Mutex::new(accept_tasks),
                accept_fds: Mutex::new(Vec::new()),
                active_connections: AtomicUsize::new(0),
                closed_notify: AsyncEvent::new(),
                cleanup_path,
                tls,
            }),
        },
    )
}

pub fn tcp_server_listener(listener: StdTcpListener) -> ServerListener {
    ServerListener::Tcp(listener)
}

#[cfg(unix)]
pub fn unix_server_listener(listener: StdUnixListener) -> ServerListener {
    ServerListener::Unix(listener)
}

fn run_tcp_accept_loop(server: Arc<ServerCore>, listener: StdTcpListener, stop: Arc<AtomicBool>) {
    profiling::scope!("stream.run_tcp_accept_loop");
    loop {
        if stop.load(Ordering::Acquire) || server.is_closed() {
            return;
        }

        match fd_ops::poll_fd(tcp_listener_raw_fd(&listener), true, false, 50) {
            Ok((false, _)) => continue,
            Ok((true, _)) => {}
            Err(err) => {
                if !server.is_closed() {
                    server.report_error(
                        PyRuntimeError::new_err(err.to_string()),
                        "TCP server accept failed",
                    );
                }
                return;
            }
        }

        loop {
            match listener.accept() {
                Ok((stream, _addr)) => {
                    let _ = stream.set_nonblocking(true);
                    let _ = stream.set_nodelay(true);
                    let result = Python::try_attach(|py| -> PyResult<_> {
                        let protocol = server.create_protocol_with_py(py)?;
                        let context = server.context.clone_ref(py);
                        let spawn_context = TransportSpawnContext {
                            loop_core: Arc::clone(&server.loop_core),
                            loop_obj: server.loop_obj.clone_ref(py),
                            protocol,
                            context,
                            context_needs_run: server.context_needs_run,
                        };
                        if let Some(tls) = server.tls.as_ref() {
                            spawn_tls_server_transport(
                                py,
                                spawn_context,
                                StreamKind::Tcp(stream),
                                ServerTlsSettings {
                                    config: Arc::clone(&tls.config),
                                    handshake_timeout: tls.handshake_timeout,
                                    shutdown_timeout: tls.shutdown_timeout,
                                    ssl_context: tls.ssl_context.clone_ref(py),
                                },
                                Some(Arc::downgrade(&server)),
                                true,
                            )
                        } else {
                            spawn_tcp_transport(
                                py,
                                spawn_context,
                                stream,
                                Some(Arc::downgrade(&server)),
                            )
                        }
                    });
                    match result {
                        Some(Ok(_)) => {}
                        Some(Err(err)) => {
                            server.report_error(err, "failed to accept TCP connection")
                        }
                        None => return,
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(err) => {
                    if !server.is_closed() {
                        server.report_error(
                            PyRuntimeError::new_err(err.to_string()),
                            "TCP server accept failed",
                        );
                    }
                    return;
                }
            }
        }
    }
}

#[cfg(target_os = "linux")]
pub(crate) async fn run_server_accept_task(server: Arc<ServerCore>, listener: ServerListener) {
    profiling::scope!("stream.run_server_accept_task");
    match listener {
        ServerListener::Tcp(listener) => run_tcp_accept_task(server, listener).await,
        #[cfg(unix)]
        ServerListener::Unix(listener) => run_unix_accept_task(server, listener).await,
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn run_server_accept_blocking(
    server: Arc<ServerCore>,
    listener: ServerListener,
    stop: Arc<AtomicBool>,
) {
    match listener {
        ServerListener::Tcp(listener) => run_tcp_accept_loop(server, listener, stop),
        #[cfg(unix)]
        ServerListener::Unix(listener) => run_unix_accept_loop(server, listener, stop),
    }
}

#[cfg(target_os = "linux")]
async fn run_tcp_accept_task(server: Arc<ServerCore>, listener: StdTcpListener) {
    profiling::scope!("stream.run_tcp_accept_task");
    let poll_fd = listener
        .try_clone()
        .and_then(|listener| PollFd::new(listener));

    let Ok(poll_fd) = poll_fd else {
        return;
    };

    loop {
        if server.is_closed() {
            return;
        }

        if let Err(err) = poll_fd.accept_ready().await {
            if !server.is_closed() {
                server.report_error(
                    PyRuntimeError::new_err(err.to_string()),
                    "TCP server accept failed",
                );
            }
            return;
        }

        loop {
            let accept_result = listener.accept();
            match accept_result {
                Ok((stream, _addr)) => {
                    let _ = stream.set_nonblocking(true);
                    let _ = stream.set_nodelay(true);
                    let result = Python::try_attach(|py| -> PyResult<_> {
                        let protocol = server.create_protocol_with_py(py)?;
                        let context = server.context.clone_ref(py);
                        let spawn_context = TransportSpawnContext {
                            loop_core: Arc::clone(&server.loop_core),
                            loop_obj: server.loop_obj.clone_ref(py),
                            protocol,
                            context,
                            context_needs_run: server.context_needs_run,
                        };
                        if let Some(tls) = server.tls.as_ref() {
                            spawn_tls_server_transport(
                                py,
                                spawn_context,
                                StreamKind::Tcp(stream),
                                ServerTlsSettings {
                                    config: Arc::clone(&tls.config),
                                    handshake_timeout: tls.handshake_timeout,
                                    shutdown_timeout: tls.shutdown_timeout,
                                    ssl_context: tls.ssl_context.clone_ref(py),
                                },
                                Some(Arc::downgrade(&server)),
                                true,
                            )
                        } else {
                            spawn_tcp_transport(
                                py,
                                spawn_context,
                                stream,
                                Some(Arc::downgrade(&server)),
                            )
                        }
                    });
                    match result {
                        Some(Ok(_)) => {}
                        Some(Err(err)) => {
                            server.report_error(err, "failed to accept TCP connection")
                        }
                        None => return,
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(err) => {
                    if !server.is_closed() {
                        server.report_error(
                            PyRuntimeError::new_err(err.to_string()),
                            "TCP server accept failed",
                        );
                    }
                    return;
                }
            }
        }
    }
}

#[cfg(windows)]
pub(crate) async fn run_tcp_accept_task(server: Arc<ServerCore>, listener: StdTcpListener) {
    let listener = match VibeTcpListener::from_std(listener) {
        Ok(listener) => listener,
        Err(err) => {
            if !server.is_closed() {
                server.report_error(
                    PyRuntimeError::new_err(err.to_string()),
                    "TCP server accept failed",
                );
            }
            return;
        }
    };

    loop {
        if server.is_closed() {
            return;
        }

        match listener.accept().await {
            Ok((stream, _addr)) => {
                let raw = stream.into_raw_socket();
                let stream = unsafe { StdTcpStream::from_raw_socket(raw) };
                let _ = stream.set_nonblocking(true);
                let _ = stream.set_nodelay(true);
                let result = Python::try_attach(|py| -> PyResult<_> {
                    let protocol = server.create_protocol_with_py(py)?;
                    let context = server.context.clone_ref(py);
                    let spawn_context = TransportSpawnContext {
                        loop_core: Arc::clone(&server.loop_core),
                        loop_obj: server.loop_obj.clone_ref(py),
                        protocol,
                        context,
                        context_needs_run: server.context_needs_run,
                    };
                    if let Some(tls) = server.tls.as_ref() {
                        spawn_tls_server_transport(
                            py,
                            spawn_context,
                            StreamKind::Tcp(stream),
                            ServerTlsSettings {
                                config: Arc::clone(&tls.config),
                                handshake_timeout: tls.handshake_timeout,
                                shutdown_timeout: tls.shutdown_timeout,
                                ssl_context: tls.ssl_context.clone_ref(py),
                            },
                            Some(Arc::downgrade(&server)),
                            true,
                        )
                    } else {
                        spawn_tcp_transport(
                            py,
                            spawn_context,
                            stream,
                            Some(Arc::downgrade(&server)),
                        )
                    }
                });
                match result {
                    Some(Ok(_)) => {}
                    Some(Err(err)) => server.report_error(err, "failed to accept TCP connection"),
                    None => return,
                }
            }
            Err(err) => {
                if !server.is_closed() {
                    server.report_error(
                        PyRuntimeError::new_err(err.to_string()),
                        "TCP server accept failed",
                    );
                }
                return;
            }
        }
    }
}

#[cfg(unix)]
fn run_unix_accept_loop(server: Arc<ServerCore>, listener: StdUnixListener, stop: Arc<AtomicBool>) {
    loop {
        if stop.load(Ordering::Acquire) || server.is_closed() {
            return;
        }

        match fd_ops::poll_fd(listener.as_raw_fd() as fd_ops::RawFd, true, false, 50) {
            Ok((false, _)) => continue,
            Ok((true, _)) => {}
            Err(err) => {
                if !server.is_closed() {
                    server.report_error(
                        PyRuntimeError::new_err(err.to_string()),
                        "Unix server accept failed",
                    );
                }
                return;
            }
        }

        loop {
            match listener.accept() {
                Ok((stream, _addr)) => {
                    let _ = stream.set_nonblocking(true);
                    let result = Python::try_attach(|py| -> PyResult<_> {
                        let protocol = server.create_protocol_with_py(py)?;
                        let context = server.context.clone_ref(py);
                        let spawn_context = TransportSpawnContext {
                            loop_core: Arc::clone(&server.loop_core),
                            loop_obj: server.loop_obj.clone_ref(py),
                            protocol,
                            context,
                            context_needs_run: server.context_needs_run,
                        };
                        if let Some(tls) = server.tls.as_ref() {
                            spawn_tls_server_transport(
                                py,
                                spawn_context,
                                StreamKind::Unix(stream),
                                ServerTlsSettings {
                                    config: Arc::clone(&tls.config),
                                    handshake_timeout: tls.handshake_timeout,
                                    shutdown_timeout: tls.shutdown_timeout,
                                    ssl_context: tls.ssl_context.clone_ref(py),
                                },
                                Some(Arc::downgrade(&server)),
                                true,
                            )
                        } else {
                            spawn_unix_transport(
                                py,
                                spawn_context,
                                stream,
                                Some(Arc::downgrade(&server)),
                            )
                        }
                    });
                    match result {
                        Some(Ok(_)) => {}
                        Some(Err(err)) => {
                            server.report_error(err, "failed to accept Unix connection")
                        }
                        None => return,
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(err) => {
                    if !server.is_closed() {
                        server.report_error(
                            PyRuntimeError::new_err(err.to_string()),
                            "Unix server accept failed",
                        );
                    }
                    return;
                }
            }
        }
    }
}

#[cfg(target_os = "linux")]
async fn run_unix_accept_task(server: Arc<ServerCore>, listener: StdUnixListener) {
    let Ok(poll_fd) = listener
        .try_clone()
        .and_then(|listener| PollFd::new(listener))
    else {
        return;
    };

    loop {
        if server.is_closed() {
            return;
        }

        if let Err(err) = poll_fd.accept_ready().await {
            if !server.is_closed() {
                server.report_error(
                    PyRuntimeError::new_err(err.to_string()),
                    "Unix server accept failed",
                );
            }
            return;
        }

        loop {
            match listener.accept() {
                Ok((stream, _addr)) => {
                    let _ = stream.set_nonblocking(true);
                    let result = Python::try_attach(|py| -> PyResult<_> {
                        let protocol = server.create_protocol_with_py(py)?;
                        let context = server.context.clone_ref(py);
                        let spawn_context = TransportSpawnContext {
                            loop_core: Arc::clone(&server.loop_core),
                            loop_obj: server.loop_obj.clone_ref(py),
                            protocol,
                            context,
                            context_needs_run: server.context_needs_run,
                        };
                        if let Some(tls) = server.tls.as_ref() {
                            spawn_tls_server_transport(
                                py,
                                spawn_context,
                                StreamKind::Unix(stream),
                                ServerTlsSettings {
                                    config: Arc::clone(&tls.config),
                                    handshake_timeout: tls.handshake_timeout,
                                    shutdown_timeout: tls.shutdown_timeout,
                                    ssl_context: tls.ssl_context.clone_ref(py),
                                },
                                Some(Arc::downgrade(&server)),
                                true,
                            )
                        } else {
                            spawn_unix_transport(
                                py,
                                spawn_context,
                                stream,
                                Some(Arc::downgrade(&server)),
                            )
                        }
                    });
                    match result {
                        Some(Ok(_)) => {}
                        Some(Err(err)) => {
                            server.report_error(err, "failed to accept Unix connection")
                        }
                        None => return,
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(err) => {
                    if !server.is_closed() {
                        server.report_error(
                            PyRuntimeError::new_err(err.to_string()),
                            "Unix server accept failed",
                        );
                    }
                    return;
                }
            }
        }
    }
}

fn spawn_reader_worker(core: Arc<StreamTransportCore>, reader: ReaderTarget) {
    let thread_core = Arc::clone(&core);
    let worker = WorkerThread::spawn("rsloop-stream-reader".to_owned(), move |stop| {
        run_stream_reader(thread_core, reader, stop)
    });
    core.register_worker(worker);
}

fn spawn_tls_reader_worker(core: Arc<StreamTransportCore>, tls_state: Arc<Mutex<TlsIoState>>) {
    let thread_core = Arc::clone(&core);
    let worker = WorkerThread::spawn("rsloop-tls-reader".to_owned(), move |stop| {
        run_tls_reader(thread_core, tls_state, stop)
    });
    core.register_worker(worker);
}

fn spawn_writer_worker(
    core: Arc<StreamTransportCore>,
    writer: WriterTarget,
    writer_rx: Receiver<WriterCommand>,
) {
    profiling::scope!("stream.spawn_writer_worker");
    let thread_core = Arc::clone(&core);
    let worker = WorkerThread::spawn("rsloop-stream-writer".to_owned(), move |stop| {
        run_stream_writer(thread_core, writer, writer_rx, stop)
    });
    core.register_worker(worker);
}

fn spawn_tls_writer_worker(
    core: Arc<StreamTransportCore>,
    tls_state: Arc<Mutex<TlsIoState>>,
    writer_rx: Receiver<WriterCommand>,
) {
    let thread_core = Arc::clone(&core);
    let worker = WorkerThread::spawn("rsloop-tls-writer".to_owned(), move |stop| {
        run_tls_writer(thread_core, tls_state, writer_rx, stop)
    });
    core.register_worker(worker);
}

fn complete_tls_handshake(tls_state: &Arc<Mutex<TlsIoState>>, timeout: Duration) -> io::Result<()> {
    profiling::scope!("stream.complete_tls_handshake");
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if std::time::Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "TLS handshake timed out",
            ));
        }

        let mut state = tls_state.lock().expect("poisoned tls state");
        if !state.connection.is_handshaking() {
            if state.connection.wants_write() {
                flush_tls_io_locked(&mut state)?;
            }
            return Ok(());
        }

        if state.connection.wants_write() {
            flush_tls_io_locked(&mut state)?;
            continue;
        }

        if state.connection.wants_read() {
            let fd = state.fd();
            let pollable = state.pollable();
            drop(state);
            wait_socket_ready(fd, pollable, true, false)?;
            let mut state = tls_state.lock().expect("poisoned tls state");
            let n = state.read_tls()?;
            if n == 0 && state.connection.is_handshaking() {
                return Err(io::Error::new(
                    io::ErrorKind::UnexpectedEof,
                    "TLS handshake ended before completion",
                ));
            }
            state
                .connection
                .process_new_packets()
                .map_err(tls_io_error)?;
            continue;
        }

        thread::sleep(Duration::from_millis(10));
    }
}

fn run_stream_reader(
    core: Arc<StreamTransportCore>,
    mut reader: ReaderTarget,
    stop: Arc<AtomicBool>,
) {
    profiling::scope!("stream.run_stream_reader");
    let mut buf = [0_u8; 65_536];

    loop {
        if stop.load(Ordering::Acquire) {
            return;
        }
        if core.is_closing() {
            core.enqueue_pending_read_event(PendingReadEvent::ConnectionLost(None));
            return;
        }

        core.wait_until_readable();
        if core.is_closing() {
            core.enqueue_pending_read_event(PendingReadEvent::ConnectionLost(None));
            return;
        }

        if reader.pollable() {
            match fd_ops::poll_fd(reader.fd(), true, false, 50) {
                Ok((false, _)) => continue,
                Ok((true, _)) => {}
                Err(err) => {
                    core.enqueue_pending_read_event(PendingReadEvent::ConnectionLost(Some(
                        err.to_string(),
                    )));
                    return;
                }
            }
        }

        match reader.read(&mut buf) {
            Ok(0) => {
                core.enqueue_pending_read_event(PendingReadEvent::Eof);
                return;
            }
            Ok(n) => core
                .enqueue_pending_read_event(PendingReadEvent::Data(Box::<[u8]>::from(&buf[..n]))),
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => continue,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => {
                core.enqueue_pending_read_event(PendingReadEvent::ConnectionLost(Some(
                    err.to_string(),
                )));
                return;
            }
        }
    }
}

#[cfg(target_os = "linux")]
pub(crate) async fn run_socket_reader_task(
    core: Arc<StreamTransportCore>,
    mut reader: ReaderTarget,
) {
    profiling::scope!("stream.run_socket_reader_task");
    let Ok(poll_fd) = poll_fd_from_raw(reader.fd()) else {
        core.enqueue_pending_read_event(PendingReadEvent::ConnectionLost(Some(
            "failed to attach socket reader".to_owned(),
        )));
        return;
    };
    let mut buf = [0_u8; 65_536];

    loop {
        if core.is_closing() {
            core.enqueue_pending_read_event(PendingReadEvent::ConnectionLost(None));
            return;
        }

        while !core.is_closing() && !core.is_reading() {
            compio_sleep(Duration::from_millis(1)).await;
        }
        if core.is_closing() {
            core.enqueue_pending_read_event(PendingReadEvent::ConnectionLost(None));
            return;
        }

        if let Err(err) = poll_fd.read_ready().await {
            core.enqueue_pending_read_event(PendingReadEvent::ConnectionLost(Some(
                err.to_string(),
            )));
            return;
        }

        match reader.read(&mut buf) {
            Ok(0) => {
                core.enqueue_pending_read_event(PendingReadEvent::Eof);
                return;
            }
            Ok(n) => core
                .enqueue_pending_read_event(PendingReadEvent::Data(Box::<[u8]>::from(&buf[..n]))),
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => continue,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => {
                core.enqueue_pending_read_event(PendingReadEvent::ConnectionLost(Some(
                    err.to_string(),
                )));
                return;
            }
        }
    }
}

#[cfg(windows)]
pub(crate) async fn run_tcp_socket_reader_task(
    core: Arc<StreamTransportCore>,
    stream: StdTcpStream,
) {
    profiling::scope!("stream.run_tcp_socket_reader_task");
    let mut reader = match VibePollTcpStream::from_std(stream) {
        Ok(reader) => reader,
        Err(err) => {
            core.enqueue_pending_read_event(PendingReadEvent::ConnectionLost(Some(
                err.to_string(),
            )));
            return;
        }
    };
    let mut buf = vec![0_u8; 65_536];

    loop {
        if core.is_closing() {
            core.enqueue_pending_read_event(PendingReadEvent::ConnectionLost(None));
            return;
        }

        while !core.is_closing() && !core.is_reading() {
            thread::sleep(Duration::from_millis(1));
        }
        if core.is_closing() {
            core.enqueue_pending_read_event(PendingReadEvent::ConnectionLost(None));
            return;
        }

        match reader.read(&mut buf).await {
            Ok(0) => {
                core.enqueue_pending_read_event(PendingReadEvent::Eof);
                return;
            }
            Ok(n) => core
                .enqueue_pending_read_event(PendingReadEvent::Data(Box::<[u8]>::from(&buf[..n]))),
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => continue,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => {
                core.enqueue_pending_read_event(PendingReadEvent::ConnectionLost(Some(
                    err.to_string(),
                )));
                return;
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn run_socket_reader_blocking(
    core: Arc<StreamTransportCore>,
    reader: ReaderTarget,
    stop: Arc<AtomicBool>,
) {
    profiling::scope!("stream.run_socket_reader_blocking");
    run_stream_reader(core, reader, stop)
}

fn run_tls_reader(
    core: Arc<StreamTransportCore>,
    tls_state: Arc<Mutex<TlsIoState>>,
    stop: Arc<AtomicBool>,
) {
    profiling::scope!("stream.run_tls_reader");
    let mut plaintext = [0_u8; 65_536];

    loop {
        if stop.load(Ordering::Acquire) {
            return;
        }
        if core.is_closing() {
            core.enqueue_pending_read_event(PendingReadEvent::ConnectionLost(None));
            return;
        }

        core.wait_until_readable();
        if core.is_closing() {
            core.enqueue_pending_read_event(PendingReadEvent::ConnectionLost(None));
            return;
        }

        {
            let mut state = tls_state.lock().expect("poisoned tls state");
            let mut drained_buffered_plaintext = false;
            loop {
                match state.connection.reader_read(&mut plaintext) {
                    Ok(0) => break,
                    Ok(n) => {
                        drained_buffered_plaintext = true;
                        core.enqueue_pending_read_event(PendingReadEvent::Data(Box::<[u8]>::from(
                            &plaintext[..n],
                        )));
                    }
                    Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                    Err(err) => {
                        core.enqueue_pending_read_event(PendingReadEvent::ConnectionLost(Some(
                            err.to_string(),
                        )));
                        return;
                    }
                }
            }
            if drained_buffered_plaintext {
                continue;
            }
        }

        let (fd, pollable) = {
            let state = tls_state.lock().expect("poisoned tls state");
            (state.fd(), state.pollable())
        };
        if let Err(err) = wait_socket_ready(fd, pollable, true, false) {
            core.enqueue_pending_read_event(PendingReadEvent::ConnectionLost(Some(
                err.to_string(),
            )));
            return;
        }

        let mut state = tls_state.lock().expect("poisoned tls state");
        match state.read_tls() {
            Ok(0) => {
                match state.connection.process_new_packets().map_err(tls_io_error) {
                    Ok(()) => {}
                    Err(err) => {
                        core.enqueue_pending_read_event(PendingReadEvent::ConnectionLost(Some(
                            err.to_string(),
                        )));
                        return;
                    }
                }
                let mut saw_data = false;
                loop {
                    match state.connection.reader_read(&mut plaintext) {
                        Ok(0) => break,
                        Ok(n) => {
                            saw_data = true;
                            core.enqueue_pending_read_event(PendingReadEvent::Data(
                                Box::<[u8]>::from(&plaintext[..n]),
                            ));
                        }
                        Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                        Err(err) => {
                            core.enqueue_pending_read_event(PendingReadEvent::ConnectionLost(
                                Some(err.to_string()),
                            ));
                            return;
                        }
                    }
                }
                if !saw_data {
                    core.enqueue_pending_read_event(PendingReadEvent::Eof);
                    return;
                }
            }
            Ok(_) => {
                if let Err(err) = state.connection.process_new_packets().map_err(tls_io_error) {
                    core.enqueue_pending_read_event(PendingReadEvent::ConnectionLost(Some(
                        err.to_string(),
                    )));
                    return;
                }
                if let Err(err) = flush_tls_io_locked(&mut state) {
                    core.enqueue_pending_read_event(PendingReadEvent::ConnectionLost(Some(
                        err.to_string(),
                    )));
                    return;
                }
                loop {
                    match state.connection.reader_read(&mut plaintext) {
                        Ok(0) => break,
                        Ok(n) => core.enqueue_pending_read_event(PendingReadEvent::Data(
                            Box::<[u8]>::from(&plaintext[..n]),
                        )),
                        Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                        Err(err) => {
                            core.enqueue_pending_read_event(PendingReadEvent::ConnectionLost(
                                Some(err.to_string()),
                            ));
                            return;
                        }
                    }
                }
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => continue,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => {
                core.enqueue_pending_read_event(PendingReadEvent::ConnectionLost(Some(
                    err.to_string(),
                )));
                return;
            }
        }
    }
}

fn run_stream_writer(
    core: Arc<StreamTransportCore>,
    mut writer: WriterTarget,
    writer_rx: Receiver<WriterCommand>,
    stop: Arc<AtomicBool>,
) {
    profiling::scope!("stream.run_stream_writer");
    let mut pending_command = None;

    loop {
        if stop.load(Ordering::Acquire) {
            return;
        }
        let command = match pending_command.take() {
            Some(command) => command,
            None => match writer_rx.recv() {
                Ok(command) => command,
                Err(_) => break,
            },
        };

        match command {
            WriterCommand::Data(mut data) => {
                let buffered_len = data.remaining().len();
                if let Err(err) = write_all_owned(&mut writer, &mut data) {
                    let _ = core.connection_lost(Some(PyRuntimeError::new_err(err.to_string())));
                    return;
                }
                core.record_write_buffer_drained(buffered_len);

                loop {
                    match writer_rx.try_recv() {
                        Ok(WriterCommand::Data(mut next)) => {
                            let buffered_len = next.remaining().len();
                            if let Err(err) = write_all_owned(&mut writer, &mut next) {
                                let _ = core.connection_lost(Some(PyRuntimeError::new_err(
                                    err.to_string(),
                                )));
                                return;
                            }
                            core.record_write_buffer_drained(buffered_len);
                        }
                        Ok(command) => {
                            pending_command = Some(command);
                            break;
                        }
                        Err(TryRecvError::Empty) => {
                            core.set_write_backpressure_active(false);
                            break;
                        }
                        Err(TryRecvError::Disconnected) => {
                            core.set_write_backpressure_active(false);
                            let _ = core.connection_lost(None);
                            return;
                        }
                    }
                }

                if pending_command.is_none() {
                    core.set_write_backpressure_active(false);
                }
            }
            WriterCommand::WriteEof => {
                let _ = writer.shutdown_write();
                if core.close_on_write_eof() {
                    let _ = core.connection_lost(None);
                    return;
                }
            }
            WriterCommand::Close => {
                let _ = writer.shutdown_close();
                let _ = core.connection_lost(None);
                return;
            }
            WriterCommand::Abort => {
                let _ = writer.shutdown_close();
                let _ = core.connection_lost(None);
                return;
            }
            WriterCommand::Stop => return,
        }
    }

    let _ = core.connection_lost(None);
}

fn run_tls_writer(
    core: Arc<StreamTransportCore>,
    tls_state: Arc<Mutex<TlsIoState>>,
    writer_rx: Receiver<WriterCommand>,
    stop: Arc<AtomicBool>,
) {
    profiling::scope!("stream.run_tls_writer");
    let mut pending_command = None;

    loop {
        if stop.load(Ordering::Acquire) {
            return;
        }
        let command = match pending_command.take() {
            Some(command) => command,
            None => match writer_rx.recv() {
                Ok(command) => command,
                Err(_) => break,
            },
        };

        match command {
            WriterCommand::Data(data) => {
                let buffered_len = data.remaining().len();
                let result = {
                    let mut state = tls_state.lock().expect("poisoned tls state");
                    match state.connection.writer_write_all(data.remaining()) {
                        Ok(()) => flush_tls_io_locked(&mut state),
                        Err(err) => Err(err),
                    }
                };
                if let Err(err) = result {
                    let _ = core.connection_lost(Some(PyRuntimeError::new_err(err.to_string())));
                    return;
                }
                core.record_write_buffer_drained(buffered_len);

                loop {
                    match writer_rx.try_recv() {
                        Ok(WriterCommand::Data(next)) => {
                            let buffered_len = next.remaining().len();
                            let result = {
                                let mut state = tls_state.lock().expect("poisoned tls state");
                                match state.connection.writer_write_all(next.remaining()) {
                                    Ok(()) => flush_tls_io_locked(&mut state),
                                    Err(err) => Err(err),
                                }
                            };
                            if let Err(err) = result {
                                let _ = core.connection_lost(Some(PyRuntimeError::new_err(
                                    err.to_string(),
                                )));
                                return;
                            }
                            core.record_write_buffer_drained(buffered_len);
                        }
                        Ok(command) => {
                            pending_command = Some(command);
                            break;
                        }
                        Err(TryRecvError::Empty) => {
                            core.set_write_backpressure_active(false);
                            break;
                        }
                        Err(TryRecvError::Disconnected) => {
                            core.set_write_backpressure_active(false);
                            let _ = core.connection_lost(None);
                            return;
                        }
                    }
                }

                if pending_command.is_none() {
                    core.set_write_backpressure_active(false);
                }
            }
            WriterCommand::WriteEof => {}
            WriterCommand::Close => {
                let close_result = {
                    let mut state = tls_state.lock().expect("poisoned tls state");
                    let shutdown_timeout = state.shutdown_timeout;
                    state.connection.send_close_notify();
                    let result = flush_tls_close_io_locked(&mut state, shutdown_timeout);
                    let _ = state.shutdown_close();
                    result
                };
                match close_result {
                    Ok(()) => {
                        let _ = core.connection_lost(None);
                    }
                    Err(err) if err.kind() == io::ErrorKind::TimedOut => {
                        let _ = core.connection_lost(Some(PyTimeoutError::new_err(
                            "SSL shutdown timed out",
                        )));
                    }
                    Err(err) => {
                        let _ =
                            core.connection_lost(Some(PyRuntimeError::new_err(err.to_string())));
                    }
                }
                return;
            }
            WriterCommand::Abort => {
                {
                    let state = tls_state.lock().expect("poisoned tls state");
                    let _ = state.shutdown_close();
                }
                let _ = core.connection_lost(None);
                return;
            }
            WriterCommand::Stop => return,
        }
    }

    let _ = core.connection_lost(None);
}

fn write_all_owned(writer: &mut WriterTarget, data: &mut OwnedWriteBuffer) -> io::Result<()> {
    while !data.is_empty() {
        match writer.write(data.remaining()) {
            Ok(0) => {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "failed to write buffered transport data",
                ));
            }
            Ok(written) => data.advance(written),
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                if let Some(fd) = writer.fd().filter(|_| writer.pollable()) {
                    loop {
                        match fd_ops::poll_fd(fd, false, true, 50) {
                            Ok((false, true)) => break,
                            Ok(_) => continue,
                            Err(err) => return Err(err),
                        }
                    }
                } else {
                    thread::sleep(Duration::from_millis(10));
                }
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    }

    Ok(())
}

fn flush_tls_io_locked(state: &mut TlsIoState) -> io::Result<()> {
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

fn flush_tls_close_io_locked(state: &mut TlsIoState, timeout: Duration) -> io::Result<()> {
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

fn wait_socket_ready(fd: fd_ops::RawFd, pollable: bool, read: bool, write: bool) -> io::Result<()> {
    if pollable {
        loop {
            match fd_ops::poll_fd(fd, read, write, 50) {
                Ok((read_ready, write_ready))
                    if (!read || read_ready) && (!write || write_ready) =>
                {
                    return Ok(());
                }
                Ok(_) => continue,
                Err(err) => return Err(err),
            }
        }
    }

    thread::sleep(Duration::from_millis(10));
    Ok(())
}

fn wait_socket_ready_until(
    fd: fd_ops::RawFd,
    pollable: bool,
    read: bool,
    write: bool,
    deadline: std::time::Instant,
) -> io::Result<()> {
    if pollable {
        loop {
            let now = std::time::Instant::now();
            if now >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "SSL shutdown timed out",
                ));
            }

            let remaining_ms = deadline
                .saturating_duration_since(now)
                .as_millis()
                .clamp(1, i32::MAX as u128) as i32;
            match fd_ops::poll_fd(fd, read, write, remaining_ms) {
                Ok((read_ready, write_ready))
                    if (!read || read_ready) && (!write || write_ready) =>
                {
                    return Ok(());
                }
                Ok(_) => continue,
                Err(err) => return Err(err),
            }
        }
    }

    if std::time::Instant::now() >= deadline {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "SSL shutdown timed out",
        ));
    }
    thread::sleep(Duration::from_millis(10));
    Ok(())
}

#[cfg(target_os = "linux")]
fn poll_fd_from_raw(fd: fd_ops::RawFd) -> io::Result<PollFd<OwnedFd>> {
    let dup = fd_ops::dup_raw_fd(fd)?;
    let dup: i32 = dup
        .try_into()
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "fd out of range"))?;
    PollFd::new(unsafe { OwnedFd::from_raw_fd(dup) })
}

fn tls_io_error(err: rustls::Error) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidData, err.to_string())
}

fn shutdown_tcp_stream(stream: &StdTcpStream, how: Shutdown) -> io::Result<()> {
    match stream.shutdown(how) {
        Ok(()) => Ok(()),
        Err(err)
            if matches!(
                err.kind(),
                io::ErrorKind::NotConnected | io::ErrorKind::BrokenPipe
            ) =>
        {
            Ok(())
        }
        Err(err) => Err(err),
    }
}

#[cfg(unix)]
fn shutdown_unix_stream(stream: &StdUnixStream, how: Shutdown) -> io::Result<()> {
    match stream.shutdown(how) {
        Ok(()) => Ok(()),
        Err(err)
            if matches!(
                err.kind(),
                io::ErrorKind::NotConnected | io::ErrorKind::BrokenPipe
            ) =>
        {
            Ok(())
        }
        Err(err) => Err(err),
    }
}

fn build_protocol_callbacks(py: Python<'_>, protocol: &Py<PyAny>) -> PyResult<ProtocolCallbacks> {
    let bound = protocol.bind(py);
    let data_received = match bound.getattr("data_received") {
        Ok(callback) => Some(callback.unbind()),
        Err(_) => None,
    };
    let eof_received = match bound.getattr("eof_received") {
        Ok(callback) => Some(callback.unbind()),
        Err(_) => None,
    };
    let get_buffer = match bound.getattr("get_buffer") {
        Ok(callback) => Some(callback.unbind()),
        Err(_) => None,
    };
    let buffer_updated = match bound.getattr("buffer_updated") {
        Ok(callback) => Some(callback.unbind()),
        Err(_) => None,
    };
    let stream_reader_fast_path = stream_reader_fast_path(py, bound)?;

    Ok(ProtocolCallbacks {
        connection_made: bound.getattr("connection_made")?.unbind(),
        data_received,
        eof_received,
        connection_lost: bound.getattr("connection_lost")?.unbind(),
        pause_writing: bound.getattr(python_names::pause_writing(py))?.unbind(),
        resume_writing: bound.getattr(python_names::resume_writing(py))?.unbind(),
        get_buffer,
        buffer_updated,
        stream_reader_fast_path,
    })
}

fn stream_reader_fast_path(
    py: Python<'_>,
    protocol: &Bound<'_, PyAny>,
) -> PyResult<Option<StreamReaderFastPath>> {
    if let Ok(native_protocol) = protocol.extract::<Py<PyFastStreamProtocol>>() {
        let reader = native_protocol.borrow(py).reader_ref(py);
        return Ok(Some(StreamReaderFastPath::Native {
            protocol: native_protocol,
            reader,
        }));
    }

    if let Ok(reader) = protocol.getattr("_rsloop_fast_reader") {
        if !reader.is_none() {
            let buffer = reader.getattr("_buffer")?;
            let limit = reader.getattr("_limit")?.extract::<usize>()?;
            return Ok(Some(StreamReaderFastPath::Generic {
                protocol: Some(protocol.clone().unbind()),
                reader: reader.unbind(),
                buffer: buffer.unbind(),
                limit,
            }));
        }
    }

    let asyncio_streams = py.import("asyncio.streams")?;
    let stream_reader_protocol_cls = asyncio_streams.getattr("StreamReaderProtocol")?;
    if !protocol.is_instance(&stream_reader_protocol_cls)? {
        return Ok(None);
    }

    let reader = protocol.getattr("_stream_reader")?;
    if reader.is_none() {
        return Ok(None);
    }

    let buffer = reader.getattr("_buffer")?;
    let limit = reader.getattr("_limit")?.extract::<usize>()?;

    Ok(Some(StreamReaderFastPath::Generic {
        protocol: Some(protocol.clone().unbind()),
        reader: reader.unbind(),
        buffer: buffer.unbind(),
        limit,
    }))
}

fn make_stream_extra(
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

pub fn remove_unix_socket_if_present(path: &str) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}
