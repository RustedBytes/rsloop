use std::cell::{RefCell, UnsafeCell};
use std::collections::VecDeque;
use std::rc::Weak;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::task::{RawWaker, RawWakerVTable, Waker};

use crossbeam_queue::SegQueue;
use futures_util::future::LocalBoxFuture;

use crate::driver::AnyInterruptor;

pub struct Task {
    pub future: RefCell<Option<LocalBoxFuture<'static, ()>>>,
    pub queue: Weak<UnsafeCell<VecDeque<Arc<Task>>>>,
    pub next_task: Weak<RefCell<Option<Arc<Task>>>>,
    pub remote_queue: std::sync::Weak<SegQueue<usize>>,
    pub interruptor: AnyInterruptor,
    pub queued: AtomicBool,
    pub thread_id: std::thread::ThreadId,
    pub token: usize,
    pub waiting: std::sync::Weak<AtomicBool>,
    pub interrupt_pending: std::sync::Weak<AtomicBool>,
}

impl Task {
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

        if !task.queued.swap(true, Ordering::Relaxed) {
            if let Some(remote_queue) = task.remote_queue.upgrade() {
                remote_queue.push(task.token);
            }
        }

        // Interrupt the driver if it's waiting
        if task
            .waiting
            .upgrade()
            .is_some_and(|waiting| waiting.load(Ordering::Acquire))
        {
            let should_interrupt = task
                .interrupt_pending
                .upgrade()
                .is_none_or(|pending| !pending.swap(true, Ordering::AcqRel));
            if should_interrupt {
                // Interrupt the driver if the waker is not on the same thread as the runtime
                task.interruptor.interrupt();
            }
        }
    }

    #[inline]
    pub fn mark_dequeued(&self) {
        self.queued.store(false, Ordering::Relaxed);
    }
}
