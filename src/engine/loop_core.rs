//! Python loop-thread state and the bridge to runtime and worker threads.
//!
//! `LoopCore` owns lifecycle state and ready queues. Python callbacks execute
//! on the caller's loop thread; other threads only enqueue commands or results.

use std::cell::{Cell, RefCell};
use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::fmt;
use std::future::Future;
use std::ops::DerefMut;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use super::callbacks::{CallbackArgs, CallbackId, CallbackKind, ReadyCallback};
use super::commands::{
    LoopCommand, LoopFutureCommand, LoopIoCommand, LoopRunCommand, LoopTransportCommand, ReadyItem,
};
use super::dispatcher::run_runtime_thread;
use super::timer_entry::TimerEntry;
use crate::context::{capture_context, clear_running_loop, ensure_running_loop};
use crate::errors::handle_callback_error;
use crate::fd_ops::RawFd;
use crossbeam_channel::Sender;
use futures::task::AtomicWaker;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PySet, PyTuple};

thread_local! {
    /// Per-loop vibeio runtime hosted on the loop thread. Keyed by `LoopCore`
    /// pointer so a thread that runs several loops sequentially keeps each
    /// loop's spawned I/O tasks alive across `run_until_complete` calls. The
    /// runtime is `!Send`, which is why it lives in thread-local storage rather
    /// than on `LoopCore`; asyncio's contract that a loop only runs on one
    /// thread makes that safe.
    /// `ManuallyDrop` so a runtime is only ever dropped explicitly in
    /// `LoopCore::close` (on the loop thread, while vibeio's own thread-locals
    /// are still alive). If a loop is never closed, its runtime is leaked at
    /// thread exit rather than dropped — dropping during TLS destruction trips
    /// an AccessError panic inside vibeio's `Runtime::drop`.
    static LOOP_RUNTIMES: RefCell<HashMap<usize, std::mem::ManuallyDrop<crate::vibeio::Runtime>>> =
        RefCell::new(HashMap::new());

    /// Handles for cancellable I/O tasks (accept loops, socket readers) spawned
    /// on the loop runtime, keyed by loop pointer then by fd. A separate
    /// thread-local from `LOOP_RUNTIMES` so registering a task (`borrow_mut`
    /// here) never conflicts with the park holding an immutable `LOOP_RUNTIMES`
    /// borrow across `block_on`. `JoinHandle` is `!Send`, so this must live in
    /// TLS on the loop thread.
    static IO_TASKS: RefCell<HashMap<usize, HashMap<RawFd, crate::vibeio::JoinHandle<()>>>> =
        RefCell::new(HashMap::new());
}

/// Cross-thread wake state for the loop thread, kept separate from `LoopCore`
/// so it stays `Ungil` (contains no Python objects) and can therefore be held
/// by a future driven under `py.detach`. `ready_pending` is the "ready queue
/// non-empty" flag the park future polls; `ready_waker` holds the loop thread's
/// task waker while it is parked. Replaces the old mpsc wake channel.
pub struct LoopWake {
    ready_pending: AtomicBool,
    ready_waker: AtomicWaker,
}

impl LoopWake {
    fn new() -> Self {
        Self {
            ready_pending: AtomicBool::new(false),
            ready_waker: AtomicWaker::new(),
        }
    }

    /// Marks the ready queue non-empty and wakes the parked loop thread. Cheap
    /// and idempotent while a wake is already pending.
    #[inline]
    pub fn signal(&self) {
        if !self.ready_pending.swap(true, Ordering::AcqRel) {
            self.ready_waker.wake();
        }
    }
}

/// The park primitive for the loop thread. `run_forever` drives this to
/// completion via `runtime.block_on` with the GIL released, so vibeio's reactor
/// (`driver.wait`) runs on the loop thread instead of a separate one. It
/// resolves when a ready item is enqueued (`LoopWake::signal`) or the
/// signal-poll timeout elapses, at which point `run_forever` re-acquires the
/// GIL and drains.
struct WaitForWake {
    wake: Arc<LoopWake>,
    sleep: Pin<Box<crate::vibeio::time::Sleep>>,
}

impl WaitForWake {
    #[cfg(test)]
    fn new(wake: Arc<LoopWake>, timeout: Duration) -> Self {
        Self::until(wake, Instant::now() + timeout)
    }

    fn until(wake: Arc<LoopWake>, deadline: Instant) -> Self {
        Self {
            wake,
            sleep: Box::pin(crate::vibeio::time::Sleep::sleep_until(deadline)),
        }
    }
}

/// Bounded busy-wait before parking in `block_on`. Cross-thread wakeups (reader
/// worker threads, the transitional runtime thread) otherwise pay the full
/// `driver.wait` park + interrupt round-trip, which dominates request/response
/// ping-pong latency and inflates its variance. Env-tunable via
/// `RSLOOP_WAKE_SPIN_US` (0 disables).
fn wake_spin_window() -> Duration {
    static WINDOW: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *WINDOW.get_or_init(|| {
        let micros = std::env::var("RSLOOP_WAKE_SPIN_US")
            .ok()
            .and_then(|value| value.trim().parse::<u64>().ok())
            .unwrap_or(50);
        Duration::from_micros(micros.min(1_000))
    })
}

