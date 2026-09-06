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
//! See "Spawning and joining tasks" and "Blocking work with an explicit pool"
//! in `tools/vibeio-check/EXAMPLES.md` for executable examples.
//!
//! # Implementation notes
//! - The runtime is single-threaded, with a local ready queue and a remote wake queue.
//! - Tasks are polled in batches for better performance.
//! - The runtime supports timers, blocking pools, and file I/O offloading via features.

#![deny(unsafe_op_in_unsafe_fn)]

use std::cell::RefCell;
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Wake, Waker};

use crossbeam_queue::SegQueue;
use slab::Slab;

#[cfg(feature = "blocking-default")]
use crate::vibeio::blocking::DefaultBlockingThreadPool;
use crate::vibeio::blocking::{BlockingThreadPool, SpawnBlockingError};
use crate::vibeio::driver::{AnyDriver, AnyInterruptor};
#[cfg(feature = "process")]
use crate::vibeio::process::{ZombieReaperMessage, start_zombie_reaper};
use crate::vibeio::task::{RemoteWakeContext, Task, TaskWake};

pub(crate) fn enqueue_local_wake(wake: &Arc<TaskWake>, remote: &Arc<RemoteWakeContext>) -> bool {
    CURRENT_RUNTIME.with(|current| {
        let Ok(current) = current.try_borrow() else {
            return false;
        };
        let Some(runtime) = current.as_ref() else {
            return false;
        };
        if !Arc::ptr_eq(&runtime.remote_wake, remote) {
            return false;
        }
        let task = {
            let Ok(slab) = runtime.token_to_task.try_borrow() else {
                return false;
            };
            let Some(task) = slab.get(wake.token) else {
                return true;
            };
            if !Arc::ptr_eq(&task.wake, wake) {
                return true;
            }
            task.clone()
        };
        let Ok(mut next) = runtime.next_task.try_borrow_mut() else {
            return false;
        };
        if next.is_none() {
            *next = Some(task);
        } else {
            runtime.enqueue(task);
        }
        true
    })
}
use crate::vibeio::timer::Timer;

#[cfg(any(target_vendor = "apple", windows))]
const MAX_PARK_INTERVAL: std::time::Duration = std::time::Duration::from_millis(50);

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
    task: std::rc::Weak<Task>,
}

pin_project_lite::pin_project! {
    struct SpawnFuture<F, T> {
        #[pin]
        future: F,
        state: Rc<RefCell<JoinState<T>>>,
    }
}

impl<F, T> Future for SpawnFuture<F, T>
where
    F: Future<Output = T>,
{
    type Output = ();

    #[inline]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.project();

        if this.state.borrow().canceled {
            return Poll::Ready(());
        }

        match this.future.poll(cx) {
            Poll::Ready(output) => {
                let mut state = this.state.borrow_mut();
                let replaced = state.output.replace(output);
                let waker = state.waker.take();
                drop(state);
                drop(replaced);
                if let Some(waker) = waker {
                    waker.wake();
                }
                Poll::Ready(())
            }
            Poll::Pending => {
                // Cancellation during the inner poll cannot take its future
                // from the task slot: the executor is currently holding it.
                // Finish now so its resources are dropped in this task batch,
                // even if the embedding loop does not run another tick.
                if this.state.borrow().canceled {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            }
        }
    }
}

/// A handle to a spawned asynchronous task.
///
/// This handle implements `Future` and can be `await`ed to retrieve the task's output.
/// It allows you to wait for a spawned task to complete and get its result.
///
/// # Examples
/// See "Spawning and joining tasks" in `tools/vibeio-check/EXAMPLES.md` for
/// awaiting an output and explicitly canceling an unpolled task.
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
    /// If called from inside the task's own poll, its future is released when
    /// that poll returns; otherwise its pending future is released immediately.
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
            let future = task.future.borrow_mut().take();
            drop(future);
            task.waker().wake();
        }
    }
}

