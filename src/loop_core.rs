use std::cell::Cell;
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::ops::DerefMut;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::task::Waker;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::callbacks::{CallbackId, CallbackKind, ReadyCallback};
use crate::context::{capture_context, clear_running_loop, ensure_running_loop};
use crate::errors::handle_callback_error;
use crate::fd_ops::RawFd;
use crate::runtime::run_runtime_thread;
use crossbeam_channel::Sender;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PySet, PyTuple};

mod commands;
pub use commands::{
    LoopCommand, LoopFutureCommand, LoopIoCommand, LoopRunCommand, LoopSignalCommand,
    LoopTransportCommand, ReadyItem,
};

const READY_DRAIN_SLICE: usize = 64;

// One combined thread-local record instead of three separate `thread_local!`
// cells: each cell access costs a dynamic TLS lookup (`_tlv_get_addr` on
// macOS), and the local-enqueue fast path is hot enough for that to show up
// in profiles.
struct ActiveLoopTls {
    core: Cell<*const LoopCore>,
    ready_queue: Cell<*mut VecDeque<ReadyItem>>,
    drain_active: Cell<bool>,
}

thread_local! {
    static ACTIVE_LOOP_TLS: ActiveLoopTls = const {
        ActiveLoopTls {
            core: Cell::new(std::ptr::null()),
            ready_queue: Cell::new(std::ptr::null_mut()),
            drain_active: Cell::new(false),
        }
    };
}

// Bounded busy-wait before parking on the wake channel. Cross-thread wakeups
// (transport reader workers, the runtime thread) otherwise pay a full condvar
// sleep/wake cycle, which dominates ping-pong style I/O latency.
fn wake_spin_window() -> Duration {
    static WINDOW: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *WINDOW.get_or_init(|| {
        let micros = std::env::var("RSLOOP_WAKE_SPIN_US")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .unwrap_or(40);
        Duration::from_micros(micros.min(1_000))
    })
}

#[derive(Debug)]
pub enum LoopCoreError {
    Closed,
    Running,
    NotRunning,
    ChannelClosed,
    ThreadJoin,
}

impl fmt::Display for LoopCoreError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Closed => write!(f, "event loop is closed"),
            Self::Running => write!(f, "event loop is already running"),
            Self::NotRunning => write!(f, "event loop is not running"),
            Self::ChannelClosed => write!(f, "event loop runtime channel is closed"),
            Self::ThreadJoin => write!(f, "failed to join loop runtime thread"),
        }
    }
}

impl std::error::Error for LoopCoreError {}

pub struct SignalHandlerTemplate {
    pub callback: Py<PyAny>,
    pub args: Py<PyTuple>,
    pub context: Py<PyAny>,
    pub context_needs_run: bool,
}

/// One fd watch registration. `ready` is the single callback shared with the
/// watcher task; cancelling it neutralizes fires that are already queued.
/// `fileobj` keeps the registered file object alive and lets
/// remove_reader()/remove_writer() find the registration by identity even
/// after the file object has been closed (fileno() == -1).
pub struct FdWatch {
    pub fileobj: Py<PyAny>,
    pub ready: Arc<ReadyCallback>,
}

struct ActiveReadyDispatch {
    pending_ready: Arc<Mutex<VecDeque<ReadyItem>>>,
    wake_tx: std::sync::mpsc::Sender<()>,
    wake_pending: Arc<AtomicBool>,
}

pub struct LoopState {
    pub closed: bool,
    pub running: bool,
    pub stopping: bool,
    pub slow_callback_duration: f64,
    pub asyncgens_shutdown_called: bool,
    pub active_asyncgens: Option<Py<PySet>>,
    pub executor_shutdown_called: bool,
    pub signal_handlers: HashMap<i32, SignalHandlerTemplate>,
    pub previous_signal_handlers: HashMap<i32, Py<PyAny>>,
    pub reader_keepalive: HashMap<RawFd, FdWatch>,
    pub writer_keepalive: HashMap<RawFd, FdWatch>,
    pub task_factory: Option<Py<PyAny>>,
    pub exception_handler: Option<Py<PyAny>>,
    pub default_executor: Option<Py<PyAny>>,
}