// After this many consecutive spin-caught wakeups, park in `block_on` anyway so
// this loop's own runtime tasks (accept loops, connect watches) and io_uring
// completions are still serviced under sustained same-connection traffic.
const MAX_CONSECUTIVE_SPINS: u32 = 64;

// Parks to skip spinning for after a spin window elapsed without catching a
// wake. Long enough that a loop whose wakes never land inside the window stops
// paying for it, short enough that the loop re-probes often enough to recover
// the low-latency path as soon as traffic turns interactive again.
const SPIN_MISS_COOLDOWN_PARKS: u32 = 8;

impl Future for WaitForWake {
    type Output = ();

    #[inline]
    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.wake.ready_pending.load(Ordering::Acquire) {
            return Poll::Ready(());
        }
        // Register before the second check so a wake that races with
        // registration is never lost.
        self.wake.ready_waker.register(cx.waker());
        if self.wake.ready_pending.load(Ordering::Acquire) {
            return Poll::Ready(());
        }
        if self.sleep.as_mut().poll(cx).is_ready() {
            return Poll::Ready(());
        }
        Poll::Pending
    }
}

const READY_DRAIN_SLICE: usize = 64;
const MAX_READY_ITEMS_PER_TURN: usize = 1024;
const SIGNAL_POLL_INTERVAL: Duration = Duration::from_millis(50);
const RUN_FINISH_TIMEOUT: Duration = Duration::from_secs(5);

// One combined thread-local record instead of three separate `thread_local!`
// cells: each cell access costs a dynamic TLS lookup (`_tlv_get_addr` on
// macOS), and the local-enqueue fast path is hot enough for that to show up
// in profiles.
struct ActiveLoopTls {
    core: Cell<*const LoopCore>,
    ready_queue: Cell<*mut VecDeque<ReadyItem>>,
    drain_active: Cell<bool>,
    timers: Cell<*mut BinaryHeap<TimerEntry>>,
}

thread_local! {
    static ACTIVE_LOOP_TLS: ActiveLoopTls = const {
        ActiveLoopTls {
            core: Cell::new(std::ptr::null()),
            ready_queue: Cell::new(std::ptr::null_mut()),
            drain_active: Cell::new(false),
            timers: Cell::new(std::ptr::null_mut()),
        }
    };
}

/// The active timer heap belongs to this run's thread. A stable allocation
/// permits callback scheduling through TLS without a mutex or dispatcher hop.
/// The guard returns outstanding timers when a run ends, including unwinding.
struct LocalTimers<'a> {
    core: &'a LoopCore,
    #[allow(
        clippy::box_collection,
        reason = "TLS points to the heap object, whose address must survive moving this guard"
    )]
    heap: Box<BinaryHeap<TimerEntry>>,
}

impl<'a> LocalTimers<'a> {
    fn new(core: &'a LoopCore) -> Self {
        let mut heap = Box::new(BinaryHeap::new());
        ACTIVE_LOOP_TLS.with(|tls| tls.timers.set(&mut *heap));
        Self { core, heap }
    }

    fn collect(&mut self, ready: &mut VecDeque<ReadyItem>) {
        if self.core.pending_timers_dirty.swap(false, Ordering::AcqRel) {
            self.heap.append(
                &mut self
                    .core
                    .pending_timers
                    .lock()
                    .expect("poisoned pending timers"),
            );
        }
        let now = Instant::now();
        while self
            .heap
            .peek()
            .is_some_and(|entry| entry.when <= now || entry.callback.cancelled())
        {
            let entry = self.heap.pop().expect("timer heap was nonempty");
            // Even cancelled callbacks are released by the ready drain, never
            // while the heap is mutably borrowed: finalizers can schedule work.
            ready.push_back(ReadyItem::Callback(entry.callback));
        }
    }

    fn deadline(&self) -> Instant {
        let signal_deadline = Instant::now() + SIGNAL_POLL_INTERVAL;
        self.heap
            .peek()
            .map_or(signal_deadline, |entry| entry.when.min(signal_deadline))
    }
}

impl Drop for LocalTimers<'_> {
    fn drop(&mut self) {
        ACTIVE_LOOP_TLS.with(|tls| tls.timers.set(std::ptr::null_mut()));
        if !self.heap.is_empty() {
            self.core
                .pending_timers
                .lock()
                .expect("poisoned pending timers")
                .append(&mut self.heap);
            self.core
                .pending_timers_dirty
                .store(true, Ordering::Release);
        }
    }
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
/// `remove_reader()`/`remove_writer()` find the registration by identity even
/// after the file object has been closed (`fileno()` == -1).
pub struct FdWatch {
    pub fileobj: Py<PyAny>,
    pub ready: Arc<ReadyCallback>,
}

struct ActiveReadyDispatch {
    pending_ready: Arc<Mutex<VecDeque<ReadyItem>>>,
}

/// Mutable lifecycle and Python-facing configuration guarded by `LoopCore`.
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