impl<T> Future for JoinHandle<T> {
    type Output = T;

    #[inline]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        {
            let mut state = self.state.borrow_mut();
            if let Some(output) = state.output.take() {
                return Poll::Ready(output);
            }
            if state
                .waker
                .as_ref()
                .is_some_and(|waker| waker.will_wake(cx.waker()))
            {
                return Poll::Pending;
            }
        }
        // Clone and destroy custom wakers without holding the join-state borrow.
        // Keep the unchanged-waker path above free of reference-count traffic.
        let incoming = cx.waker().clone();
        let mut state = self.state.borrow_mut();
        // A custom clone callback may have driven the task to completion.
        let (result, retired) = if let Some(output) = state.output.take() {
            (Poll::Ready(output), Some(incoming))
        } else {
            (Poll::Pending, state.waker.replace(incoming))
        };
        drop(state);
        drop(retired);
        result
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

    #[cfg(any(target_vendor = "apple", windows))]
    #[inline]
    fn force_ready(&self) {
        self.ready.store(true, Ordering::Release);
    }

    #[inline]
    fn notify(&self) {
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
        Waker::from(Arc::clone(self))
    }
}

impl Wake for BlockOnNotify {
    #[inline]
    fn wake(self: Arc<Self>) {
        self.notify();
    }

    #[inline]
    fn wake_by_ref(self: &Arc<Self>) {
        self.notify();
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
pub(crate) fn current_timer() -> Option<Rc<Timer>> {
    CURRENT_RUNTIME.with(|runtime| {
        let runtime = runtime.borrow();
        runtime
            .as_ref()
            .and_then(|runtime_inner| runtime_inner.timer.clone())
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
/// See "Spawning and joining tasks" in `tools/vibeio-check/EXAMPLES.md`.
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
/// # Errors
/// Returns `SpawnBlockingError` if no runtime or blocking pool is available,
/// or if the pool does not deliver a result.
///
/// # Examples
/// See "Blocking work with an explicit pool" in
/// `tools/vibeio-check/EXAMPLES.md` for a checked result and pool configuration.
pub async fn spawn_blocking<T, F>(f: F) -> Result<T, SpawnBlockingError>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let runtime = CURRENT_RUNTIME
        .with(|runtime| runtime.borrow().as_ref().cloned())
        .ok_or(SpawnBlockingError)?;

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
    queue: RefCell<VecDeque<Rc<Task>>>,
    next_task: Rc<RefCell<Option<Rc<Task>>>>,
    remote_wake: Arc<RemoteWakeContext>,
    token_to_task: RefCell<Slab<Rc<Task>>>,
    driver: Rc<AnyDriver>,
    task_batch_size: usize,
    timer_poll_threshold: usize,
    blocking_pool: Option<Box<dyn BlockingThreadPool>>,
    #[cfg(feature = "fs")]
    fs_offload: bool,
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
/// See "Spawning and joining tasks" in `tools/vibeio-check/EXAMPLES.md`.
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
            task: std::rc::Weak::new(),
        }));
        let future = Box::pin(SpawnFuture {
            future,
            state: state.clone(),
        });

        let mut slab = self.token_to_task.borrow_mut();
        let vacant_slab_entry = slab.vacant_entry();
        let task = Rc::new(Task {
            future: RefCell::new(Some(future)),
            wake: Arc::new(TaskWake {
                remote_wake: Arc::downgrade(&self.remote_wake),
                queued: AtomicBool::new(true),
                thread_id: std::thread::current().id(),
                token: vacant_slab_entry.key(),
            }),
            token: vacant_slab_entry.key(),
        });
        state.borrow_mut().task = Rc::downgrade(&task);
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
        let pool = self.blocking_pool.as_ref().ok_or(SpawnBlockingError)?;
        crate::vibeio::blocking::spawn_blocking(pool.as_ref(), f).await
    }

    /// Enqueue a task for polling.
    #[inline]
    fn enqueue(&self, task: Rc<Task>) {
        self.queue.borrow_mut().push_back(task);
    }