impl LoopState {
    fn new() -> Self {
        Self {
            closed: false,
            running: false,
            stopping: false,
            slow_callback_duration: 0.1,
            asyncgens_shutdown_called: false,
            active_asyncgens: None,
            executor_shutdown_called: false,
            signal_handlers: HashMap::new(),
            previous_signal_handlers: HashMap::new(),
            reader_keepalive: HashMap::new(),
            writer_keepalive: HashMap::new(),
            task_factory: None,
            exception_handler: None,
            default_executor: None,
        }
    }
}

pub struct LoopCore {
    pub state: Mutex<LoopState>,
    pub start: Instant,
    pub debug_enabled: AtomicBool,
    task_factory_installed: AtomicBool,
    next_callback_id: AtomicU64,
    command_tx: Sender<LoopCommand>,
    runtime_thread: Mutex<Option<JoinHandle<()>>>,
    runtime_waker: Mutex<Option<Waker>>,
    active_ready_dispatch: Mutex<Option<ActiveReadyDispatch>>,
}

impl LoopCore {
    pub fn new() -> Arc<Self> {
        let (command_tx, command_rx) = crossbeam_channel::unbounded();
        let core = Arc::new(Self {
            state: Mutex::new(LoopState::new()),
            start: Instant::now(),
            debug_enabled: AtomicBool::new(false),
            task_factory_installed: AtomicBool::new(false),
            next_callback_id: AtomicU64::new(1),
            command_tx,
            runtime_thread: Mutex::new(None),
            runtime_waker: Mutex::new(None),
            active_ready_dispatch: Mutex::new(None),
        });

        let thread_core = Arc::clone(&core);
        let join_handle = thread::Builder::new()
            .name("rsloop".to_owned())
            .spawn(move || run_runtime_thread(thread_core, command_rx))
            .expect("failed to spawn loop runtime thread");

        *core
            .runtime_thread
            .lock()
            .expect("poisoned runtime thread mutex") = Some(join_handle);
        core
    }

    pub fn send_command(&self, command: LoopCommand) -> Result<(), LoopCoreError> {
        profiling::scope!("LoopCore::send_command");
        let command = match self.try_handle_local_command(command) {
            Ok(()) => return Ok(()),
            Err(command) => command,
        };
        self.command_tx
            .send(command)
            .map_err(|_| LoopCoreError::ChannelClosed)?;
        if let Some(waker) = self
            .runtime_waker
            .lock()
            .expect("poisoned runtime waker")
            .as_ref()
        {
            waker.wake_by_ref();
        }
        Ok(())
    }

    pub fn is_running(&self) -> bool {
        self.state.lock().expect("poisoned loop state").running
    }

    pub fn is_closed(&self) -> bool {
        self.state.lock().expect("poisoned loop state").closed
    }

    pub fn set_debug(&self, enabled: bool) {
        self.debug_enabled.store(enabled, Ordering::SeqCst);
    }

    pub fn get_debug(&self) -> bool {
        self.debug_enabled.load(Ordering::SeqCst)
    }

    #[inline]
    pub fn has_task_factory(&self) -> bool {
        self.task_factory_installed.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn set_task_factory_installed(&self, installed: bool) {
        self.task_factory_installed
            .store(installed, Ordering::Relaxed);
    }

    pub fn next_callback_id(&self) -> CallbackId {
        self.next_callback_id.fetch_add(1, Ordering::Relaxed)
    }

    pub fn time(&self) -> f64 {
        self.start.elapsed().as_secs_f64()
    }
}

impl LoopCore {
    pub fn schedule_callback(
        self: &Arc<Self>,
        py: Python<'_>,
        kind: CallbackKind,
        callback: Py<PyAny>,
        args: Py<PyTuple>,
        context: Option<Py<PyAny>>,
    ) -> PyResult<Py<crate::callbacks::PyHandle>> {
        profiling::scope!("LoopCore::schedule_callback");
        let (captured, context_needs_run) = capture_context(py, context)?;
        let ready = ReadyCallback::new(
            py,
            self.next_callback_id(),
            kind,
            callback,
            args,
            captured,
            context_needs_run,
        );
        let handle = Py::new(py, crate::callbacks::PyHandle::new(ready))?;

        // send_command falls through local enqueue, the active-run pending
        // queue, and finally the runtime command channel.
        self.send_command(LoopCommand::ScheduleReadyHandle(handle.clone_ref(py)))
            .map_err(|err| pyo3::exceptions::PyRuntimeError::new_err(err.to_string()))?;
        Ok(handle)
    }

