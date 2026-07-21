//! Async runtime and task execution utilities.
//!
//! This module provides the core async runtime infrastructure:
//! - `Runtime`: the main async runtime that drives futures to completion.
//! - `spawn`: spawn a task on the current runtime.
//! - `spawn_blocking`: spawn a blocking task on the thread pool.
//! - `JoinHandle`: a handle to a spawned task that can be awaited.
//! - `current_driver`: get the driver for the current runtime.
//!
//! # Examples
//!
//! ```ignore
//! use vibeio::RuntimeBuilder;
//!
//! let runtime = RuntimeBuilder::new().build().unwrap();
//! let value = runtime.block_on(async {
//!     // Spawn a task and await its result
//!     let handle = vibeio::spawn(async { 42 });
//!     handle.await + 10
//! });
//! assert_eq!(value, 52);
//! ```
//!
//! # Implementation notes
//! - The runtime is single-threaded and uses a work-stealing queue.
//! - Tasks are polled in batches for better performance.
//! - The runtime supports timers, blocking pools, and file I/O offloading via features.

use std::cell::{RefCell, UnsafeCell};
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{Context, Poll, RawWaker, RawWakerVTable, Waker};

use crossbeam_queue::SegQueue;
use slab::Slab;

#[cfg(feature = "blocking-default")]
use crate::blocking::DefaultBlockingThreadPool;
use crate::blocking::{BlockingThreadPool, SpawnBlockingError};
use crate::driver::{AnyDriver, AnyInterruptor};
#[cfg(feature = "process")]
use crate::process::{start_zombie_reaper, ZombieReaperMessage};
use crate::task::Task;
#[cfg(feature = "time")]
use crate::timer::Timer;

thread_local! {
    static CURRENT_RUNTIME: RefCell<Option<Rc<RuntimeInner>>> = const { RefCell::new(None) };
}

/// Internal state for a spawned task.
///
/// Stores the task's output and a waker to notify when the task completes.
struct JoinState<T> {
    output: Option<T>,
    waker: Option<Waker>,
    canceled: bool,
    task: std::sync::Weak<Task>,
}

#[inline]
fn update_waker_slot(waiter_slot: &mut Option<Waker>, waker: &Waker) {
    if !waiter_slot
        .as_ref()
        .is_some_and(|waiter| waiter.will_wake(waker))
    {
        *waiter_slot = Some(waker.clone());
    }
}

struct SpawnFuture<F, T> {
    future: F,
    state: Rc<RefCell<JoinState<T>>>,
}