/// Shared event-loop owner used by Python bindings, the dispatcher, and transports.
pub struct LoopCore {
    /// Mutable lifecycle and Python-facing configuration.
    pub state: Mutex<LoopState>,
    /// Monotonic origin used by [`LoopCore::time`].
    pub start: Instant,
    /// Whether asyncio debug diagnostics are enabled.
    pub debug_enabled: AtomicBool,
    task_factory_installed: AtomicBool,
    next_callback_id: AtomicU64,
    command_tx: Sender<LoopCommand>,
    runtime_thread: Mutex<Option<JoinHandle<()>>>,
    runtime_waker: Mutex<Option<Waker>>,
    active_ready_dispatch: Mutex<Option<ActiveReadyDispatch>>,
    pending_timers: Mutex<BinaryHeap<TimerEntry>>,
    pending_timers_dirty: AtomicBool,
    // Wakes the loop thread when a producer enqueues a ready item. Held in an
    // Arc so the park future (`WaitForWake`) can own a clone under `py.detach`.
    wake: Arc<LoopWake>,
}

impl LoopCore {
    /// Creates a loop core and starts its command-dispatcher thread.
    ///
    /// Python callbacks are not run on that thread; the dispatcher only
    /// coordinates runtime work and forwards ready items to the loop thread.
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
            pending_timers: Mutex::new(BinaryHeap::new()),
            pending_timers_dirty: AtomicBool::new(false),
            wake: Arc::new(LoopWake::new()),
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

    /// Submits a control command to the loop dispatcher.
    ///
    /// Commands originating on the active loop thread may be handled locally to
    /// avoid a channel round trip. `LoopCoreError::ChannelClosed` means the
    /// dispatcher has already terminated.
    pub fn send_command(&self, command: LoopCommand) -> Result<(), LoopCoreError> {
        let command = match self.try_handle_local_command(command) {
            Ok(()) => return Ok(()),
            Err(command) => command,
        };
        self.send_remote_command(command)
    }

    fn send_remote_command(&self, command: LoopCommand) -> Result<(), LoopCoreError> {
        self.command_tx
            .send(command)
            .map_err(|_| LoopCoreError::ChannelClosed)?;
        if let Some(waker) = self
            .runtime_waker
            .lock()
            .expect("poisoned runtime waker")
            .as_ref()
        {
            // Keep the dispatcher waker registered across commands.  A
            // consuming AtomicWaker can lose the next command when a wake is
            // delivered while the dispatcher task is already queued.
            waker.wake_by_ref();
        }
        Ok(())
    }

    #[inline]
    fn schedule_ready_handle(
        &self,
        handle: Py<super::callbacks::PyHandle>,
    ) -> Result<(), LoopCoreError> {
        let item = ReadyItem::HandleCallback(handle);
        let item = match self.try_enqueue_local_ready(item) {
            Ok(()) => return Ok(()),
            Err(item) => item,
        };
        let item = match self.try_enqueue_active_ready(item) {
            Ok(()) => return Ok(()),
            Err(item) => item,
        };
        let ReadyItem::HandleCallback(handle) = item else {
            unreachable!("ready handle enqueue preserves item kind")
        };
        self.send_remote_command(LoopCommand::ScheduleReadyHandle(handle))
    }

    /// Reports whether a run session is currently active.
    pub fn is_running(&self) -> bool {
        self.state.lock().expect("poisoned loop state").running
    }

    /// Reports whether this loop has been permanently closed.
    pub fn is_closed(&self) -> bool {
        self.state.lock().expect("poisoned loop state").closed
    }

    /// Enables or disables asyncio debug diagnostics.
    pub fn set_debug(&self, enabled: bool) {
        self.debug_enabled.store(enabled, Ordering::SeqCst);
    }

    /// Returns whether asyncio debug diagnostics are enabled.
    pub fn get_debug(&self) -> bool {
        self.debug_enabled.load(Ordering::SeqCst)
    }

    #[inline]
    /// Reports whether a custom Python task factory is installed.
    pub fn has_task_factory(&self) -> bool {
        self.task_factory_installed.load(Ordering::Relaxed)
    }

    #[inline]
    /// Updates the fast-path flag for custom task-factory installation.
    ///
    /// The Python object itself remains protected by [`LoopCore::state`].
    pub fn set_task_factory_installed(&self, installed: bool) {
        self.task_factory_installed
            .store(installed, Ordering::Relaxed);
    }

    /// Allocates a loop-unique callback identifier.
    pub fn next_callback_id(&self) -> CallbackId {
        self.next_callback_id.fetch_add(1, Ordering::Relaxed)
    }

    /// Returns elapsed monotonic seconds on this loop's clock.
    pub fn time(&self) -> f64 {
        self.start.elapsed().as_secs_f64()
    }
}