    pub fn schedule_timer(
        self: &Arc<Self>,
        py: Python<'_>,
        delay: Duration,
        callback: Py<PyAny>,
        args: Py<PyTuple>,
        context: Option<Py<PyAny>>,
    ) -> PyResult<(Arc<ReadyCallback>, f64)> {
        profiling::scope!("LoopCore::schedule_timer");
        let (captured, context_needs_run) = capture_context(py, context)?;
        let ready = Arc::new(ReadyCallback::new(
            py,
            self.next_callback_id(),
            CallbackKind::Timer,
            callback,
            args,
            captured,
            context_needs_run,
        ));

        let when = self.time() + delay.as_secs_f64();
        let deadline = Instant::now() + delay;
        self.send_command(LoopCommand::ScheduleTimer {
            callback: Arc::clone(&ready),
            when: deadline,
        })
        .map_err(|err| pyo3::exceptions::PyRuntimeError::new_err(err.to_string()))?;
        Ok((ready, when))
    }

    #[profiling::function]
    pub fn run_forever(&self, py: Python<'_>, loop_obj: Py<PyAny>) -> PyResult<()> {
        const SIGNAL_POLL_INTERVAL: Duration = Duration::from_millis(50);

        {
            let mut state = self.state.lock().expect("poisoned loop state");
            if state.closed {
                return Err(pyo3::exceptions::PyRuntimeError::new_err(
                    LoopCoreError::Closed.to_string(),
                ));
            }
            if state.running {
                return Err(pyo3::exceptions::PyRuntimeError::new_err(
                    LoopCoreError::Running.to_string(),
                ));
            }
            state.running = true;
            state.stopping = false;
        }

        let (wake_tx, wake_rx) = std::sync::mpsc::channel();
        let wake_rx = Arc::new(Mutex::new(wake_rx));
        let pending_ready = Arc::new(Mutex::new(VecDeque::new()));
        let wake_pending = Arc::new(AtomicBool::new(false));
        {
            let mut active_dispatch = self
                .active_ready_dispatch
                .lock()
                .expect("poisoned active ready dispatch");
            *active_dispatch = Some(ActiveReadyDispatch {
                pending_ready: Arc::clone(&pending_ready),
                wake_tx: wake_tx.clone(),
                wake_pending: Arc::clone(&wake_pending),
            });
        }
        self.send_command(LoopCommand::Run(LoopRunCommand::EnterRun {
            pending_ready: Arc::clone(&pending_ready),
            wake_tx,
            wake_pending: Arc::clone(&wake_pending),
        }))
        .map_err(|err| pyo3::exceptions::PyRuntimeError::new_err(err.to_string()))?;

        ensure_running_loop(py, &loop_obj)?;
        self.mark_runtime_thread();
        let mut local_ready = VecDeque::new();
        self.install_local_ready_queue(&mut local_ready);

        let mut pending_signal_error: Option<PyErr> = None;
        let mut ready_batch = VecDeque::new();
        let mut spin_next_wait = false;
        let run_result = loop {
            self.set_ready_drain_active(true);

            let mut ready_error = None;
            let mut processed_since_refill = 0_usize;
            loop {
                if ready_batch.is_empty() || processed_since_refill >= READY_DRAIN_SLICE {
                    // Every cross-thread producer raises `wake_pending` after
                    // pushing, so the pending queue only needs to be locked
                    // when the flag is set; a hot chain of locally scheduled
                    // callbacks otherwise skips the mutex entirely.
                    if wake_pending.load(Ordering::Acquire) {
                        let mut pending =
                            pending_ready.lock().expect("poisoned pending ready queue");
                        if !pending.is_empty() {
                            if ready_batch.is_empty() {
                                std::mem::swap(&mut ready_batch, pending.deref_mut());
                            } else {
                                pending.append(&mut ready_batch);
                                std::mem::swap(&mut ready_batch, pending.deref_mut());
                            }
                        }
                        if pending.is_empty() {
                            wake_pending.store(false, Ordering::Release);
                        }
                    }

                    // Prioritize cross-thread wakeups such as signals and transport
                    // connection_lost notifications so they cannot be starved by a
                    // hot stream of locally-scheduled callbacks.
                    if !local_ready.is_empty() {
                        if ready_batch.is_empty() {
                            std::mem::swap(&mut ready_batch, &mut local_ready);
                        } else {
                            ready_batch.extend(local_ready.drain(..));
                        }
                    }

                    processed_since_refill = 0;

                    if ready_batch.is_empty() {
                        break;
                    }
                }

                let item = ready_batch
                    .pop_front()
                    .expect("ready batch was checked as non-empty");
                spin_next_wait = true;
                match item {
                    ReadyItem::Stop => {
                        profiling::scope!("ready.stop");
                        self.state.lock().expect("poisoned loop state").stopping = true;
                    }
                    ReadyItem::Callback(callback) => {
                        profiling::scope!("ready.callback");
                        if let Some(err) =
                            self.execute_ready(py, Some(&loop_obj), callback.as_ref())?
                        {
                            ready_error = Some(err);
                            break;
                        }
                    }
                    ReadyItem::HandleCallback(handle) => {
                        profiling::scope!("ready.handle_callback");
                        if let Some(err) =
                            self.execute_ready(py, Some(&loop_obj), handle.get().ready())?
                        {
                            ready_error = Some(err);
                            break;
                        }
                    }
                    ReadyItem::FutureSetResult { future, value } => {
                        profiling::scope!("ready.future_set_result");
                        let future = future.bind(py);
                        if !crate::python_names::call_method0(
                            py,
                            future,
                            crate::python_names::done(py),
                        )?
                        .bind(py)
                        .extract::<bool>()?
                        {
                            crate::python_names::call_method1(
                                py,
                                future,
                                crate::python_names::set_result(py),
                                value.bind(py),
                            )?;
                        }
                    }
                    ReadyItem::FutureSetException { future, value } => {
                        profiling::scope!("ready.future_set_exception");
                        let future = future.bind(py);
                        if !crate::python_names::call_method0(
                            py,
                            future,
                            crate::python_names::done(py),
                        )?
                        .bind(py)
                        .extract::<bool>()?
                        {
                            crate::python_names::call_method1(
                                py,
                                future,
                                crate::python_names::set_exception(py),
                                value.bind(py),
                            )?;
                        }
                    }
                    ReadyItem::StreamTransportRead(core) => {
                        profiling::scope!("ready.stream_transport_read");
                        core.drain_pending_read_events_with_py(py)?;
                    }
                    ReadyItem::StreamTransportWrite(core) => {
                        profiling::scope!("ready.stream_transport_write");
                        core.flush_pending_direct_write();
                    }
                    ReadyItem::ProcessTransport(core) => {
                        profiling::scope!("ready.process_transport");
                        core.drain_pending_events_with_py(py)?;
                    }
                    ReadyItem::ServerAccepted { server, stream } => {
                        profiling::scope!("ready.server_accepted");
                        if let Err(err) = crate::stream_transport::spawn_accepted_transport_with_py(
                            py, &server, stream,
                        ) {
                            server.report_error(err, "failed to accept connection");
                        }
                    }
                    #[cfg(unix)]
                    ReadyItem::ConnectCompleted {
                        future,
                        fd,
                        wait_errno,
                    } => {
                        profiling::scope!("ready.connect_completed");
                        self.resolve_connect_completed(py, future, fd, wait_errno)?;
                    }
                }

                processed_since_refill += 1;
            }

            self.set_ready_drain_active(false);

            if let Some(err) = ready_error {
                break Err(err);
            }

            if self.state.lock().expect("poisoned loop state").stopping {
                break match pending_signal_error {
                    Some(err) => Err(err),
                    None => Ok(()),
                };
            }

            if pending_signal_error.is_none()
                && let Err(err) = py.check_signals()
            {
                let _ = self.send_command(LoopCommand::RequestStop);
                pending_signal_error = Some(err);
                continue;
            }

            let wake_rx = Arc::clone(&wake_rx);
            let spin_window = if std::mem::take(&mut spin_next_wait) {
                wake_spin_window()
            } else {
                Duration::ZERO
            };
            let spin_wake_pending = Arc::clone(&wake_pending);
            match py.detach(move || {
                // Busy-wait briefly for the next wakeup while work is flowing;
                // this skips the condvar round trip that dominates request/
                // response latency. If nothing shows up, park as before.
                if !spin_window.is_zero() {
                    let spin_deadline = Instant::now() + spin_window;
                    'spin: loop {
                        for _ in 0..64 {
                            if spin_wake_pending.load(Ordering::Acquire) {
                                // Consume at most one queued wake token so
                                // tokens for wakeups observed via the spin
                                // path cannot accumulate. Missing a token here
                                // is fine: the drain loop re-reads the pending
                                // queue directly.
                                let _ = wake_rx.lock().expect("poisoned wake receiver").try_recv();
                                return Ok(());
                            }
                            std::hint::spin_loop();
                        }
                        if Instant::now() >= spin_deadline {
                            break 'spin;
                        }
                    }
                }

                wake_rx
                    .lock()
                    .expect("poisoned wake receiver")
                    .recv_timeout(SIGNAL_POLL_INTERVAL)
            }) {
                Ok(()) => {}
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    break Err(pyo3::exceptions::PyRuntimeError::new_err(
                        "event loop runtime terminated unexpectedly",
                    ));
                }
            }
        };

