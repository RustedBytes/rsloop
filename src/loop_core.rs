use std::cell::Cell;
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
#[cfg(target_os = "linux")]
use std::task::Waker;
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crate::callbacks::{CallbackId, CallbackKind, ReadyCallback};
use crate::context::{capture_context, clear_running_loop, ensure_running_loop};
use crate::errors::handle_callback_error;
use crate::fd_ops::RawFd;
use crate::process_transport::ProcessTransportCore;
use crate::runtime::run_runtime_thread;
use crate::stream_transport::{ReaderTarget, ServerCore, ServerListener, StreamTransportCore};
use crossbeam_channel::Sender;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyTuple};

const READY_DRAIN_SLICE: usize = 64;

thread_local! {
    static ACTIVE_LOOP_CORE: Cell<*const LoopCore> = const { Cell::new(std::ptr::null()) };
    static ACTIVE_READY_QUEUE: Cell<*mut VecDeque<ReadyItem>> = const { Cell::new(std::ptr::null_mut()) };
    static ACTIVE_READY_DRAIN: Cell<bool> = const { Cell::new(false) };
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

pub enum ReadyItem {
    Callback(Arc<ReadyCallback>),
    FutureSetResult { future: Py<PyAny>, value: Py<PyAny> },
    FutureSetException { future: Py<PyAny>, value: Py<PyAny> },
    StreamTransportRead(Arc<StreamTransportCore>),
    ProcessTransport(Arc<ProcessTransportCore>),
    Stop,
}

pub enum LoopCommand {
    ScheduleReady(Arc<ReadyCallback>),
    ScheduleTimer {
        callback: Arc<ReadyCallback>,
        when: Instant,
    },
    EnterRun {
        pending_ready: Arc<Mutex<VecDeque<ReadyItem>>>,
        wake_tx: std::sync::mpsc::Sender<()>,
    },
    FinishRun {
        done_tx: std::sync::mpsc::Sender<()>,
    },
    StartSignalWatcher(i32),
    StopSignalWatcher(i32),
    SignalFired(i32),
    StartReader {
        fd: RawFd,
        callback: Py<PyAny>,
        args: Py<PyTuple>,
        context: Py<PyAny>,
        context_needs_run: bool,
    },
    StopReader(RawFd),
    StartWriter {
        fd: RawFd,
        callback: Py<PyAny>,
        args: Py<PyTuple>,
        context: Py<PyAny>,
        context_needs_run: bool,
    },
    StopWriter(RawFd),
    ScheduleFutureSetResult {
        future: Py<PyAny>,
        value: Py<PyAny>,
    },
    ScheduleFutureSetException {
        future: Py<PyAny>,
        value: Py<PyAny>,
    },
    ScheduleStreamTransportRead(Arc<StreamTransportCore>),
    ScheduleProcessTransport(Arc<ProcessTransportCore>),
    StartSocketReader {
        fd: RawFd,
        core: Arc<StreamTransportCore>,
        reader: ReaderTarget,
    },
    StopSocketReader(RawFd),
    StartServerAccept {
        fd: RawFd,
        server: Arc<ServerCore>,
        listener: ServerListener,
    },
    StopServerAccept(RawFd),
    RequestStop,
    Close,
}

pub struct SignalHandlerTemplate {
    pub callback: Py<PyAny>,
    pub args: Py<PyTuple>,
    pub context: Py<PyAny>,
    pub context_needs_run: bool,
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
    pub asyncgens_shutdown_called: bool,
    pub executor_shutdown_called: bool,
    pub signal_handlers: HashMap<i32, SignalHandlerTemplate>,
    pub previous_signal_handlers: HashMap<i32, Py<PyAny>>,
    pub reader_keepalive: HashMap<RawFd, Py<PyAny>>,
    pub writer_keepalive: HashMap<RawFd, Py<PyAny>>,
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
            asyncgens_shutdown_called: false,
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
    #[cfg(target_os = "linux")]
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
            #[cfg(target_os = "linux")]
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
        #[cfg(target_os = "linux")]
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

    pub fn schedule_callback(
        self: &Arc<Self>,
        py: Python<'_>,
        kind: CallbackKind,
        callback: Py<PyAny>,
        args: Py<PyTuple>,
        context: Option<Py<PyAny>>,
    ) -> PyResult<Arc<ReadyCallback>> {
        profiling::scope!("LoopCore::schedule_callback");
        let (captured, context_needs_run) = capture_context(py, context)?;
        let ready = Arc::new(ReadyCallback::new(
            py,
            self.next_callback_id(),
            kind,
            callback,
            args,
            captured,
            context_needs_run,
        ));

        match self.try_enqueue_local_ready(ReadyItem::Callback(Arc::clone(&ready))) {
            Ok(()) => return Ok(ready),
            Err(ReadyItem::Callback(callback)) => {
                if self
                    .try_enqueue_active_ready(ReadyItem::Callback(callback))
                    .is_ok()
                {
                    return Ok(ready);
                }
            }
            Err(
                ReadyItem::Stop
                | ReadyItem::FutureSetResult { .. }
                | ReadyItem::FutureSetException { .. }
                | ReadyItem::StreamTransportRead(_)
                | ReadyItem::ProcessTransport(_),
            ) => unreachable!("schedule_callback only enqueues callback ready items"),
        }

        self.command_tx
            .send(LoopCommand::ScheduleReady(Arc::clone(&ready)))
            .map_err(|_| {
                pyo3::exceptions::PyRuntimeError::new_err(LoopCoreError::ChannelClosed.to_string())
            })?;
        #[cfg(target_os = "linux")]
        if let Some(waker) = self
            .runtime_waker
            .lock()
            .expect("poisoned runtime waker")
            .as_ref()
        {
            waker.wake_by_ref();
        }
        Ok(ready)
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
        self.send_command(LoopCommand::EnterRun {
            pending_ready: Arc::clone(&pending_ready),
            wake_tx,
        })
        .map_err(|err| pyo3::exceptions::PyRuntimeError::new_err(err.to_string()))?;

        ensure_running_loop(py, &loop_obj)?;
        self.mark_runtime_thread();
        let mut local_ready = VecDeque::new();
        self.install_local_ready_queue(&mut local_ready);

        let mut pending_signal_error: Option<PyErr> = None;
        let mut ready_batch = VecDeque::new();
        let run_result = loop {
            self.set_ready_drain_active(true);

            let mut ready_error = None;
            let mut processed_since_refill = 0_usize;
            loop {
                if ready_batch.is_empty() || processed_since_refill >= READY_DRAIN_SLICE {
                    let should_check_pending =
                        ready_batch.is_empty() || wake_pending.load(Ordering::Acquire);
                    if should_check_pending {
                        let mut pending =
                            pending_ready.lock().expect("poisoned pending ready queue");
                        if !pending.is_empty() {
                            if ready_batch.is_empty() {
                                std::mem::swap(&mut ready_batch, &mut *pending);
                            } else {
                                pending.append(&mut ready_batch);
                                std::mem::swap(&mut ready_batch, &mut *pending);
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
                match item {
                    ReadyItem::Stop => {
                        profiling::scope!("ready.stop");
                        self.state.lock().expect("poisoned loop state").stopping = true;
                    }
                    ReadyItem::Callback(callback) => {
                        profiling::scope!("ready.callback");
                        if let Some(err) = self.execute_ready(py, Some(&loop_obj), &callback)? {
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
                    ReadyItem::ProcessTransport(core) => {
                        profiling::scope!("ready.process_transport");
                        core.drain_pending_events_with_py(py)?;
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

            if pending_signal_error.is_none() {
                if let Err(err) = py.check_signals() {
                    let _ = self.send_command(LoopCommand::RequestStop);
                    pending_signal_error = Some(err);
                    continue;
                }
            }

            let wake_rx = Arc::clone(&wake_rx);
            match py.detach(move || {
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
        self.active_ready_dispatch
            .lock()
            .expect("poisoned active ready dispatch")
            .take();

        let (done_tx, done_rx) = std::sync::mpsc::channel();
        self.send_command(LoopCommand::FinishRun { done_tx })
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
        ready: &Arc<ReadyCallback>,
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

    fn rearm_fd_watch_if_needed(&self, ready: &Arc<ReadyCallback>) {
        let command = match ready.kind() {
            CallbackKind::Reader(fd) => {
                let should_rearm = self
                    .state
                    .lock()
                    .expect("poisoned loop state")
                    .reader_keepalive
                    .contains_key(&fd);
                should_rearm.then(|| {
                    Python::attach(|py| LoopCommand::StartReader {
                        fd,
                        callback: ready.callback().clone_ref(py),
                        args: ready.clone_args_tuple(py),
                        context: ready.context().clone_ref(py),
                        context_needs_run: ready.context_needs_run(),
                    })
                })
            }
            CallbackKind::Writer(fd) => {
                let should_rearm = self
                    .state
                    .lock()
                    .expect("poisoned loop state")
                    .writer_keepalive
                    .contains_key(&fd);
                should_rearm.then(|| {
                    Python::attach(|py| LoopCommand::StartWriter {
                        fd,
                        callback: ready.callback().clone_ref(py),
                        args: ready.clone_args_tuple(py),
                        context: ready.context().clone_ref(py),
                        context_needs_run: ready.context_needs_run(),
                    })
                })
            }
            _ => None,
        };

        if let Some(command) = command {
            let _ = self.send_command(command);
        }
    }

    #[inline]
    pub(crate) fn mark_runtime_thread(&self) {
        ACTIVE_LOOP_CORE.with(|current| current.set(self as *const Self));
    }

    #[cfg(target_os = "linux")]
    pub(crate) fn set_runtime_waker(&self, waker: Option<Waker>) {
        *self.runtime_waker.lock().expect("poisoned runtime waker") = waker;
    }

    #[inline]
    pub(crate) fn install_local_ready_queue(&self, ready: *mut VecDeque<ReadyItem>) {
        ACTIVE_READY_QUEUE.with(|current| current.set(ready));
    }

    #[inline]
    pub(crate) fn clear_runtime_thread(&self) {
        ACTIVE_LOOP_CORE.with(|current| {
            if std::ptr::eq(current.get(), self) {
                current.set(std::ptr::null());
            }
        });
        ACTIVE_READY_QUEUE.with(|current| current.set(std::ptr::null_mut()));
        ACTIVE_READY_DRAIN.with(|current| current.set(false));
    }

    #[inline]
    pub(crate) fn set_ready_drain_active(&self, active: bool) {
        ACTIVE_READY_DRAIN.with(|current| current.set(active));
    }

    #[inline]
    pub(crate) fn on_runtime_thread(&self) -> bool {
        ACTIVE_LOOP_CORE.with(|current| std::ptr::eq(current.get(), self))
    }

    #[inline]
    fn try_handle_local_command(&self, command: LoopCommand) -> Result<(), LoopCommand> {
        match command {
            LoopCommand::ScheduleReady(callback) => self
                .try_enqueue_local_ready(ReadyItem::Callback(callback))
                .or_else(|item| self.try_enqueue_active_ready(item))
                .map_err(|item| match item {
                    ReadyItem::Callback(callback) => LoopCommand::ScheduleReady(callback),
                    ReadyItem::Stop => LoopCommand::RequestStop,
                    ReadyItem::FutureSetResult { future, value } => {
                        LoopCommand::ScheduleFutureSetResult { future, value }
                    }
                    ReadyItem::FutureSetException { future, value } => {
                        LoopCommand::ScheduleFutureSetException { future, value }
                    }
                    ReadyItem::StreamTransportRead(core) => {
                        LoopCommand::ScheduleStreamTransportRead(core)
                    }
                    ReadyItem::ProcessTransport(core) => {
                        LoopCommand::ScheduleProcessTransport(core)
                    }
                }),
            LoopCommand::ScheduleFutureSetResult { future, value } => self
                .try_enqueue_local_ready(ReadyItem::FutureSetResult { future, value })
                .or_else(|item| self.try_enqueue_active_ready(item))
                .map_err(|item| match item {
                    ReadyItem::FutureSetResult { future, value } => {
                        LoopCommand::ScheduleFutureSetResult { future, value }
                    }
                    ReadyItem::Callback(_)
                    | ReadyItem::FutureSetException { .. }
                    | ReadyItem::StreamTransportRead(_)
                    | ReadyItem::ProcessTransport(_)
                    | ReadyItem::Stop => {
                        unreachable!("local future result enqueue preserves item kind")
                    }
                }),
            LoopCommand::ScheduleFutureSetException { future, value } => self
                .try_enqueue_local_ready(ReadyItem::FutureSetException { future, value })
                .or_else(|item| self.try_enqueue_active_ready(item))
                .map_err(|item| match item {
                    ReadyItem::FutureSetException { future, value } => {
                        LoopCommand::ScheduleFutureSetException { future, value }
                    }
                    ReadyItem::Callback(_)
                    | ReadyItem::FutureSetResult { .. }
                    | ReadyItem::StreamTransportRead(_)
                    | ReadyItem::ProcessTransport(_)
                    | ReadyItem::Stop => {
                        unreachable!("local future exception enqueue preserves item kind")
                    }
                }),
            LoopCommand::ScheduleStreamTransportRead(core) => self
                .try_enqueue_local_ready(ReadyItem::StreamTransportRead(core))
                .or_else(|item| self.try_enqueue_active_ready(item))
                .map_err(|item| match item {
                    ReadyItem::StreamTransportRead(core) => {
                        LoopCommand::ScheduleStreamTransportRead(core)
                    }
                    ReadyItem::Callback(_)
                    | ReadyItem::FutureSetResult { .. }
                    | ReadyItem::FutureSetException { .. }
                    | ReadyItem::ProcessTransport(_)
                    | ReadyItem::Stop => {
                        unreachable!("local stream read enqueue preserves item kind")
                    }
                }),
            LoopCommand::ScheduleProcessTransport(core) => self
                .try_enqueue_local_ready(ReadyItem::ProcessTransport(core))
                .or_else(|item| self.try_enqueue_active_ready(item))
                .map_err(|item| match item {
                    ReadyItem::ProcessTransport(core) => {
                        LoopCommand::ScheduleProcessTransport(core)
                    }
                    ReadyItem::Callback(_)
                    | ReadyItem::FutureSetResult { .. }
                    | ReadyItem::FutureSetException { .. }
                    | ReadyItem::StreamTransportRead(_)
                    | ReadyItem::Stop => {
                        unreachable!("local process enqueue preserves item kind")
                    }
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
        let same_loop = ACTIVE_LOOP_CORE.with(|current| std::ptr::eq(current.get(), self));
        if !same_loop {
            return Err(item);
        }

        ACTIVE_READY_QUEUE.with(|current| {
            let ready = current.get();
            if ready.is_null() {
                return Err(item);
            }

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