impl LoopCore {
    /// Captures context, creates a Python handle, and schedules a callback.
    ///
    /// The callback is eligible for the next ready-queue drain.
    pub fn schedule_callback(
        self: &Arc<Self>,
        py: Python<'_>,
        kind: CallbackKind,
        callback: Py<PyAny>,
        args: Py<PyTuple>,
        context: Option<Py<PyAny>>,
    ) -> PyResult<Py<super::callbacks::PyHandle>> {
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
        let handle = Py::new(py, super::callbacks::PyHandle::new(ready))?;

        // Keep the common loop-thread path out of the generic command router;
        // fall back to the active-run queue and then the runtime channel.
        self.schedule_ready_handle(handle.clone_ref(py))
            .map_err(|err| pyo3::exceptions::PyRuntimeError::new_err(err.to_string()))?;
        Ok(handle)
    }

    /// FASTCALL entry point: no temporary argument tuple for zero/one args.
    pub(crate) fn schedule_callback_args(
        self: &Arc<Self>,
        py: Python<'_>,
        kind: CallbackKind,
        callback: Py<PyAny>,
        args: CallbackArgs,
        context: Option<Py<PyAny>>,
    ) -> PyResult<Py<super::callbacks::PyHandle>> {
        let (context, needs_run) = capture_context(py, context)?;
        let ready = ReadyCallback::from_args(
            self.next_callback_id(),
            kind,
            callback,
            args,
            context,
            needs_run,
        );
        let handle = Py::new(py, super::callbacks::PyHandle::new(ready))?;
        self.schedule_ready_handle(handle.clone_ref(py))
            .map_err(|err| pyo3::exceptions::PyRuntimeError::new_err(err.to_string()))?;
        Ok(handle)
    }

    /// Captures context and schedules a callback after `delay`.
    ///
    /// Returns the shared callback and its absolute value on [`LoopCore::time`],
    /// which the Python `TimerHandle` exposes as `when()`.
    pub fn schedule_timer(
        self: &Arc<Self>,
        py: Python<'_>,
        delay: Duration,
        callback: Py<PyAny>,
        args: Py<PyTuple>,
        context: Option<Py<PyAny>>,
    ) -> PyResult<(Arc<ReadyCallback>, f64)> {
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
        let entry = TimerEntry {
            callback: Arc::clone(&ready),
            when: deadline,
            seq: ready.id(),
        };
        let remote = ACTIVE_LOOP_TLS.with(|tls| {
            if std::ptr::eq(tls.core.get(), Arc::as_ptr(self)) && !tls.timers.get().is_null() {
                // SAFETY: LocalTimers owns this stable heap on the current
                // thread. No heap borrow crosses callback execution.
                unsafe { (*tls.timers.get()).push(entry) };
                None
            } else {
                Some(entry)
            }
        });
        if let Some(entry) = remote {
            let state = self.state.lock().expect("poisoned loop state");
            if state.closed {
                return Err(pyo3::exceptions::PyRuntimeError::new_err(
                    "event loop is closed",
                ));
            }
            self.pending_timers
                .lock()
                .expect("poisoned pending timers")
                .push(entry);
            self.pending_timers_dirty.store(true, Ordering::Release);
            drop(state);
            self.wake.signal();
        }
        Ok((ready, when))
    }

