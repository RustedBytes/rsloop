use std::cell::{RefCell, UnsafeCell};
use std::collections::VecDeque;
use std::mem::ManuallyDrop;
use std::rc::Weak;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{RawWaker, RawWakerVTable, Waker};

use crossbeam_queue::SegQueue;
use futures_util::future::LocalBoxFuture;
use futures_util::task::WakerRef;

use crate::vibeio::driver::AnyInterruptor;

pub(crate) struct RemoteWakeContext {
    pub(crate) queue: Arc<SegQueue<usize>>,
    pub(crate) interruptor: AnyInterruptor,
    pub(crate) waiting: Arc<AtomicBool>,
    pub(crate) interrupt_pending: Arc<AtomicBool>,
}

pub struct Task {
    pub future: RefCell<Option<LocalBoxFuture<'static, ()>>>,
    pub queue: Weak<UnsafeCell<VecDeque<Arc<Task>>>>,
    pub next_task: Weak<RefCell<Option<Arc<Task>>>>,
    pub remote_wake: std::sync::Weak<RemoteWakeContext>,
    pub queued: AtomicBool,
    pub thread_id: std::thread::ThreadId,
    pub token: usize,
}

impl Task {
    /// Borrow the polling task's reference instead of incrementing its Arc count.
    #[inline]
    pub fn waker_ref(self: &Arc<Self>) -> WakerRef<'_> {
        // SAFETY: the returned lifetime keeps `self` alive. WakerRef suppresses
        // the borrowed waker's destructor and only exposes &Waker. Cloning it
        // uses our normal vtable to acquire an owned Arc, so futures may retain
        // cloned wakers or send them to other threads after this borrow ends.
        let waker = unsafe { Waker::from_raw(Self::raw_waker(Arc::as_ptr(self).cast())) };
        WakerRef::new_unowned(ManuallyDrop::new(waker))
    }

    #[inline]
    pub fn waker(self: &Arc<Self>) -> Waker {
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
        let task = Arc::<Self>::from_raw(ptr as *const Self);
        let cloned = Arc::clone(&task);
        let _ = Arc::into_raw(task);
        Self::raw_waker(Arc::into_raw(cloned) as *const ())
    }

    #[inline]
    unsafe fn raw_waker_wake(ptr: *const ()) {
        let task = Arc::<Self>::from_raw(ptr as *const Self);
        Self::enqueue_if_needed(&task);
    }

    #[inline]
    unsafe fn raw_waker_wake_by_ref(ptr: *const ()) {
        let task = Arc::<Self>::from_raw(ptr as *const Self);
        Self::enqueue_if_needed(&task);
        let _ = Arc::into_raw(task);
    }

    #[inline]
    unsafe fn raw_waker_drop(ptr: *const ()) {
        drop(Arc::<Self>::from_raw(ptr as *const Self));
    }

    #[inline]
    fn enqueue_if_needed(task: &Arc<Self>) {
        if std::thread::current().id() == task.thread_id {
            if !task.queued.swap(true, Ordering::Relaxed) {
                let mut pushed_next = false;
                if let Some(next_task) = task.next_task.upgrade() {
                    let mut next_task = next_task.borrow_mut();
                    if next_task.is_none() {
                        *next_task = Some(Arc::clone(task));
                        pushed_next = true;
                    }
                }
                if !pushed_next {
                    if let Some(queue) = task.queue.upgrade() {
                        // SAFETY: the runtime is single-threaded and only mutates the ready
                        // queue from that thread. We also never hold a mutable queue borrow
                        // while polling task futures, so re-entrant wakes do not alias.
                        unsafe {
                            (&mut *queue.get()).push_back(Arc::clone(task));
                        }
                    }
                }
            }
            return;
        }

        let Some(remote) = task.remote_wake.upgrade() else {
            return;
        };
        if !task.queued.swap(true, Ordering::Relaxed) {
            remote.queue.push(task.token);
        }

        // Interrupt the driver if it's waiting.
        if remote.waiting.load(Ordering::Acquire) {
            let should_interrupt = !remote.interrupt_pending.swap(true, Ordering::AcqRel);
            if should_interrupt {
                remote.interruptor.interrupt();
            }
        }
    }

    #[inline]
    pub fn mark_dequeued(&self) {
        self.queued.store(false, Ordering::Relaxed);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task() -> Arc<Task> {
        #[allow(clippy::arc_with_non_send_sync)]
        Arc::new(Task {
            future: RefCell::new(None),
            queue: Weak::new(),
            next_task: Weak::new(),
            remote_wake: std::sync::Weak::new(),
            queued: AtomicBool::new(false),
            thread_id: std::thread::current().id(),
            token: 0,
        })
    }

    #[test]
    fn borrowed_waker_only_owns_references_when_cloned() {
        let task = task();
        let borrowed = task.waker_ref();
        assert_eq!(Arc::strong_count(&task), 1);
        let owned = borrowed.clone();
        assert_eq!(Arc::strong_count(&task), 2);
        borrowed.wake_by_ref();
        assert!(task.queued.load(Ordering::Relaxed));
        drop(borrowed);
        assert_eq!(Arc::strong_count(&task), 2);
        task.mark_dequeued();
        owned.wake();
        assert!(task.queued.load(Ordering::Relaxed));
        assert_eq!(Arc::strong_count(&task), 1);
    }

    #[test]
    fn cloned_borrowed_waker_outlives_task_owner_and_wakes_remotely() {
        let task = task();
        let weak = Arc::downgrade(&task);
        let owned = task.waker_ref().clone();
        drop(task);
        assert!(weak.upgrade().is_some());
        std::thread::spawn(move || owned.wake()).join().unwrap();
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn borrowed_waker_does_not_release_task_during_unwind() {
        let task = task();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _waker = task.waker_ref();
            panic!("poll panicked");
        }));
        assert!(result.is_err());
        assert_eq!(Arc::strong_count(&task), 1);
    }
}