        self.set_ready_drain_active(false);
        self.clear_runtime_thread();
        clear_running_loop(py)?;

        // Preserve callbacks that were queued but not yet executed when the run
        // ended. This matters when a propagating BaseException (e.g. SystemExit
        // or KeyboardInterrupt) breaks out of the drain loop mid-batch:
        // FinishRun moves pending_ready back into the runtime's ready_batch.
        if !ready_batch.is_empty() || !local_ready.is_empty() {
            // We want to *prepend* the scheduled items to preserve order (even
            // if it's not strictly guaranteed). so rebuild and replace the pending Deque
            let mut leftover = std::mem::take(&mut ready_batch);
            leftover.extend(local_ready.drain(..));
            let mut pending = pending_ready.lock().expect("poisoned pending ready queue");
            leftover.append(pending.deref_mut());
            *pending = leftover;
        }

        self.active_ready_dispatch
            .lock()
            .expect("poisoned active ready dispatch")
            .take();

        let (done_tx, done_rx) = std::sync::mpsc::channel();
        self.send_command(LoopCommand::Run(LoopRunCommand::FinishRun { done_tx }))
            .map_err(|err| pyo3::exceptions::PyRuntimeError::new_err(err.to_string()))?;
        match py.detach(move || done_rx.recv_timeout(SIGNAL_POLL_INTERVAL)) {
            Ok(()) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {
                return Err(pyo3::exceptions::PyRuntimeError::new_err(
                    "timed out while finishing event loop run",
                ));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(pyo3::exceptions::PyRuntimeError::new_err(
                    "event loop runtime terminated unexpectedly",
                ));
            }
        }