    /// Drain ready tasks into the given batch.
    #[inline]
    fn drain_ready(&self, batch: &mut Vec<Rc<Task>>, mut budget: usize) {
        if budget != 0 {
            let slab = self.token_to_task.borrow();
            while budget != 0 {
                let Some(wake) = self.remote_wake.queue.pop() else {
                    break;
                };
                if let Some(task) = slab.get(wake.token)
                    && Arc::ptr_eq(&task.wake, &wake)
                {
                    task.mark_dequeued();
                    batch.push(task.clone());
                    budget -= 1;
                }
            }
        }

        // Release the queue borrow before polling futures or invoking callbacks.
        let mut queue = self.queue.borrow_mut();
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
        self.remote_wake.waiting.store(false, Ordering::Release);
        self.remote_wake
            .interrupt_pending
            .store(false, Ordering::Release);
    }

    #[inline]
    fn should_skip_wait(&self) -> bool {
        if self.next_task.borrow().is_some() || !self.remote_wake.queue.is_empty() {
            return true;
        }

        !self.queue.borrow().is_empty()
    }

    /// Take the next task to run, if any.
    #[inline]
    fn take_next_task(&self) -> Option<Rc<Task>> {
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
        Self::with_options(driver, true, blocking_pool, true, false)
    }

    /// Create a new runtime with the given driver and options.
    #[inline]
    pub(crate) fn with_options(
        driver: AnyDriver,
        enable_timer: bool,
        blocking_pool: Option<Box<dyn BlockingThreadPool>>,
        fs_offload: bool,
        rsloop_profile: bool,
    ) -> Self {
        #[cfg(not(feature = "fs"))]
        let _ = fs_offload;

        let (task_batch_size, timer_poll_threshold, ready_queue_capacity) = if rsloop_profile {
            (256, 64, 4096)
        } else {
            (256, 64, 256)
        };
        let ready_queue = RefCell::new(VecDeque::with_capacity(ready_queue_capacity));
        let driver = Rc::new(driver);
        let remote_wake = Arc::new(RemoteWakeContext {
            queue: Arc::new(SegQueue::new()),
            interruptor: driver.get_interruptor(),
            waiting: Arc::new(AtomicBool::new(false)),
            interrupt_pending: Arc::new(AtomicBool::new(false)),
        });
        Runtime {
            inner: Some(Rc::new(RuntimeInner {
                queue: ready_queue,
                next_task: Rc::new(RefCell::new(None)),
                remote_wake,
                token_to_task: RefCell::new(Slab::with_capacity(4096)),
                driver,
                task_batch_size,
                timer_poll_threshold,
                blocking_pool,
                #[cfg(feature = "fs")]
                fs_offload,
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

    /// Service kernel readiness and one bounded task batch without parking.
    /// Used when the embedding Python loop still has runnable callbacks.
    pub(crate) fn poll_once(&self) {
        let inner = self.inner.as_ref().expect("runtime has been dropped");
        inner.driver.wait(Some(std::time::Duration::ZERO));
        if let Some(timer) = inner.timer.as_ref() {
            let _ = timer.spin_and_get_deadline();
        }
        let mut yielded = false;
        self.block_on(std::future::poll_fn(move |cx| {
            if yielded {
                Poll::Ready(())
            } else {
                yielded = true;
                cx.waker().wake_by_ref();
                Poll::Pending
            }
        }));
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
            Arc::clone(&inner.remote_wake.waiting),
            Arc::clone(&inner.remote_wake.interrupt_pending),
        );
        let root_waker = root_notify.waker();
        let mut batch = Vec::with_capacity(inner.task_batch_size);

        loop {
            if root_notify.take_ready() {
                let mut context = Context::from_waker(&root_waker);
                if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
                    return output;
                }
            }

            batch.clear();

            let mut budget = inner.task_batch_size;
            if let Some(next_task) = inner.take_next_task() {
                batch.push(next_task);
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

                inner
                    .remote_wake
                    .interrupt_pending
                    .store(false, Ordering::Release);
                inner.remote_wake.waiting.store(true, Ordering::Release);

                if root_notify.is_ready() || inner.should_skip_wait() {
                    inner.stop_waiting();
                    continue;
                }

                let (deadline, woken_up) = if let Some(timer) = inner.timer.as_ref() {
                    // Spin the timing wheel
                    timer.spin_and_get_deadline()
                } else {
                    (None, false)
                };

                if woken_up {
                    inner.stop_waiting();
                    continue;
                }

                #[cfg(any(target_vendor = "apple", windows))]
                inner
                    .driver
                    .wait(Some(deadline.map_or(MAX_PARK_INTERVAL, |deadline| {
                        deadline.min(MAX_PARK_INTERVAL)
                    })));
                #[cfg(not(any(target_vendor = "apple", windows)))]
                inner.driver.wait(deadline);

                // A bounded wait on platforms with native interrupt recovery is
                // also a recovery poll. Normal cross-thread notifications still
                // wake immediately, while a missed notification can delay the
                // root future by at most one interval instead of parking forever.
                #[cfg(any(target_vendor = "apple", windows))]
                root_notify.force_ready();

                inner.stop_waiting();
                continue;
            }

            if batch.len() > inner.timer_poll_threshold {
                if let Some(timer) = inner.timer.as_ref() {
                    // Spin the timing wheel to avoid starving timers
                    let _ = timer.spin_and_get_deadline();
                }
            }

            for task in batch.drain(..) {
                let mut future_slot = task.future.borrow_mut();
                if let Some(mut future) = future_slot.take() {
                    drop(future_slot);
                    let waker = task.waker_ref();
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
                        .is_some_and(|current| Rc::ptr_eq(current, &task));
                    if should_remove {
                        inner.token_to_task.borrow_mut().remove(task.token);
                    }
                }
            }

            // Completion submissions must reach the kernel even when a task
            // continually wakes itself into the single-task fast slot. The old
            // `next_task_taken` guard could postpone io_uring SQEs indefinitely.
            if inner.driver.should_flush() {
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
        // Clear the TLS reference while `inner` still owns the runtime. If the
        // TLS-held `Rc` were the last one, assigning `None` would drop pending
        // task futures while the `RefCell` is mutably borrowed; I/O future
        // destructors calling `current_driver()` would then panic.
        drop(_runtime_guard);
        // Driver registrations can hold task wakers whose futures own that same
        // driver. Break this cycle explicitly; dropping RuntimeInner alone cannot
        // cancel those tasks. Detach the slab before running user destructors.
        let tasks = std::mem::take(&mut *inner.token_to_task.borrow_mut());
        for (_, task) in tasks {
            let future = task.future.borrow_mut().take();
            drop(future);
        }
        drop(inner);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;
    use std::task::{RawWaker, RawWakerVTable};

    #[test]
    fn local_ready_queue_preserves_fifo_and_drain_budget() {
        let runtime = Runtime::new(AnyDriver::new_mock());
        let handles: Vec<_> = (0..3)
            .map(|_| runtime.spawn(std::future::pending::<()>()))
            .collect();
        let tasks: Vec<_> = handles
            .iter()
            .map(|handle| handle.state.borrow().task.upgrade().unwrap())
            .collect();
        let inner = runtime.inner.as_ref().unwrap();
        let mut batch = Vec::new();
        inner.drain_ready(&mut batch, 0);
        assert!(batch.is_empty());
        assert_eq!(inner.queue.borrow().len(), 3);
        inner.drain_ready(&mut batch, 2);
        assert_eq!(batch.len(), 2);
        for (actual, expected) in batch.iter().zip(&tasks) {
            assert!(Rc::ptr_eq(actual, expected));
            assert!(!actual.wake.queued.load(Ordering::Relaxed));
        }
        assert!(tasks[2].wake.queued.load(Ordering::Relaxed));
        assert!(inner.should_skip_wait());
        batch.clear();
        inner.drain_ready(&mut batch, 2);
        assert_eq!(batch.len(), 1);
        assert!(Rc::ptr_eq(&batch[0], &tasks[2]));
        assert!(!inner.should_skip_wait());
    }

    #[test]
    fn root_waker_preserves_ownership_and_local_notifications() {
        let driver = AnyDriver::new_mock();
        let waiting = Arc::new(AtomicBool::new(true));
        let interrupted = Arc::new(AtomicBool::new(false));
        let notify = BlockOnNotify::new(driver.get_interruptor(), waiting, interrupted.clone());
        assert!(notify.take_ready());
        let waker = notify.waker();
        let cloned = waker.clone();
        assert_eq!(Arc::strong_count(&notify), 3);
        waker.wake_by_ref();
        assert!(notify.take_ready());
        assert!(!notify.take_ready());
        assert_eq!(Arc::strong_count(&notify), 3);
        cloned.wake();
        assert!(notify.take_ready());
        assert_eq!(Arc::strong_count(&notify), 2);
        assert!(!interrupted.load(Ordering::Acquire));
        let weak = Arc::downgrade(&notify);
        drop(notify);
        assert!(weak.upgrade().is_some());
        drop(waker);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn root_waker_notifies_across_threads_without_consuming_borrowed_ownership() {
        let driver = AnyDriver::new_mock();
        let interrupted = Arc::new(AtomicBool::new(false));
        let notify = BlockOnNotify::new(
            driver.get_interruptor(),
            Arc::new(AtomicBool::new(true)),
            interrupted.clone(),
        );
        notify.take_ready();
        let waker = notify.waker();
        let waker = std::thread::spawn(move || {
            waker.wake_by_ref();
            waker.wake_by_ref();
            waker
        })
        .join()
        .unwrap();
        assert!(notify.take_ready());
        assert!(interrupted.load(Ordering::Acquire));
        assert_eq!(Arc::strong_count(&notify), 2);
        let weak = Arc::downgrade(&notify);
        drop(notify);
        std::thread::spawn(move || waker.wake()).join().unwrap();
        assert!(weak.upgrade().is_none());
    }

    thread_local! {
        static JOIN_REENTRY: RefCell<Option<Box<dyn Fn()>>> = RefCell::new(None);
    }

    struct JoinReentryScope;
    impl Drop for JoinReentryScope {
        fn drop(&mut self) {
            let callback = JOIN_REENTRY.with(|slot| slot.borrow_mut().take());
            drop(callback);
        }
    }

    fn reenter_join() {
        JOIN_REENTRY.with(|slot| {
            if let Some(callback) = slot.borrow().as_ref() {
                callback();
            }
        });
    }

    struct JoinWake;
    impl std::task::Wake for JoinWake {
        fn wake(self: Arc<Self>) {
            reenter_join();
        }
    }
    impl Drop for JoinWake {
        fn drop(&mut self) {
            reenter_join();
        }
    }

    #[test]
    fn join_waiter_callbacks_can_reenter_on_replacement_and_completion() {
        for complete in [false, true] {
            let state = Rc::new(RefCell::new(JoinState {
                output: None,
                waker: None,
                canceled: false,
                task: std::rc::Weak::new(),
            }));
            let observed = state.clone();
            let calls = Rc::new(Cell::new(0));
            let called = calls.clone();
            JOIN_REENTRY.with(|slot| {
                *slot.borrow_mut() = Some(Box::new(move || {
                    assert!(
                        observed.try_borrow_mut().is_ok(),
                        "join callback ran under state borrow"
                    );
                    called.set(called.get() + 1);
                }));
            });
            let _scope = JoinReentryScope;
            let mut handle = JoinHandle::new(state.clone());
            let waker = Waker::from(Arc::new(JoinWake));
            assert!(
                Pin::new(&mut handle)
                    .poll(&mut Context::from_waker(&waker))
                    .is_pending()
            );
            drop(waker);
            let mut cx = Context::from_waker(Waker::noop());
            if complete {
                let mut future = std::pin::pin!(SpawnFuture {
                    future: std::future::ready(42),
                    state
                });
                assert!(future.as_mut().poll(&mut cx).is_ready());
                assert_eq!(Pin::new(&mut handle).poll(&mut cx), Poll::Ready(42));
            } else {
                assert!(Pin::new(&mut handle).poll(&mut cx).is_pending());
            }
            assert!(calls.get() > 0);
        }
    }

    #[test]
    fn cancellation_drops_future_outside_its_slot_borrow() {
        struct CheckDrop {
            task: Rc<RefCell<std::rc::Weak<Task>>>,
            dropped: Rc<Cell<bool>>,
        }
        impl Drop for CheckDrop {
            fn drop(&mut self) {
                let task = self.task.borrow().upgrade().unwrap();
                assert!(task.future.try_borrow_mut().is_ok());
                self.dropped.set(true);
            }
        }
        let runtime = Runtime::new(AnyDriver::new_mock());
        let task = Rc::new(RefCell::new(std::rc::Weak::new()));
        let dropped = Rc::new(Cell::new(false));
        let guard = CheckDrop {
            task: task.clone(),
            dropped: dropped.clone(),
        };
        let handle = runtime.spawn(async move {
            let _guard = guard;
            std::future::pending::<()>().await;
        });
        *task.borrow_mut() = handle.state.borrow().task.clone();
        handle.cancel();
        assert!(dropped.get());
    }

    #[test]
    fn join_rechecks_completion_after_reentrant_waker_clone() {
        unsafe fn clone(_: *const ()) -> RawWaker {
            reenter_join();
            RawWaker::new(std::ptr::null(), &VTABLE)
        }
        unsafe fn ignore(_: *const ()) {}
        const VTABLE: RawWakerVTable = RawWakerVTable::new(clone, ignore, ignore, ignore);
        let state = Rc::new(RefCell::new(JoinState {
            output: None,
            waker: None,
            canceled: false,
            task: std::rc::Weak::new(),
        }));
        let observed = state.clone();
        JOIN_REENTRY.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                observed.borrow_mut().output = Some(42);
            }));
        });
        let _scope = JoinReentryScope;
        let mut handle = JoinHandle::new(state.clone());
        // SAFETY: this stateless vtable never dereferences its null data pointer
        // or owns resources. Only clone invokes the current thread's callback.
        let waker = unsafe { Waker::from_raw(RawWaker::new(std::ptr::null(), &VTABLE)) };
        assert_eq!(
            Pin::new(&mut handle).poll(&mut Context::from_waker(&waker)),
            Poll::Ready(42)
        );
        assert!(state.borrow().waker.is_none());
    }

    #[test]
    fn missing_blocking_pool_returns_error() {
        let runtime = Runtime::with_options(AnyDriver::new_mock(), true, None, false, false);
        let result = runtime.block_on(async { spawn_blocking(|| 42).await });
        assert_eq!(result, Err(SpawnBlockingError));
    }

    #[test]
    fn blocking_spawn_without_runtime_returns_error_and_drops_closure() {
        struct Captured(Arc<AtomicBool>);
        impl Drop for Captured {
            fn drop(&mut self) {
                self.0.store(true, Ordering::Release);
            }
        }
        let dropped = Arc::new(AtomicBool::new(false));
        let captured = Captured(dropped.clone());
        let mut future = Box::pin(spawn_blocking(move || {
            let _captured = captured;
            panic!("closure must not run without a runtime");
        }));
        assert!(matches!(
            future
                .as_mut()
                .poll(&mut Context::from_waker(Waker::noop())),
            Poll::Ready(Err(SpawnBlockingError))
        ));
        assert!(dropped.load(Ordering::Acquire));
    }

    #[test]
    fn spawn_without_runtime_panics_and_drops_unpolled_future() {
        let lifetime = std::sync::Arc::new(());
        let weak = std::sync::Arc::downgrade(&lifetime);
        let future = async move {
            let _lifetime = lifetime;
            panic!("future must not be polled without a runtime");
        };
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| spawn(future)));
        let panic = result
            .err()
            .expect("spawn's documented precondition must be enforced");
        assert_eq!(
            panic.downcast_ref::<&str>(),
            Some(&"can't spawn a task outside runtime")
        );
        assert!(weak.upgrade().is_none(), "rejected future must be released");
    }

