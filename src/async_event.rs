//! Small broadcast notification primitive used by transport shutdown paths.

use std::sync::Mutex;

use futures::channel::oneshot;

/// Registers one-shot waiters and wakes every waiter present at notification
/// time.
pub struct AsyncEvent {
    waiters: Mutex<Vec<oneshot::Sender<()>>>,
}

impl AsyncEvent {
    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "AsyncEvent"))]
    pub fn new() -> Self {
        Self {
            waiters: Mutex::new(Vec::with_capacity(4)),
        }
    }

    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "AsyncEvent"))]
    pub fn listen(&self) -> oneshot::Receiver<()> {
        let (tx, rx) = oneshot::channel();
        self.waiters
            .lock()
            .expect("poisoned async event waiters")
            .push(tx);
        rx
    }

    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "AsyncEvent"))]
    pub fn notify_all(&self) {
        let mut pending = {
            let mut waiters = self.waiters.lock().expect("poisoned async event waiters");
            if waiters.is_empty() {
                return;
            }
            std::mem::take(&mut *waiters)
        };
        // Sending can invoke an arbitrary waker synchronously. Notify this
        // snapshot outside the lock; reentrant listeners belong to the next
        // one.
        for waiter in pending.drain(..) {
            let _ = waiter.send(());
        }
        // Reuse the allocation when callbacks/concurrent threads have not
        // registered more waiters. Never overwrite their live registrations.
        let mut waiters = self.waiters.lock().expect("poisoned async event waiters");
        if waiters.is_empty() && pending.capacity() > waiters.capacity() {
            *waiters = pending;
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        future::Future,
        pin::Pin,
        sync::{
            Arc, Mutex,
            atomic::{AtomicBool, Ordering},
        },
        task::{Context, Wake, Waker},
    };

    use super::AsyncEvent;

    #[test]
    fn notification_callbacks_can_register_the_next_listener() {
        struct RegisterListener {
            event: Arc<AsyncEvent>,
            unlocked: AtomicBool,
            next: Mutex<Option<futures::channel::oneshot::Receiver<()>>>,
        }
        impl Wake for RegisterListener {
            fn wake(self: Arc<Self>) {
                self.wake_by_ref();
            }

            fn wake_by_ref(self: &Arc<Self>) {
                // Detect the old deadlock without making the test hang.
                let unlocked = self.event.waiters.try_lock().is_ok();
                self.unlocked.store(unlocked, Ordering::Relaxed);
                if unlocked {
                    *self.next.lock().unwrap() = Some(self.event.listen());
                }
            }
        }

        let event = Arc::new(AsyncEvent::new());
        let callback = Arc::new(RegisterListener {
            event: Arc::clone(&event),
            unlocked: AtomicBool::new(false),
            next: Mutex::new(None),
        });
        let waker = Waker::from(Arc::clone(&callback));
        let mut first = event.listen();
        assert!(
            Pin::new(&mut first)
                .poll(&mut Context::from_waker(&waker))
                .is_pending()
        );

        event.notify_all();
        assert!(callback.unlocked.load(Ordering::Relaxed));
        assert_eq!(first.try_recv().unwrap(), Some(()));
        let mut next = callback.next.lock().unwrap().take().unwrap();
        assert_eq!(next.try_recv().unwrap(), None);
        event.notify_all();
        assert_eq!(next.try_recv().unwrap(), Some(()));
    }

    #[test]
    fn notification_reuses_waiter_capacity_between_bursts() {
        let event = AsyncEvent::new();
        let mut listeners: Vec<_> = (0..16).map(|_| event.listen()).collect();
        let capacity = event.waiters.lock().unwrap().capacity();
        event.notify_all();
        assert!(
            listeners
                .iter_mut()
                .all(|listener| listener.try_recv().unwrap() == Some(()))
        );
        assert_eq!(event.waiters.lock().unwrap().capacity(), capacity);
        event.notify_all();
        assert_eq!(event.waiters.lock().unwrap().capacity(), capacity);
    }

    #[test]
    fn notification_wakes_all_current_listeners() {
        let event = AsyncEvent::new();
        let mut first = event.listen();
        let mut second = event.listen();

        event.notify_all();

        assert_eq!(first.try_recv().expect("first listener"), Some(()));
        assert_eq!(second.try_recv().expect("second listener"), Some(()));
    }

    #[test]
    fn listeners_registered_after_notification_wait_for_the_next_one() {
        let event = AsyncEvent::new();
        event.notify_all();
        let mut listener = event.listen();

        assert_eq!(listener.try_recv().expect("pending listener"), None);
        event.notify_all();
        assert_eq!(listener.try_recv().expect("notified listener"), Some(()));
    }

    #[test]
    fn dropped_listener_does_not_prevent_other_notifications() {
        let event = AsyncEvent::new();
        let dropped = event.listen();
        let mut active = event.listen();
        drop(dropped);

        event.notify_all();

        assert_eq!(active.try_recv().expect("active listener"), Some(()));
        assert!(event.waiters.lock().expect("event waiters").is_empty());
    }
}