        run_result
    }
}

impl LoopCore {
    pub fn schedule_stop(&self) -> Result<(), LoopCoreError> {
        profiling::scope!("LoopCore::schedule_stop");
        self.send_command(LoopCommand::RequestStop)
    }

    pub fn close(&self) -> Result<(), LoopCoreError> {
        profiling::scope!("LoopCore::close");
        {
            let mut state = self.state.lock().expect("poisoned loop state");
            if state.running {
                return Err(LoopCoreError::Running);
            }
            if state.closed {
                return Ok(());
            }
            state.closed = true;
        }

        self.send_command(LoopCommand::Close)?;
        if let Some(handle) = self
            .runtime_thread
            .lock()
            .expect("poisoned runtime thread mutex")
            .take()
        {
            handle.join().map_err(|_| LoopCoreError::ThreadJoin)?;
        }

        Ok(())
    }
}

impl LoopCore {
    pub fn call_exception_handler(
        &self,
        py: Python<'_>,
        loop_obj: Option<&Py<PyAny>>,
        context: Py<PyAny>,
    ) -> PyResult<()> {
        let handler = {
            self.state
                .lock()
                .expect("poisoned loop state")
                .exception_handler
                .as_ref()
                .map(|handler| handler.clone_ref(py))
        };

        if let Some(handler) = handler {
            let loop_arg = loop_obj
                .map(|loop_obj| loop_obj.clone_ref(py))
                .unwrap_or_else(|| py.None());
            handler.call1(py, (loop_arg, context))?;
            return Ok(());
        }

        self.default_exception_handler(py, context)
    }

