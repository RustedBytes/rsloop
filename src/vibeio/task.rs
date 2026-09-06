#![forbid(unsafe_code)]

use std::cell::RefCell;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::Waker;

use crossbeam_queue::SegQueue;
use futures_util::future::LocalBoxFuture;
use futures_util::task::{ArcWake, WakerRef, waker, waker_ref};

use crate::vibeio::driver::AnyInterruptor;

pub(crate) struct RemoteWakeContext {
    pub(crate) queue: Arc<SegQueue<Arc<TaskWake>>>,
    pub(crate) interruptor: AnyInterruptor,
    pub(crate) waiting: Arc<AtomicBool>,
    pub(crate) interrupt_pending: Arc<AtomicBool>,
}

/// Only this Send + Sync proxy crosses threads through a Waker.
pub(crate) struct TaskWake {
    pub(crate) remote_wake: std::sync::Weak<RemoteWakeContext>,
    pub(crate) queued: AtomicBool,
    pub(crate) thread_id: std::thread::ThreadId,
    pub(crate) token: usize,
}

pub struct Task {
    pub future: RefCell<Option<LocalBoxFuture<'static, ()>>>,
    pub(crate) wake: Arc<TaskWake>,
    pub token: usize,
}

impl Task {
    #[inline]
    pub fn waker_ref(self: &Rc<Self>) -> WakerRef<'_> {
        // Borrowing the proxy does not increment its reference count. Only
        // cloning the resulting Waker takes an owned proxy reference.
        waker_ref(&self.wake)
    }

    #[inline]
    pub fn waker(self: &Rc<Self>) -> Waker {
        waker(self.wake.clone())
    }

    #[inline]
    pub fn mark_dequeued(&self) {
        self.wake.queued.store(false, Ordering::Relaxed);
    }
}

impl ArcWake for TaskWake {
    #[inline]
    fn wake_by_ref(wake: &Arc<Self>) {
        Self::enqueue_if_needed(wake);
    }
}

impl TaskWake {
    fn enqueue_if_needed(wake: &Arc<Self>) {
        let Some(remote) = wake.remote_wake.upgrade() else {
            return;
        };
        if !wake.queued.swap(true, Ordering::Relaxed) {
            if std::thread::current().id() == wake.thread_id
                && crate::vibeio::executor::enqueue_local_wake(wake, &remote)
            {
                return;
            }
            remote.queue.push(wake.clone());
        }
        if remote.waiting.load(Ordering::Acquire)
            && !remote.interrupt_pending.swap(true, Ordering::AcqRel)
        {
            remote.interruptor.interrupt();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn task() -> Rc<Task> {
        Rc::new(Task {
            future: RefCell::new(None),
            wake: Arc::new(TaskWake {
                remote_wake: std::sync::Weak::new(),
                queued: AtomicBool::new(false),
                thread_id: std::thread::current().id(),
                token: 0,
            }),
            token: 0,
        })
    }

    #[test]
    fn borrowed_waker_only_clones_thread_safe_proxy() {
        fn assert_send_sync<T: Send + Sync>() {}
        assert_send_sync::<TaskWake>();
        let task = task();
        let owned = {
            let borrowed = task.waker_ref();
            assert_eq!(Arc::strong_count(&task.wake), 1);
            borrowed.wake_by_ref();
            assert_eq!(Arc::strong_count(&task.wake), 1);
            let owned = borrowed.clone();
            assert_eq!(Arc::strong_count(&task.wake), 2);
            assert_eq!(Rc::strong_count(&task), 1);
            owned
        };
        drop(owned);
        assert_eq!(Arc::strong_count(&task.wake), 1);
    }

    #[test]
    fn remote_waker_cannot_retain_local_task() {
        let task = task();
        let local = Rc::downgrade(&task);
        let proxy = Arc::downgrade(&task.wake);
        let owned = task.waker_ref().clone();
        drop(task);
        assert!(local.upgrade().is_none());
        assert!(proxy.upgrade().is_some());
        std::thread::spawn(move || owned.wake()).join().unwrap();
        assert!(proxy.upgrade().is_none());
    }

    #[test]
    fn borrowed_wakes_enqueue_one_proxy_until_dequeued() {
        let driver = crate::vibeio::driver::AnyDriver::new_mock();
        let remote = Arc::new(RemoteWakeContext {
            queue: Arc::new(SegQueue::new()),
            interruptor: driver.get_interruptor(),
            waiting: Arc::new(AtomicBool::new(false)),
            interrupt_pending: Arc::new(AtomicBool::new(false)),
        });
        let mut task = task();
        Arc::get_mut(&mut Rc::get_mut(&mut task).unwrap().wake)
            .unwrap()
            .remote_wake = Arc::downgrade(&remote);
        let borrowed = task.waker_ref();
        for _ in 0..2 {
            borrowed.wake_by_ref();
            borrowed.wake_by_ref();
            assert_eq!(Arc::strong_count(&task.wake), 2);
            let queued = remote.queue.pop().unwrap();
            assert!(Arc::ptr_eq(&queued, &task.wake));
            assert!(remote.queue.pop().is_none());
            drop(queued);
            assert_eq!(Arc::strong_count(&task.wake), 1);
            task.mark_dequeued();
        }
    }

    #[test]
    fn local_future_is_dropped_on_owner_thread_despite_remote_waker() {
        struct ThreadBoundDrop(std::thread::ThreadId, std::rc::Rc<std::cell::Cell<bool>>);
        impl Drop for ThreadBoundDrop {
            fn drop(&mut self) {
                assert_eq!(std::thread::current().id(), self.0);
                self.1.set(true);
            }
        }
        let task = task();
        let dropped = std::rc::Rc::new(std::cell::Cell::new(false));
        let guard = ThreadBoundDrop(std::thread::current().id(), dropped.clone());
        *task.future.borrow_mut() = Some(Box::pin(async move {
            let _guard = guard;
            std::future::pending::<()>().await;
        }));
        let owned = task.waker();
        drop(task);
        assert!(dropped.get());
        std::thread::spawn(move || drop(owned)).join().unwrap();
    }

    #[test]
    fn borrowed_waker_does_not_release_task_during_unwind() {
        let task = task();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let _waker = task.waker_ref();
            panic!("poll panicked");
        }));
        assert!(result.is_err());
        assert_eq!(Arc::strong_count(&task.wake), 1);
    }
}