    /// Runs callbacks and I/O until [`LoopCore::schedule_stop`] is processed.
    ///
    /// The caller is the Python loop thread. While parked, the GIL is released
    /// and the thread drives the loop's `vibeio` runtime; callback drains attach
    /// to Python again before invoking user code.
    ///
    /// Returns an error if the loop is closed or already running.
    pub fn run_forever(self: &Arc<Self>, py: Python<'_>, loop_obj: Py<PyAny>) -> PyResult<()> {
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

        let pending_ready = Arc::new(Mutex::new(VecDeque::new()));
        self.wake.ready_pending.store(false, Ordering::Release);
        {
            let mut active_dispatch = self
                .active_ready_dispatch
                .lock()
                .expect("poisoned active ready dispatch");
            *active_dispatch = Some(ActiveReadyDispatch {
                pending_ready: Arc::clone(&pending_ready),
            });
        }
        self.send_command(LoopCommand::Run(LoopRunCommand::EnterRun {
            pending_ready: Arc::clone(&pending_ready),
        }))
        .map_err(|err| pyo3::exceptions::PyRuntimeError::new_err(err.to_string()))?;

        // The vibeio runtime that drives I/O for this loop lives on this (the
        // loop) thread. Parking below runs its reactor via `block_on`, so I/O
        // readiness and Python callbacks share one thread.
        let loop_runtime_key = Arc::as_ptr(self) as usize;
        LOOP_RUNTIMES.with(|runtimes| {
            runtimes
                .borrow_mut()
                .entry(loop_runtime_key)
                .or_insert_with(|| {
                    let builder = crate::vibeio::RuntimeBuilder::new()
                        .rsloop_profile()
                        .enable_timer(true);
                    std::mem::ManuallyDrop::new(
                        builder
                            .build()
                            .expect("failed to initialize loop-thread vibeio runtime"),
                    )
                });
        });

        ensure_running_loop(py, &loop_obj)?;
        self.mark_runtime_thread();
        let mut local_timers = LocalTimers::new(self);
        let mut local_ready = VecDeque::new();
        self.install_local_ready_queue(&mut local_ready);

        let mut pending_signal_error: Option<PyErr> = None;
        let mut ready_batch = VecDeque::new();
        let spin_window = wake_spin_window();
        let mut consecutive_spins: u32 = 0;
        let mut spin_cooldown: u32 = 0;
        let run_result = loop {
            self.set_ready_drain_active(true);
            local_timers.collect(&mut local_ready);

            let mut ready_error = None;
            let mut deferred_fd_rearms = Vec::new();
            let mut processed_since_refill = 0_usize;
            let mut processed_this_turn = 0_usize;
            loop {
                if ready_batch.is_empty() || processed_since_refill >= READY_DRAIN_SLICE {
                    // Every cross-thread producer raises `wake_pending` after
                    // pushing, so the pending queue only needs to be locked
                    // when the flag is set; a hot chain of locally scheduled
                    // callbacks otherwise skips the mutex entirely.
                    if self.wake.ready_pending.load(Ordering::Acquire) {
                        let mut pending =
                            pending_ready.lock().expect("poisoned pending ready queue");
                        if !pending.is_empty() {
                            if ready_batch.is_empty() {
                                std::mem::swap(&mut ready_batch, pending.deref_mut());
                            } else {
                                // A refill triggered by READY_DRAIN_SLICE leaves
                                // older items in `ready_batch`. Newly queued items
                                // have to go behind them: asyncio orders callbacks
                                // by when they were scheduled, so putting the
                                // fresh arrivals first would run a producer's
                                // later `call_soon_threadsafe` before its earlier
                                // one. Rare with the GIL, because a producer only
                                // gets to enqueue while this thread is parked;
                                // constant on a free-threaded interpreter, where
                                // producers append all through the drain.
                                ready_batch.append(pending.deref_mut());
                            }
                        }
                        if pending.is_empty() {
                            self.wake.ready_pending.store(false, Ordering::Release);
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
                        self.state.lock().expect("poisoned loop state").stopping = true;
                    }
                    ReadyItem::Callback(callback) => {
                        let should_rearm = matches!(
                            callback.kind(),
                            CallbackKind::Reader(_) | CallbackKind::Writer(_)
                        );
                        let callback_error =
                            self.execute_ready(py, Some(&loop_obj), callback.as_ref())?;
                        if should_rearm {
                            deferred_fd_rearms.push(callback);
                        }
                        if let Some(err) = callback_error {
                            ready_error = Some(err);
                            break;
                        }
                    }
                    ReadyItem::HandleCallback(handle) => {
                        if let Some(err) =
                            self.execute_ready(py, Some(&loop_obj), handle.get().ready())?
                        {
                            ready_error = Some(err);
                            break;
                        }
                    }
                    ReadyItem::FutureSetResult { future, value } => {
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
                        core.drain_pending_read_events_with_py(py)?;
                    }
                    ReadyItem::StreamTransportWrite(core) => {
                        core.flush_pending_direct_write();
                    }
                    #[cfg(unix)]
                    ReadyItem::StartTcpReader { fd, core, stream } => {
                        // The runtime is installed before the ready drain begins.
                        assert!(self.spawn_io_tracked(
                            fd,
                            crate::transport::stream::run_tcp_socket_reader_task(core, stream),
                        ));
                    }
                    ReadyItem::ProcessTransport(core) => {
                        core.drain_pending_events_with_py(py)?;
                    }
                    ReadyItem::ServerAccepted { server, stream } => {
                        if let Err(err) = crate::transport::stream::spawn_accepted_transport_with_py(
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
                        self.resolve_connect_completed(py, future, fd, wait_errno)?;
                    }
                }

                processed_since_refill += 1;
                processed_this_turn += 1;
                if processed_this_turn >= MAX_READY_ITEMS_PER_TURN {
                    // stop() must not truncate callbacks already queued for
                    // this run merely because we reached the I/O budget.
                    if !self.state.lock().expect("poisoned loop state").stopping {
                        break;
                    }
                    processed_this_turn = 0;
                }
            }

            self.set_ready_drain_active(false);
            for ready in deferred_fd_rearms {
                self.rearm_fd_watch_if_needed(ready.as_ref());
            }

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

            if self.pending_timers_dirty.load(Ordering::Acquire) {
                // A producer may have published a timer while the ready drain
                // cleared the ordinary wake flag. Never park before importing it.
                continue;
            }
            // A missed cross-thread spin suggests progress needs the owning
            // reactor. During its cooldown, service local readiness before
            // detaching and constructing a park future. Keep successful
            // worker-thread ping-pong on its cheaper spin path. As before,
            // hot Python callback chains must also service local I/O.
            if !ready_batch.is_empty() || !local_ready.is_empty() || spin_cooldown > 0 {
                LOOP_RUNTIMES.with(|runtimes| {
                    runtimes.borrow()[&loop_runtime_key].poll_once();
                });
                if !ready_batch.is_empty()
                    || !local_ready.is_empty()
                    || self.wake.ready_pending.load(Ordering::Acquire)
                {
                    continue;
                }
            }

            // Wait for the next wakeup with the GIL released. First spin briefly
            // to catch an imminent cross-thread wake (reader worker / runtime
            // thread) in user space — this keeps request/response ping-pong
            // latency low and tight. On spin timeout (or after too many
            // consecutive catches, to avoid starving this loop's own runtime
            // tasks) park by driving the runtime: its `driver.wait` runs here on
            // the loop thread and is interrupted by a cross-thread wake.
            // Keep the `!Send` runtime lookup and future construction inside
            // the closure so the closure itself satisfies PyO3's `Ungil`
            // bound without bypassing PyO3's attachment bookkeeping.
            let park_deadline = local_timers.deadline();
            py.detach(|| {
                let mut caught = false;
                // A spin that times out is pure loss: the loop thread burns a
                // whole window and then parks anyway. That is cheap when the
                // loop is otherwise idle, but this thread is the bottleneck
                // whenever the protocol does real work per message (websockets,
                // ASGI), where one wasted window costs more than a callback.
                // So back off after a miss and re-probe once the cooldown ends,
                // which keeps the win on quiet ping-pong loops — there spins
                // essentially always catch — without paying for it under load.
                if spin_window.is_zero() || spin_cooldown > 0 {
                    spin_cooldown = spin_cooldown.saturating_sub(1);
                } else {
                    let spin_deadline = (Instant::now() + spin_window).min(park_deadline);
                    'spin: loop {
                        for _ in 0..64 {
                            if self.wake.ready_pending.load(Ordering::Acquire) {
                                caught = true;
                                break 'spin;
                            }
                            std::hint::spin_loop();
                        }
                        if Instant::now() >= spin_deadline {
                            spin_cooldown = SPIN_MISS_COOLDOWN_PARKS;
                            break 'spin;
                        }
                    }
                }

                if caught && consecutive_spins < MAX_CONSECUTIVE_SPINS {
                    consecutive_spins += 1;
                } else {
                    consecutive_spins = 0;
                    let wait = WaitForWake::until(Arc::clone(&self.wake), park_deadline);
                    LOOP_RUNTIMES.with(|runtimes| {
                        let runtimes = runtimes.borrow();
                        let runtime = runtimes
                            .get(&loop_runtime_key)
                            .expect("loop runtime missing");
                        runtime.block_on(wait);
                    });
                }
            });
        };

        self.set_ready_drain_active(false);
        drop(local_timers);
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
        if let Err(err) = self.send_command(LoopCommand::Run(LoopRunCommand::FinishRun { done_tx }))
        {
            self.reset_run_state_after_finish_error();
            return Err(pyo3::exceptions::PyRuntimeError::new_err(err.to_string()));
        }

        // SIGNAL_POLL_INTERVAL is only the cadence for checking Python signals
        // while the loop is parked. Finishing a run requires a round trip
        // through the runtime command queue, which can legitimately take
        // longer when watcher commands are pending or the OS delays the
        // runtime thread.
        match py.detach(move || done_rx.recv_timeout(RUN_FINISH_TIMEOUT)) {
            Ok(()) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {
                self.reset_run_state_after_finish_error();
                return Err(pyo3::exceptions::PyRuntimeError::new_err(
                    "timed out while finishing event loop run",
                ));
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                self.reset_run_state_after_finish_error();
                return Err(pyo3::exceptions::PyRuntimeError::new_err(
                    "event loop runtime terminated unexpectedly",
                ));
            }
        }

        run_result
    }
}

impl LoopCore {
    fn reset_run_state_after_finish_error(&self) {
        let mut state = self.state.lock().expect("poisoned loop state");
        state.running = false;
        state.stopping = false;
    }