    pub fn default_exception_handler(&self, py: Python<'_>, context: Py<PyAny>) -> PyResult<()> {
        let sys = py.import("sys")?;
        let stderr = sys.getattr("stderr")?;
        let context_dict = context.bind(py).cast::<PyDict>()?;
        let message = match context_dict.get_item("message")? {
            Some(item) => item
                .extract::<String>()
                .unwrap_or_else(|_| "Unhandled exception in rsloop".to_owned()),
            None => "Unhandled exception in rsloop".to_owned(),
        };

        stderr.call_method1("write", (format!("{message}\n"),))?;

        if let Some(exc) = context_dict.get_item("exception")? {
            let traceback = py.import("traceback")?;
            traceback.getattr("print_exception")?.call1((exc,))?;
        }

        Ok(())
    }

    pub fn execute_ready(
        &self,
        py: Python<'_>,
        loop_obj: Option<&Py<PyAny>>,
        ready: &ReadyCallback,
    ) -> PyResult<Option<PyErr>> {
        profiling::scope!("LoopCore::execute_ready");
        if ready.cancelled() {
            return Ok(None);
        }

        let result = match ready.invoke(py) {
            Ok(_) => Ok(None),
            Err(err) => handle_callback_error(
                py,
                self,
                loop_obj,
                err,
                format!("<{:?} id={}>", ready.kind(), ready.id()),
            ),
        };

        self.rearm_fd_watch_if_needed(ready);
        result
    }

    fn rearm_fd_watch_if_needed(&self, ready: &ReadyCallback) {
        // Only re-arm when this callback is still the current registration
        // for the fd: a stale fire that outlived remove_reader()/add_reader()
        // must not restart a watcher for the superseded callback.
        let command = match ready.kind() {
            CallbackKind::Reader(fd) => self
                .state
                .lock()
                .expect("poisoned loop state")
                .reader_keepalive
                .get(&fd)
                .filter(|watch| std::ptr::eq(Arc::as_ptr(&watch.ready), ready))
                .map(|watch| {
                    LoopCommand::Io(LoopIoCommand::StartReader {
                        fd,
                        callback: Arc::clone(&watch.ready),
                    })
                }),
            CallbackKind::Writer(fd) => self
                .state
                .lock()
                .expect("poisoned loop state")
                .writer_keepalive
                .get(&fd)
                .filter(|watch| std::ptr::eq(Arc::as_ptr(&watch.ready), ready))
                .map(|watch| {
                    LoopCommand::Io(LoopIoCommand::StartWriter {
                        fd,
                        callback: Arc::clone(&watch.ready),
                    })
                }),
            _ => None,
        };

        if let Some(command) = command {
            let _ = self.send_command(command);
        }
    }

    #[inline]
    pub(crate) fn mark_runtime_thread(&self) {
        ACTIVE_LOOP_TLS.with(|tls| tls.core.set(self as *const Self));
    }

    pub(crate) fn set_runtime_waker(&self, waker: Option<Waker>) {
        *self.runtime_waker.lock().expect("poisoned runtime waker") = waker;
    }

    #[inline]
    pub(crate) fn install_local_ready_queue(&self, ready: *mut VecDeque<ReadyItem>) {
        ACTIVE_LOOP_TLS.with(|tls| tls.ready_queue.set(ready));
    }

    #[inline]
    pub(crate) fn clear_runtime_thread(&self) {
        ACTIVE_LOOP_TLS.with(|tls| {
            if std::ptr::eq(tls.core.get(), self) {
                tls.core.set(std::ptr::null());
            }
            tls.ready_queue.set(std::ptr::null_mut());
            tls.drain_active.set(false);
        });
    }

    #[inline]
    pub(crate) fn set_ready_drain_active(&self, active: bool) {
        ACTIVE_LOOP_TLS.with(|tls| tls.drain_active.set(active));
    }

    #[inline]
    pub(crate) fn on_runtime_thread(&self) -> bool {
        ACTIVE_LOOP_TLS.with(|tls| std::ptr::eq(tls.core.get(), self))
    }

