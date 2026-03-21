use std::sync::Mutex;

use futures::channel::oneshot;

pub struct AsyncEvent {
    waiters: Mutex<Vec<oneshot::Sender<()>>>,
}

impl AsyncEvent {
    pub fn new() -> Self {
        Self {
            waiters: Mutex::new(Vec::with_capacity(4)),
        }
    }

    pub fn listen(&self) -> oneshot::Receiver<()> {
        let (tx, rx) = oneshot::channel();
        self.waiters
            .lock()
            .expect("poisoned async event waiters")
            .push(tx);
        rx
    }

    pub fn notify_all(&self) {
        let mut waiters = self.waiters.lock().expect("poisoned async event waiters");
        let drained = waiters.drain(..);
        for waiter in drained {
            let _ = waiter.send(());
        }
    }
}