    #[test]
    fn stale_remote_wake_cannot_wake_reused_task_slot() {
        let runtime = Runtime::new(AnyDriver::new_mock());
        let first = runtime.spawn(std::future::pending::<()>());
        let task = first.state.borrow().task.upgrade().unwrap();
        let token = task.token;
        let stale = task.waker();
        drop(task);
        first.cancel();
        runtime.poll_once();

        let polls = Rc::new(Cell::new(0));
        let count = polls.clone();
        let replacement = runtime.spawn(std::future::poll_fn(move |_| {
            count.set(count.get() + 1);
            Poll::<()>::Pending
        }));
        assert_eq!(
            replacement.state.borrow().task.upgrade().unwrap().token,
            token
        );
        runtime.poll_once();
        assert_eq!(polls.get(), 1);
        std::thread::spawn(move || stale.wake()).join().unwrap();
        runtime.poll_once();
        assert_eq!(polls.get(), 1, "stale wake targeted a different task");
        replacement.cancel();
    }

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

    struct ChecksDriverOnDrop;

    impl Future for ChecksDriverOnDrop {
        type Output = ();

        fn poll(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Self::Output> {
            Poll::Pending
        }
    }

    impl Drop for ChecksDriverOnDrop {
        fn drop(&mut self) {
            let _ = current_driver();
        }
    }

