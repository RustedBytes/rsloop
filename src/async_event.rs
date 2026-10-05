//! Small broadcast notification primitive used by transport shutdown paths.

use std::sync::Mutex;

use futures::channel::oneshot;

/// Registers one-shot waiters and wakes every waiter present at notification
/// time.
pub struct AsyncEvent {
    waiters: Mutex<Vec<oneshot::Sender<()>>>,
}

impl AsyncEvent {
    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "AsyncEvent")
    )]
    pub fn new() -> Self {
        Self {
            waiters: Mutex::new(Vec::with_capacity(4)),
        }
    }

    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "AsyncEvent")
    )]
    pub fn listen(&self) -> oneshot::Receiver<()> {
        let (tx, rx) = oneshot::channel();
        self.waiters
            .lock()
            .expect("poisoned async event waiters")
            .push(tx);
        rx
    }

    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "AsyncEvent")
    )]
    pub fn notify_all(&self) {
        let mut waiters = self.waiters.lock().expect("poisoned async event waiters");
        let drained = waiters.drain(..);
        for waiter in drained {
            let _ = waiter.send(());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::AsyncEvent;

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