impl<F, T> Future for SpawnFuture<F, T>
where
    F: Future<Output = T>,
{
    type Output = ();

    #[inline]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: once `self` is pinned we never move `future`.
        let this = unsafe { self.get_unchecked_mut() };

        if this.state.borrow().canceled {
            return Poll::Ready(());
        }

        // SAFETY: `future` is pinned together with `self`.
        match unsafe { Pin::new_unchecked(&mut this.future) }.poll(cx) {
            Poll::Ready(output) => {
                let mut state = this.state.borrow_mut();
                state.output = Some(output);
                if let Some(waker) = state.waker.take() {
                    waker.wake();
                }
                Poll::Ready(())
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

/// A handle to a spawned asynchronous task.
///
/// This handle implements `Future` and can be `await`ed to retrieve the task's output.
/// It allows you to wait for a spawned task to complete and get its result.
///
/// # Examples
/// ```ignore
/// let handle = vibeio::spawn(async { 42 });
/// let result = handle.await;  // result == 42
/// ```
pub struct JoinHandle<T> {
    state: Rc<RefCell<JoinState<T>>>,
}

impl<T> JoinHandle<T> {
    /// Creates a new `JoinHandle` with the given state.
    #[inline]
    fn new(state: Rc<RefCell<JoinState<T>>>) -> Self {
        Self { state }
    }

    /// Cancels the task associated with this handle.
    ///
    /// The task will be interrupted and not resumed.
    #[inline]
    pub fn cancel(self) {
        let task = {
            let mut state = self.state.borrow_mut();
            state.canceled = true;
            state.task.upgrade()
        };

        if let Some(task) = task {
            // JoinHandle is !Send, and tasks are bound to this single-threaded
            // runtime, so cancellation can drop the future synchronously. This
            // is important for pending overlapped I/O: dropping the operation
            // initiates cancellation before the caller can reuse the socket.
            task.future.borrow_mut().take();
            task.waker().wake();
        }
    }
}

impl<T> Future for JoinHandle<T> {
    type Output = T;

    #[inline]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let mut state = self.state.borrow_mut();
        if let Some(output) = state.output.take() {
            Poll::Ready(output)
        } else {
            update_waker_slot(&mut state.waker, cx.waker());
            Poll::Pending
        }
    }
}

struct BlockOnNotify {
    ready: AtomicBool,
    thread_id: std::thread::ThreadId,
    interruptor: AnyInterruptor,
    waiting: Arc<AtomicBool>,
    interrupt_pending: Arc<AtomicBool>,
}

impl BlockOnNotify {
    #[inline]
    fn new(
        interruptor: AnyInterruptor,
        waiting: Arc<AtomicBool>,
        interrupt_pending: Arc<AtomicBool>,
    ) -> Arc<Self> {
        Arc::new(Self {
            ready: AtomicBool::new(true),
            thread_id: std::thread::current().id(),
            interruptor,
            waiting,
            interrupt_pending,
        })
    }

    #[inline]
    fn is_ready(&self) -> bool {
        self.ready.load(Ordering::Acquire)
    }

    #[inline]
    fn take_ready(&self) -> bool {
        self.ready.swap(false, Ordering::AcqRel)
    }

    #[inline]
    fn wake_by_ref(&self) {
        self.ready.store(true, Ordering::Release);

        if std::thread::current().id() != self.thread_id
            && self.waiting.load(Ordering::Acquire)
            && !self.interrupt_pending.swap(true, Ordering::AcqRel)
        {
            self.interruptor.interrupt();
        }
    }

    #[inline]
    fn waker(self: &Arc<Self>) -> Waker {
        // SAFETY: the vtable methods correctly clone/drop the Arc reference count.
        unsafe { Waker::from_raw(Self::raw_waker(Arc::into_raw(Arc::clone(self)) as *const ())) }
    }

    #[inline]
    unsafe fn raw_waker(ptr: *const ()) -> RawWaker {
        RawWaker::new(ptr, &Self::VTABLE)
    }

    const VTABLE: RawWakerVTable = RawWakerVTable::new(
        Self::raw_waker_clone,
        Self::raw_waker_wake,
        Self::raw_waker_wake_by_ref,
        Self::raw_waker_drop,
    );

    #[inline]
    unsafe fn raw_waker_clone(ptr: *const ()) -> RawWaker {
        let notify = Arc::<Self>::from_raw(ptr as *const Self);
        let cloned = Arc::clone(&notify);
        let _ = Arc::into_raw(notify);
        Self::raw_waker(Arc::into_raw(cloned) as *const ())
    }

    #[inline]
    unsafe fn raw_waker_wake(ptr: *const ()) {
        let notify = Arc::<Self>::from_raw(ptr as *const Self);
        notify.wake_by_ref();
    }

    #[inline]
    unsafe fn raw_waker_wake_by_ref(ptr: *const ()) {
        let notify = Arc::<Self>::from_raw(ptr as *const Self);
        notify.wake_by_ref();
        let _ = Arc::into_raw(notify);
    }

    #[inline]
    unsafe fn raw_waker_drop(ptr: *const ()) {
        drop(Arc::<Self>::from_raw(ptr as *const Self));
    }
}

struct CurrentRuntimeGuard;

impl CurrentRuntimeGuard {
    /// Enter a runtime, making it available for spawning tasks.
    ///
    /// Panics if called while already inside a runtime.
    #[inline]
    fn enter(runtime_inner: Rc<RuntimeInner>) -> Self {
        CURRENT_RUNTIME.with(|runtime| {
            let mut runtime = runtime.borrow_mut();
            if runtime.is_some() {
                panic!("can't spawn a runtime inside another runtime");
            }

            *runtime = Some(runtime_inner);
        });

        Self
    }
}

impl Drop for CurrentRuntimeGuard {
    /// Exit the runtime, clearing the current runtime reference.
    #[inline]
    fn drop(&mut self) {
        CURRENT_RUNTIME.with(|runtime| {
            let mut runtime = runtime.borrow_mut();
            *runtime = None;
        });
    }
}

/// Get the I/O driver for the current runtime.
///
/// Returns `None` if called outside a runtime context.
pub(crate) fn current_driver() -> Option<Rc<AnyDriver>> {
    CURRENT_RUNTIME.with(|runtime| {
        let runtime = runtime.borrow();
        runtime
            .as_ref()
            .map(|runtime_inner| runtime_inner.driver.clone())
    })
}

/// Get the timer for the current runtime.
///
/// Returns `None` if called outside a runtime context or if timers are not enabled.
#[cfg(feature = "time")]
pub(crate) fn current_timer() -> Option<Rc<Timer>> {
    CURRENT_RUNTIME.with(|runtime| {
        let runtime = runtime.borrow();
        runtime.as_ref().map(|runtime_inner| {
            runtime_inner
                .timer
                .as_ref()
                .expect("timer not enabled")
                .clone()
        })
    })
}

/// Get the zombie reaper channel for the current runtime.
///
/// Returns `None` if called outside a runtime context or if process support is not enabled.
#[cfg(feature = "process")]
pub(crate) async fn current_zombie_reaper() -> Option<async_channel::Sender<ZombieReaperMessage>> {
    let runtime = CURRENT_RUNTIME.with(|runtime| {
        let runtime = runtime.borrow();
        runtime.as_ref().map(|runtime_inner| runtime_inner.clone())
    })?;
    let option = runtime
        .zombie_reaper
        .try_borrow()
        .ok()
        .and_then(|e| e.as_ref().cloned());
    if let Some(option) = option {
        Some(option.clone())
    } else {
        let reaper = runtime.spawn(start_zombie_reaper()).await;
        if let Ok(mut option) = runtime.zombie_reaper.try_borrow_mut() {
            *option = Some(reaper.clone());
        }
        Some(reaper)
    }
}

/// Spawn a task on the current runtime.
///
/// This function spawns the given future on the runtime and returns a `JoinHandle`
/// that can be awaited to get the task's output.
///
/// # Panics
/// Panics if called outside a runtime context.
///
/// # Examples
/// ```ignore
/// let handle = vibeio::spawn(async { 42 });
/// let result = handle.await;
/// ```
pub fn spawn<T>(future: impl Future<Output = T> + 'static) -> JoinHandle<T>
where
    T: 'static,
{
    let runtime = CURRENT_RUNTIME.with(|runtime| {
        let runtime = runtime.borrow();
        if let Some(runtime_inner) = &*runtime {
            runtime_inner.clone()
        } else {
            panic!("can't spawn a task outside runtime");
        }
    });

    runtime.spawn(future)
}

/// Spawn a blocking task on the thread pool.
///
/// This function spawns the given closure on a blocking thread pool and returns
/// a future that resolves to the result.
///
/// # Panics
/// Panics if called outside a runtime context.
///
/// # Examples
/// ```ignore
/// let result = vibeio::spawn_blocking(|| {
///     // blocking work
///     42
/// }).await?;
/// ```
pub async fn spawn_blocking<T, F>(f: F) -> Result<T, SpawnBlockingError>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let runtime = CURRENT_RUNTIME.with(|runtime| {
        let runtime = runtime.borrow();
        if let Some(runtime_inner) = &*runtime {
            runtime_inner.clone()
        } else {
            panic!("can't spawn a blocking task outside runtime");
        }
    });

    runtime.spawn_blocking(f).await
}

/// Check if file I/O should be offloaded to blocking threads.
///
/// Returns `true` if fs offload is enabled and we're inside a runtime.
#[cfg(feature = "fs")]
#[inline]
pub(crate) fn offload_fs() -> bool {
    CURRENT_RUNTIME.with(|runtime| {
        let runtime = runtime.borrow();
        if let Some(runtime_inner) = &*runtime {
            runtime_inner.fs_offload
        } else {
            false
        }
    })
}

pub(crate) struct RuntimeInner {
    queue: Rc<UnsafeCell<VecDeque<Arc<Task>>>>,
    next_task: Rc<RefCell<Option<Arc<Task>>>>,
    remote_queue: Arc<SegQueue<usize>>,
    token_to_task: RefCell<Slab<Arc<Task>>>,
    driver: Rc<AnyDriver>,
    waiting: Arc<AtomicBool>,
    interrupt_pending: Arc<AtomicBool>,
    blocking_pool: Option<Box<dyn BlockingThreadPool>>,
    #[cfg(feature = "fs")]
    fs_offload: bool,
    #[cfg(feature = "time")]
    timer: Option<Rc<Timer>>,
    #[cfg(feature = "process")]
    zombie_reaper: RefCell<Option<async_channel::Sender<ZombieReaperMessage>>>,
}

/// The async runtime that drives futures to completion.
///
/// The runtime provides:
/// - Task spawning via `spawn()` and `block_on()`.
/// - Blocking task support via `spawn_blocking()`.
/// - Timer support (when the `time` feature is enabled).
/// - File I/O offloading (when the `fs` feature is enabled).
///
/// # Examples
/// ```ignore
/// let runtime = RuntimeBuilder::new().build().unwrap();
/// let value = runtime.block_on(async { 42 });
/// assert_eq!(value, 42);
/// ```
pub struct Runtime {
    inner: Option<Rc<RuntimeInner>>,
}

impl RuntimeInner {
    /// Spawn a task on this runtime.
    #[inline]
    pub(crate) fn spawn<T>(&self, future: impl Future<Output = T> + 'static) -> JoinHandle<T>
    where
        T: 'static,
    {
        let state = Rc::new(RefCell::new(JoinState {
            output: None,
            waker: None,
            canceled: false,
            task: std::sync::Weak::new(),
        }));
        let future = Box::pin(SpawnFuture {
            future,
            state: state.clone(),
        });

        let mut slab = self.token_to_task.borrow_mut();
        let vacant_slab_entry = slab.vacant_entry();
        #[allow(clippy::arc_with_non_send_sync)]
        let task = Arc::new(Task {
            future: RefCell::new(Some(future)),
            queue: Rc::downgrade(&self.queue),
            next_task: Rc::downgrade(&self.next_task),
            remote_queue: Arc::downgrade(&self.remote_queue),
            queued: AtomicBool::new(true),
            thread_id: std::thread::current().id(),
            interruptor: self.driver.get_interruptor(),
            waiting: Arc::downgrade(&self.waiting),
            interrupt_pending: Arc::downgrade(&self.interrupt_pending),
            token: vacant_slab_entry.key(),
        });
        state.borrow_mut().task = Arc::downgrade(&task);
        vacant_slab_entry.insert(task.clone());

        self.enqueue(task);
        JoinHandle::new(state)
    }

    /// Spawn a blocking task on this runtime's thread pool.
    #[inline]
    pub(crate) async fn spawn_blocking<T, F>(&self, f: F) -> Result<T, SpawnBlockingError>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        let pool = self
            .blocking_pool
            .as_ref()
            .expect("blocking pool not initialized");
        crate::blocking::spawn_blocking(pool.as_ref(), f).await
    }

    /// Enqueue a task for polling.
    #[inline]
    fn enqueue(&self, task: Arc<Task>) {
        // SAFETY: this runtime is single-threaded. All ready-queue mutation goes
        // through runtime/task wake paths on the same thread.
        unsafe {
            (&mut *self.queue.get()).push_back(task);
        }
    }

    /// Drain ready tasks into the given batch.
    #[inline]
    fn drain_ready(&self, batch: &mut Vec<Arc<Task>>, mut budget: usize) {
        if budget != 0 {
            let slab = self.token_to_task.borrow();
            while budget != 0 {
                let Some(token) = self.remote_queue.pop() else {
                    break;
                };
                if let Some(task) = slab.get(token) {
                    task.mark_dequeued();
                    batch.push(task.clone());
                    budget -= 1;
                }
            }
        }

        // SAFETY: this runtime is single-threaded and we only hold this mutable
        // access while draining the queue before polling any task futures.
        let queue = unsafe { &mut *self.queue.get() };
        while budget != 0 {
            let Some(task) = queue.pop_front() else {
                break;
            };
            task.mark_dequeued();
            batch.push(task);
            budget -= 1;
        }
    }

    #[inline]
    fn stop_waiting(&self) {
        self.waiting.store(false, Ordering::Release);
        self.interrupt_pending.store(false, Ordering::Release);
    }

    #[inline]
    fn should_skip_wait(&self) -> bool {
        if self.next_task.borrow().is_some() || !self.remote_queue.is_empty() {
            return true;
        }

        // SAFETY: the runtime only mutates the local ready queue on the runtime thread.
        unsafe { !(&*self.queue.get()).is_empty() }
    }

    /// Take the next task to run, if any.
    #[inline]
    fn take_next_task(&self) -> Option<Arc<Task>> {
        let task = self.next_task.take();
        if let Some(task) = &task {
            task.mark_dequeued();
        }
        task
    }
}

impl Runtime {
    /// Create a new runtime with the given driver.
    ///
    /// By default, this enables the timer and file I/O offload.
    #[allow(dead_code)]
    #[inline]
    pub(crate) fn new(driver: AnyDriver) -> Self {
        #[cfg(not(feature = "blocking-default"))]
        let blocking_pool = None;
        #[cfg(feature = "blocking-default")]
        let blocking_pool: Option<Box<dyn BlockingThreadPool>> =
            Some(Box::new(DefaultBlockingThreadPool::new()));
        Self::with_options(driver, true, blocking_pool, true)
    }

    /// Create a new runtime with the given driver and options.
    #[inline]
    pub(crate) fn with_options(
        driver: AnyDriver,
        enable_timer: bool,
        blocking_pool: Option<Box<dyn BlockingThreadPool>>,
        fs_offload: bool,
    ) -> Self {
        #[cfg(not(feature = "fs"))]
        let _ = fs_offload;

        let ready_queue = Rc::new(UnsafeCell::new(VecDeque::with_capacity(4096)));
        Runtime {
            inner: Some(Rc::new(RuntimeInner {
                queue: ready_queue,
                next_task: Rc::new(RefCell::new(None)),
                remote_queue: Arc::new(SegQueue::new()),
                token_to_task: RefCell::new(Slab::with_capacity(4096)),
                driver: Rc::new(driver),
                waiting: Arc::new(AtomicBool::new(false)),
                interrupt_pending: Arc::new(AtomicBool::new(false)),
                blocking_pool,
                #[cfg(feature = "fs")]
                fs_offload,
                #[cfg(feature = "time")]
                timer: if enable_timer {
                    Some(Rc::new(Timer::new()))
                } else {
                    None
                },
                #[cfg(feature = "process")]
                zombie_reaper: RefCell::new(None),
            })),
        }
    }

    /// Spawn a task on this runtime.
    ///
    /// Returns a `JoinHandle` that can be awaited to get the task's output.
    #[inline]
    pub fn spawn<T>(&self, future: impl Future<Output = T> + 'static) -> JoinHandle<T>
    where
        T: 'static,
    {
        self.inner
            .as_ref()
            .expect("runtime has been dropped")
            .spawn(future)
    }

    /// Spawn a blocking task on this runtime's thread pool.
    #[inline]
    pub async fn spawn_blocking<T, F>(&self, f: F) -> Result<T, SpawnBlockingError>
    where
        T: Send + 'static,
        F: FnOnce() -> T + Send + 'static,
    {
        let inner = self.inner.as_ref().expect("runtime has been dropped");
        inner.spawn_blocking(f).await
    }

    /// Run the runtime and execute the given future to completion.
    ///
    /// This method blocks the current thread and drives the runtime until
    /// the provided future completes.
    #[inline]
    pub fn block_on<T>(&self, future: impl Future<Output = T> + 'static) -> T
    where
        T: 'static,
    {
        let inner = self.inner.as_ref().expect("runtime has been dropped");
        let _runtime_guard = CurrentRuntimeGuard::enter(inner.clone());

        let mut future = std::pin::pin!(future);
        let root_notify = BlockOnNotify::new(
            inner.driver.get_interruptor(),
            inner.waiting.clone(),
            inner.interrupt_pending.clone(),
        );
        let root_waker = root_notify.waker();
        let mut batch = Vec::with_capacity(256);

        loop {
            if root_notify.take_ready() {
                let mut context = Context::from_waker(&root_waker);
                if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
                    return output;
                }
            }

            let mut next_task_taken = false;

            batch.clear();

            let mut budget = 256;
            if let Some(next_task) = inner.take_next_task() {
                batch.push(next_task);
                next_task_taken = true;
                budget -= 1;
            }
            // Always drain to fill the rest of the batch
            if budget > 0 {
                inner.drain_ready(&mut batch, budget);
            }

            if batch.is_empty() {
                if root_notify.is_ready() {
                    continue;
                }

                inner.interrupt_pending.store(false, Ordering::Release);
                inner.waiting.store(true, Ordering::Release);

                if root_notify.is_ready() || inner.should_skip_wait() {
                    inner.stop_waiting();
                    continue;
                }

                #[cfg(feature = "time")]
                let (deadline, woken_up) = if let Some(timer) = inner.timer.as_ref() {
                    // Spin the timing wheel
                    timer.spin_and_get_deadline()
                } else {
                    (None, false)
                };

                #[cfg(feature = "time")]
                if woken_up {
                    inner.stop_waiting();
                    continue;
                }

                #[cfg(feature = "time")]
                inner.driver.wait(deadline);
                #[cfg(not(feature = "time"))]
                inner.driver.wait(None);

                inner.stop_waiting();
                continue;
            }

            #[cfg(feature = "time")]
            if batch.len() > 64 {
                if let Some(timer) = inner.timer.as_ref() {
                    // Spin the timing wheel to avoid starving timers
                    let _ = timer.spin_and_get_deadline();
                }
            }

            for task in batch.drain(..) {
                let mut future_slot = task.future.borrow_mut();
                if let Some(mut future) = future_slot.take() {
                    drop(future_slot);
                    let waker = task.waker();
                    let mut context = Context::from_waker(&waker);

                    if future.as_mut().poll(&mut context).is_pending() {
                        let mut future_slot = task.future.borrow_mut();
                        *future_slot = Some(future);
                    } else {
                        // Future completed, remove task from token_to_task slab to prevent memory leaks
                        inner.token_to_task.borrow_mut().remove(task.token);
                    }
                } else {
                    // Cancellation can synchronously drop the future and then
                    // enqueue the task so its slab entry is reclaimed here.
                    // Check identity in case a stale wake targets a reused token.
                    let should_remove = inner
                        .token_to_task
                        .borrow()
                        .get(task.token)
                        .is_some_and(|current| Arc::ptr_eq(current, &task));
                    if should_remove {
                        inner.token_to_task.borrow_mut().remove(task.token);
                    }
                }
            }

            if !next_task_taken && inner.driver.should_flush() {
                inner.driver.flush();
            }
        }
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        // Drop all tasks with current runtime entered
        let inner = self.inner.take().expect("runtime has been dropped");
        #[cfg(feature = "process")]
        if let Some(zombie_reaper) = inner.zombie_reaper.borrow_mut().take() {
            zombie_reaper.close();
        }
        let _runtime_guard = CurrentRuntimeGuard::enter(inner.clone());
        drop(inner);
        drop(_runtime_guard);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    struct PendingUntilDropped(Rc<Cell<bool>>);

    impl Future for PendingUntilDropped {
        type Output = ();

        fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
            Poll::Pending
        }
    }

    impl Drop for PendingUntilDropped {
        fn drop(&mut self) {
            self.0.set(true);
        }
    }

    #[test]
    fn block_on_returns_future_output() {
        let runtime = crate::executor::Runtime::new(AnyDriver::new_mock());
        let value = runtime.block_on(async { 42usize });
        assert_eq!(value, 42);
    }

    #[test]
    fn spawn_join_handle_returns_task_output() {
        let runtime = crate::executor::Runtime::new(AnyDriver::new_mock());
        let value = runtime.block_on(async {
            let handle = spawn(async { 21usize });
            handle.await * 2
        });
        assert_eq!(value, 42);
    }

    #[test]
    fn runtime_spawn_returns_join_handle() {
        let runtime = crate::executor::Runtime::new(AnyDriver::new_mock());
        let handle = runtime.spawn(async { 7usize });
        let value = runtime.block_on(handle);
        assert_eq!(value, 7);
    }

    #[test]
    fn cancel_drops_and_reclaims_pending_task() {
        let runtime = crate::executor::Runtime::new(AnyDriver::new_mock());
        let dropped = Rc::new(Cell::new(false));
        let handle = runtime.spawn(PendingUntilDropped(dropped.clone()));

        handle.cancel();
        assert!(dropped.get());

        let mut first_poll = true;
        runtime.block_on(std::future::poll_fn(move |cx| {
            if first_poll {
                first_poll = false;
                cx.waker().wake_by_ref();
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        }));
        assert!(runtime
            .inner
            .as_ref()
            .unwrap()
            .token_to_task
            .borrow()
            .is_empty());
    }

    #[test]
    fn block_on_repolls_root_future_after_self_wake() {
        let runtime = crate::executor::Runtime::new(AnyDriver::new_mock());
        let mut polled_once = false;
        let value = runtime.block_on(std::future::poll_fn(move |cx| {
            if polled_once {
                Poll::Ready(11usize)
            } else {
                polled_once = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }));
        assert_eq!(value, 11);
    }

    #[cfg(feature = "blocking-default")]
    #[test]
    fn spawn_blocking_returns_task_output() {
        let runtime = crate::executor::Runtime::new(AnyDriver::new_mock());
        let value = runtime.block_on(async {
            let handle = spawn_blocking(|| 21usize).await.unwrap();
            handle * 2
        });
        assert_eq!(value, 42);
    }
}