    /// Requests a graceful stop of the active run session.
    ///
    /// Already-ready callbacks are still processed according to asyncio's
    /// stop semantics before `run_forever` returns.
    pub fn schedule_stop(&self) -> Result<(), LoopCoreError> {
        self.send_command(LoopCommand::RequestStop)
    }

    /// Permanently closes the loop and joins its dispatcher thread.
    ///
    /// Closing is idempotent, but an actively running loop returns
    /// `LoopCoreError::Running`. Tracked I/O tasks are cancelled before the
    /// on-thread runtime is dropped.
    pub fn close(&self) -> Result<(), LoopCoreError> {
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

        // Release Python callbacks outside the timer mutex (their finalizers
        // may re-enter the loop). No active LocalTimers exists while stopped.
        let timers =
            std::mem::take(&mut *self.pending_timers.lock().expect("poisoned pending timers"));
        self.pending_timers_dirty.store(false, Ordering::Release);
        drop(timers);

        // Drop this loop's on-thread vibeio runtime now, while the loop thread
        // (and vibeio's own thread-locals) are still alive. Letting it drop
        // during thread destruction trips a TLS-access panic. `close()` runs on
        // the loop thread with the loop stopped, so no `block_on` is active.
        let runtime_key = self as *const LoopCore as usize;
        // Cancel tracked tasks while the runtime and its driver are alive, then
        // drop the runtime after cancellation has released pending operations.
        IO_TASKS.with(|tasks| {
            if let Some(handles) = tasks.borrow_mut().remove(&runtime_key) {
                for (_, handle) in handles {
                    handle.cancel();
                }
            }
        });
        LOOP_RUNTIMES.with(|runtimes| {
            if let Some(runtime) = runtimes.borrow_mut().remove(&runtime_key) {
                drop(std::mem::ManuallyDrop::into_inner(runtime));
            }
        });

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
    /// Dispatches an asyncio error-context dictionary to the configured handler.
    ///
    /// Falls back to [`LoopCore::default_exception_handler`] when no custom
    /// handler is installed.
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

    /// Writes an unhandled callback error and traceback to Python's `sys.stderr`.
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

    /// Invokes one ready callback and converts callback failures into loop errors.
    ///
    /// Returns a secondary error only when reporting the original callback
    /// failure through the exception handler also fails.
    pub fn execute_ready(
        &self,
        py: Python<'_>,
        loop_obj: Option<&Py<PyAny>>,
        ready: &ReadyCallback,
    ) -> PyResult<Option<PyErr>> {
        if ready.cancelled() {
            return Ok(None);
        }

        match ready.invoke(py) {
            Ok(_) => Ok(None),
            Err(err) => handle_callback_error(
                py,
                self,
                loop_obj,
                err,
                format!("<{:?} id={}>", ready.kind(), ready.id()),
            ),
        }
    }

    fn rearm_fd_watch_if_needed(&self, ready: &ReadyCallback) {
        // Readiness callbacks are one-shot at the runtime layer. Re-arm them
        // only after the current ready batch has drained so callbacks queued
        // by Future.set_result() can remove the registration first. This
        // matches asyncio's selector-cycle ordering and prevents a still-
        // readable fd from resolving the same Future twice.
        //
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

    /// Marks the ready queue non-empty and wakes the parked loop thread. Used by
    /// cross-thread ready producers (the transitional runtime thread, signal and
    /// transport workers).
    #[inline]
    pub(crate) fn signal_ready(&self) {
        self.wake.signal();
    }

    /// Spawns a detached I/O task on this loop's on-thread vibeio runtime. Must
    /// be called on the loop thread (asyncio contract). The task begins running
    /// the next time the loop parks in `block_on`; its completions push ready
    /// items and wake the loop **on the same thread**, with no cross-thread hop.
    /// Returns `false` if the loop has no runtime yet (spawned before first run).
    pub(crate) fn spawn_io<F>(&self, future: F) -> bool
    where
        F: Future<Output = ()> + 'static,
    {
        let key = self as *const LoopCore as usize;
        LOOP_RUNTIMES.with(|runtimes| {
            let runtimes = runtimes.borrow();
            match runtimes.get(&key) {
                // Detach the JoinHandle: the task manages its own lifetime.
                Some(runtime) => {
                    std::mem::drop(runtime.spawn(future));
                    true
                }
                None => false,
            }
        })
    }

    /// Spawns a cancellable I/O task (accept loop / socket reader) on this loop's
    /// runtime, tracked by `fd` so `stop_io_task` can cancel it. Any existing
    /// task registered for `fd` is cancelled first. Must run on the loop thread.
    /// Returns `false` if the loop has no runtime yet.
    pub(crate) fn spawn_io_tracked<F>(&self, fd: RawFd, future: F) -> bool
    where
        F: Future<Output = ()> + 'static,
    {
        let key = self as *const LoopCore as usize;
        let handle =
            LOOP_RUNTIMES.with(|runtimes| runtimes.borrow().get(&key).map(|rt| rt.spawn(future)));
        match handle {
            Some(handle) => {
                IO_TASKS.with(|tasks| {
                    if let Some(old) = tasks
                        .borrow_mut()
                        .entry(key)
                        .or_default()
                        .insert(fd, handle)
                    {
                        old.cancel();
                    }
                });
                true
            }
            None => false,
        }
    }

    /// Cancels the tracked I/O task registered for `fd`, if any. Must run on the
    /// loop thread.
    ///
    /// Returns whether a task was actually found and cancelled. `IO_TASKS` is a
    /// thread-local, so a task handed to the runtime thread instead is simply
    /// not here, and the caller has to cancel it through the command channel.
    pub(crate) fn stop_io_task(&self, fd: RawFd) -> bool {
        let key = self as *const LoopCore as usize;
        IO_TASKS.with(|tasks| {
            if let Some(map) = tasks.borrow_mut().get_mut(&key)
                && let Some(handle) = map.remove(&fd)
            {
                handle.cancel();
                return true;
            }
            false
        })
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
    /// reactor. Runs on the loop thread so the `SO_ERROR` check and the
    /// `set_result` / `set_exception` happen with the GIL already held for the
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
            #[cfg(unix)]
            LoopCommand::Io(LoopIoCommand::StartSocketReader {
                fd,
                core,
                reader: crate::transport::stream::ReaderTarget::Tcp(stream),
            }) if !core.uses_native_stream_reader() => self
                .try_enqueue_local_ready(ReadyItem::StartTcpReader { fd, core, stream })
                .or_else(|item| self.try_enqueue_active_ready(item))
                .map_err(|item| match item {
                    ReadyItem::StartTcpReader { fd, core, stream } => {
                        LoopCommand::Io(LoopIoCommand::StartSocketReader {
                            fd,
                            core,
                            reader: crate::transport::stream::ReaderTarget::Tcp(stream),
                        })
                    }
                    _ => unreachable!("local reader enqueue preserves item kind"),
                }),
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
                    #[cfg(unix)]
                    ReadyItem::StartTcpReader { fd, core, stream } => {
                        LoopCommand::Io(LoopIoCommand::StartSocketReader {
                            fd,
                            core,
                            reader: crate::transport::stream::ReaderTarget::Tcp(stream),
                        })
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
            if !std::ptr::eq(tls.core.get(), self) {
                return Err(item);
            }

            let ready = tls.ready_queue.get();
            if ready.is_null() {
                return Err(item);
            }

            // SAFETY: `ready` points to run_forever's stack-local queue on this
            // thread. Neither callback invocation nor runtime polling holds a
            // mutable reference to it across this call.
            unsafe { (*ready).push_back(item) };
            if !tls.drain_active.get() {
                // I/O futures are polled with Python detached, but still on
                // this thread. Wake the park future without locking the
                // cross-thread pending queue for each transport event.
                self.wake.signal();
            }
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
        self.wake.signal();
        Ok(())
    }
}

#[cfg(test)]
mod wake_tests {
    use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

    use futures::task::{ArcWake, noop_waker, waker};

    use super::*;

    struct WakeCounter(AtomicUsize);

    impl ArcWake for WakeCounter {
        fn wake_by_ref(arc_self: &Arc<Self>) {
            arc_self.0.fetch_add(1, AtomicOrdering::SeqCst);
        }
    }

    #[test]
    fn loop_wake_coalesces_signals_until_pending_is_cleared() {
        let wake = Arc::new(LoopWake::new());
        let counter = Arc::new(WakeCounter(AtomicUsize::new(0)));
        let task_waker = waker(Arc::clone(&counter));
        wake.ready_waker.register(&task_waker);

        wake.signal();
        wake.signal();
        assert!(wake.ready_pending.load(Ordering::Acquire));
        assert_eq!(counter.0.load(AtomicOrdering::SeqCst), 1);

        wake.ready_pending.store(false, Ordering::Release);
        wake.ready_waker.register(&task_waker);
        wake.signal();
        assert_eq!(counter.0.load(AtomicOrdering::SeqCst), 2);
    }

    #[test]
    fn wait_for_wake_observes_a_signal_that_arrived_before_polling() {
        let wake = Arc::new(LoopWake::new());
        wake.signal();
        let mut wait = Box::pin(WaitForWake::new(wake, Duration::from_secs(1)));
        let task_waker = noop_waker();
        let mut context = Context::from_waker(&task_waker);

        assert_eq!(wait.as_mut().poll(&mut context), Poll::Ready(()));
    }

    #[test]
    fn registered_waiter_is_woken_by_a_cross_thread_signal() {
        let wake = Arc::new(LoopWake::new());
        let counter = Arc::new(WakeCounter(AtomicUsize::new(0)));
        wake.ready_waker.register(&waker(Arc::clone(&counter)));

        let producer_wake = Arc::clone(&wake);
        std::thread::spawn(move || producer_wake.signal())
            .join()
            .expect("signal producer");

        assert!(wake.ready_pending.load(Ordering::Acquire));
        assert_eq!(counter.0.load(AtomicOrdering::SeqCst), 1);
    }

    #[test]
    fn wait_for_wake_completes_when_its_timer_expires() {
        let runtime = crate::vibeio::RuntimeBuilder::new()
            .enable_timer(true)
            .build()
            .expect("timer-enabled runtime");
        let wake = Arc::new(LoopWake::new());
        let started = Instant::now();

        runtime.block_on(WaitForWake::new(
            Arc::clone(&wake),
            Duration::from_millis(2),
        ));

        assert!(!wake.ready_pending.load(Ordering::Acquire));
        assert!(started.elapsed() >= Duration::from_millis(1));
    }

    #[test]
    fn command_channel_accepts_concurrent_producers_and_disconnects_cleanly() {
        const PRODUCERS: usize = 8;
        const COMMANDS_PER_PRODUCER: usize = 128;

        let core = LoopCore::new();
        let producers = (0..PRODUCERS)
            .map(|_| {
                let core = Arc::clone(&core);
                std::thread::spawn(move || {
                    for _ in 0..COMMANDS_PER_PRODUCER {
                        core.send_command(LoopCommand::RequestStop)
                            .expect("dispatcher should accept concurrent commands");
                    }
                })
            })
            .collect::<Vec<_>>();

        for producer in producers {
            producer.join().expect("command producer");
        }

        core.send_command(LoopCommand::Close)
            .expect("dispatcher close command");
        core.runtime_thread
            .lock()
            .expect("runtime thread mutex")
            .take()
            .expect("runtime thread")
            .join()
            .expect("runtime thread join");

        assert!(matches!(
            core.send_command(LoopCommand::RequestStop),
            Err(LoopCoreError::ChannelClosed)
        ));
    }
}