    #[test]
    fn block_on_returns_future_output() {
        let runtime = crate::vibeio::executor::Runtime::new(AnyDriver::new_mock());
        let value = runtime.block_on(async { 42usize });
        assert_eq!(value, 42);
    }

    #[test]
    fn poll_once_services_tasks_without_draining_a_self_waking_task() {
        let runtime = Runtime::new(AnyDriver::new_mock());
        runtime.poll_once();
        let polls = Rc::new(Cell::new(0));
        let task_polls = Rc::clone(&polls);
        let handle = runtime.spawn(std::future::poll_fn(move |cx| {
            task_polls.set(task_polls.get() + 1);
            cx.waker().wake_by_ref();
            Poll::<()>::Pending
        }));
        runtime.poll_once();
        assert_eq!(polls.get(), 1);
        runtime.poll_once();
        assert_eq!(polls.get(), 2);
        handle.cancel();
        runtime.poll_once();
    }

    #[test]
    fn spawn_join_handle_returns_task_output() {
        let runtime = crate::vibeio::executor::Runtime::new(AnyDriver::new_mock());
        let value = runtime.block_on(async {
            let handle = spawn(async { 21usize });
            handle.await * 2
        });
        assert_eq!(value, 42);
    }

    #[test]
    fn runtime_spawn_returns_join_handle() {
        let runtime = crate::vibeio::executor::Runtime::new(AnyDriver::new_mock());
        let handle = runtime.spawn(async { 7usize });
        let value = runtime.block_on(handle);
        assert_eq!(value, 7);
    }