    /// Resolves a TCP connect whose writability wait finished on the vibeio
    /// reactor. Runs on the loop thread so the SO_ERROR check and the
    /// set_result / set_exception happen with the GIL already held for the
    /// whole ready batch — no per-completion GIL handoff.
    #[cfg(unix)]
    fn resolve_connect_completed(
        &self,
        py: Python<'_>,
        future: Py<PyAny>,
        fd: RawFd,
        wait_errno: i32,
    ) -> PyResult<()> {
        let future = future.bind(py);
        let done = crate::python_names::call_method0(py, future, crate::python_names::done(py))?
            .bind(py)
            .extract::<bool>()?;
        if done {
            return Ok(());
        }

        // SO_ERROR is authoritative for the connect outcome; the wait error is
        // only a fallback for the rare case where SO_ERROR is already cleared.
        let so_error = crate::fd_ops::socket_so_error(fd)
            .unwrap_or_else(|err| err.raw_os_error().unwrap_or(libc::EBADF));
        let errno = if so_error != 0 { so_error } else { wait_errno };

        if errno == 0 || crate::fd_ops::is_already_connected_errno(errno) {
            crate::python_names::call_method1(
                py,
                future,
                crate::python_names::set_result(py),
                py.None().bind(py),
            )?;
        } else if crate::fd_ops::is_connect_in_progress_errno(errno) {
            // Spurious writability wakeup while still connecting; re-arm.
            let _ = self.send_command(LoopCommand::Io(LoopIoCommand::WatchConnect {
                fd,
                future: future.clone().unbind(),
            }));
        } else {
            let message = std::io::Error::from_raw_os_error(errno).to_string();
            let oserror = pyo3::exceptions::PyOSError::new_err((errno, message)).into_value(py);
            crate::python_names::call_method1(
                py,
                future,
                crate::python_names::set_exception(py),
                oserror.bind(py).as_any(),
            )?;
        }
        Ok(())
    }