    #[test]
    fn cancel_drops_and_reclaims_pending_task() {
        let runtime = crate::vibeio::executor::Runtime::new(AnyDriver::new_mock());
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
        assert!(
            runtime
                .inner
                .as_ref()
                .unwrap()
                .token_to_task
                .borrow()
                .is_empty()
        );
    }

    #[test]
    fn self_cancellation_releases_future_when_its_current_poll_returns() {
        struct OnDrop(Rc<Cell<usize>>);
        impl Drop for OnDrop {
            fn drop(&mut self) {
                self.0.set(self.0.get() + 1);
            }
        }
        for ready in [false, true] {
            let runtime = Runtime::new(AnyDriver::new_mock());
            let slot = Rc::new(RefCell::new(None::<JoinHandle<()>>));
            let own_handle = slot.clone();
            let drops = Rc::new(Cell::new(0));
            let guard = OnDrop(drops.clone());
            let polls = Rc::new(Cell::new(0));
            let polled = polls.clone();
            let handle = runtime.spawn(std::future::poll_fn(move |_| {
                let _keep_alive = &guard;
                polled.set(polled.get() + 1);
                own_handle.borrow_mut().take().unwrap().cancel();
                if ready {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            }));
            *slot.borrow_mut() = Some(handle);
            runtime.poll_once();
            assert_eq!(polls.get(), 1);
            assert_eq!(drops.get(), 1, "cancellation must not require another tick");
            assert!(
                runtime
                    .inner
                    .as_ref()
                    .unwrap()
                    .token_to_task
                    .borrow()
                    .is_empty()
            );
        }
    }

    #[test]
    fn spawned_pinned_future_keeps_its_address_until_cancellation_drop() {
        struct PinnedFuture {
            address: Cell<usize>,
            dropped: Rc<Cell<bool>>,
            _pin: std::marker::PhantomPinned,
        }
        impl Future for PinnedFuture {
            type Output = ();
            fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<()> {
                let this = self.as_ref().get_ref();
                let address = this as *const Self as usize;
                let previous = this.address.replace(address);
                assert!(previous == 0 || previous == address);
                Poll::Pending
            }
        }
        impl Drop for PinnedFuture {
            fn drop(&mut self) {
                assert_eq!(self.address.get(), self as *const Self as usize);
                self.dropped.set(true);
            }
        }
        let runtime = Runtime::new(AnyDriver::new_mock());
        let dropped = Rc::new(Cell::new(false));
        let handle = runtime.spawn(PinnedFuture {
            address: Cell::new(0),
            dropped: dropped.clone(),
            _pin: std::marker::PhantomPinned,
        });
        runtime.poll_once();
        handle.cancel();
        assert!(dropped.get());
    }

    #[test]
    fn dropping_runtime_does_not_borrow_tls_reentrantly() {
        let runtime = crate::vibeio::executor::Runtime::new(AnyDriver::new_mock());
        let _handle = runtime.spawn(ChecksDriverOnDrop);
        drop(runtime);
    }

    #[test]
    fn block_on_repolls_root_future_after_self_wake() {
        let runtime = crate::vibeio::executor::Runtime::new(AnyDriver::new_mock());
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

    #[test]
    fn spawned_task_resumes_after_remote_wake() {
        let runtime = Runtime::new(AnyDriver::new_mock());
        let (sender, receiver) = futures::channel::oneshot::channel();
        let (polled, wait_for_poll) = std::sync::mpsc::channel();
        let worker = std::thread::spawn(move || {
            wait_for_poll.recv().unwrap();
            sender.send(42).unwrap();
        });
        let handle = runtime.spawn(async move {
            let mut receiver = std::pin::pin!(receiver);
            let mut polled = Some(polled);
            std::future::poll_fn(move |cx| {
                let result = receiver.as_mut().poll(cx);
                if let Some(polled) = polled.take() {
                    polled.send(()).unwrap();
                }
                result
            })
            .await
            .unwrap()
        });
        assert_eq!(runtime.block_on(handle), 42);
        worker.join().unwrap();
    }

    #[cfg(feature = "blocking-default")]
    #[test]
    fn spawn_blocking_returns_task_output() {
        let runtime = crate::vibeio::executor::Runtime::new(AnyDriver::new_mock());
        let value = runtime.block_on(async {
            let handle = spawn_blocking(|| 21usize).await.unwrap();
            handle * 2
        });
        assert_eq!(value, 42);
    }
}