    #[inline]
    fn try_handle_local_command(&self, command: LoopCommand) -> Result<(), LoopCommand> {
        match command {
            LoopCommand::ScheduleReady(callback) => self
                .try_enqueue_local_ready(ReadyItem::Callback(callback))
                .or_else(|item| self.try_enqueue_active_ready(item))
                .map_err(|item| match item {
                    ReadyItem::Callback(callback) => LoopCommand::ScheduleReady(callback),
                    ReadyItem::HandleCallback(handle) => LoopCommand::ScheduleReadyHandle(handle),
                    ReadyItem::Stop => LoopCommand::RequestStop,
                    ReadyItem::FutureSetResult { future, value } => {
                        LoopCommand::Future(LoopFutureCommand::SetResult { future, value })
                    }
                    ReadyItem::FutureSetException { future, value } => {
                        LoopCommand::Future(LoopFutureCommand::SetException { future, value })
                    }
                    ReadyItem::StreamTransportRead(core) => {
                        LoopCommand::Transport(LoopTransportCommand::StreamRead(core))
                    }
                    ReadyItem::StreamTransportWrite(core) => {
                        LoopCommand::Transport(LoopTransportCommand::StreamWrite(core))
                    }
                    ReadyItem::ProcessTransport(core) => {
                        LoopCommand::Transport(LoopTransportCommand::Process(core))
                    }
                    ReadyItem::ServerAccepted { server, stream } => {
                        LoopCommand::Transport(LoopTransportCommand::ServerAccepted {
                            server,
                            stream,
                        })
                    }
                    #[cfg(unix)]
                    ReadyItem::ConnectCompleted {
                        future,
                        fd,
                        wait_errno,
                    } => LoopCommand::ConnectCompleted {
                        future,
                        fd,
                        wait_errno,
                    },
                }),
            LoopCommand::ScheduleReadyHandle(handle) => self
                .try_enqueue_local_ready(ReadyItem::HandleCallback(handle))
                .or_else(|item| self.try_enqueue_active_ready(item))
                .map_err(|item| match item {
                    ReadyItem::HandleCallback(handle) => LoopCommand::ScheduleReadyHandle(handle),
                    _ => unreachable!("local handle enqueue preserves item kind"),
                }),
            LoopCommand::Future(LoopFutureCommand::SetResult { future, value }) => self
                .try_enqueue_local_ready(ReadyItem::FutureSetResult { future, value })
                .or_else(|item| self.try_enqueue_active_ready(item))
                .map_err(|item| match item {
                    ReadyItem::FutureSetResult { future, value } => {
                        LoopCommand::Future(LoopFutureCommand::SetResult { future, value })
                    }
                    _ => {
                        unreachable!("local future result enqueue preserves item kind")
                    }
                }),
            LoopCommand::Future(LoopFutureCommand::SetException { future, value }) => self
                .try_enqueue_local_ready(ReadyItem::FutureSetException { future, value })
                .or_else(|item| self.try_enqueue_active_ready(item))
                .map_err(|item| match item {
                    ReadyItem::FutureSetException { future, value } => {
                        LoopCommand::Future(LoopFutureCommand::SetException { future, value })
                    }
                    _ => {
                        unreachable!("local future exception enqueue preserves item kind")
                    }
                }),
            LoopCommand::Transport(LoopTransportCommand::StreamRead(core)) => self
                .try_enqueue_local_ready(ReadyItem::StreamTransportRead(core))
                .or_else(|item| self.try_enqueue_active_ready(item))
                .map_err(|item| match item {
                    ReadyItem::StreamTransportRead(core) => {
                        LoopCommand::Transport(LoopTransportCommand::StreamRead(core))
                    }
                    _ => {
                        unreachable!("local stream read enqueue preserves item kind")
                    }
                }),
            LoopCommand::Transport(LoopTransportCommand::StreamWrite(core)) => self
                .try_enqueue_local_ready(ReadyItem::StreamTransportWrite(core))
                .or_else(|item| self.try_enqueue_active_ready(item))
                .map_err(|item| match item {
                    ReadyItem::StreamTransportWrite(core) => {
                        LoopCommand::Transport(LoopTransportCommand::StreamWrite(core))
                    }
                    _ => {
                        unreachable!("local stream write enqueue preserves item kind")
                    }
                }),
            LoopCommand::Transport(LoopTransportCommand::Process(core)) => self
                .try_enqueue_local_ready(ReadyItem::ProcessTransport(core))
                .or_else(|item| self.try_enqueue_active_ready(item))
                .map_err(|item| match item {
                    ReadyItem::ProcessTransport(core) => {
                        LoopCommand::Transport(LoopTransportCommand::Process(core))
                    }
                    _ => {
                        unreachable!("local process enqueue preserves item kind")
                    }
                }),
            LoopCommand::Transport(LoopTransportCommand::ServerAccepted { server, stream }) => self
                .try_enqueue_local_ready(ReadyItem::ServerAccepted { server, stream })
                .or_else(|item| self.try_enqueue_active_ready(item))
                .map_err(|item| match item {
                    ReadyItem::ServerAccepted { server, stream } => {
                        LoopCommand::Transport(LoopTransportCommand::ServerAccepted {
                            server,
                            stream,
                        })
                    }
                    _ => {
                        unreachable!("local accepted transport enqueue preserves item kind")
                    }
                }),
            #[cfg(unix)]
            LoopCommand::ConnectCompleted {
                future,
                fd,
                wait_errno,
            } => self
                .try_enqueue_local_ready(ReadyItem::ConnectCompleted {
                    future,
                    fd,
                    wait_errno,
                })
                .or_else(|item| self.try_enqueue_active_ready(item))
                .map_err(|item| match item {
                    ReadyItem::ConnectCompleted {
                        future,
                        fd,
                        wait_errno,
                    } => LoopCommand::ConnectCompleted {
                        future,
                        fd,
                        wait_errno,
                    },
                    _ => unreachable!("local connect completion enqueue preserves item kind"),
                }),
            LoopCommand::RequestStop => self
                .try_enqueue_local_ready(ReadyItem::Stop)
                .or_else(|item| self.try_enqueue_active_ready(item))
                .map_err(|_| LoopCommand::RequestStop),
            other => Err(other),
        }
    }

    #[inline]
    fn try_enqueue_local_ready(&self, item: ReadyItem) -> Result<(), ReadyItem> {
        ACTIVE_LOOP_TLS.with(|tls| {
            if !std::ptr::eq(tls.core.get(), self) || !tls.drain_active.get() {
                return Err(item);
            }

            let ready = tls.ready_queue.get();
            if ready.is_null() {
                return Err(item);
            }

            // SAFETY: `ready` points to the stack-local queue owned by `run_forever` on this thread.
            unsafe { (*ready).push_back(item) };
            Ok(())
        })
    }

    #[inline]
    fn try_enqueue_active_ready(&self, item: ReadyItem) -> Result<(), ReadyItem> {
        let active_dispatch = self
            .active_ready_dispatch
            .lock()
            .expect("poisoned active ready dispatch");
        let Some(dispatch) = active_dispatch.as_ref() else {
            return Err(item);
        };

        dispatch
            .pending_ready
            .lock()
            .expect("poisoned pending ready queue")
            .push_back(item);
        if !dispatch.wake_pending.swap(true, Ordering::AcqRel) {
            let _ = dispatch.wake_tx.send(());
        }
        Ok(())
    }
}
