//! Blocking helper threads owned by a transport or server.
//!
//! Readers, writers, and blocking accept loops all run on plain OS threads
//! rather than the loop runtime. `WorkerThread` gives them a uniform stop
//! protocol: set the flag, optionally wake a thread parked in a syscall, then
//! join with a timeout so one stuck worker cannot block loop shutdown.

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use super::tuning::WORKER_JOIN_TIMEOUT;

pub(super) struct WorkerThread {
    stop: Arc<AtomicBool>,
    wake: Option<Box<dyn FnOnce() + Send>>,
    join: thread::JoinHandle<()>,
    done_rx: Receiver<()>,
}

impl WorkerThread {
    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "WorkerThread")
    )]
    pub(super) fn spawn(
        name: &'static str,
        task: impl FnOnce(Arc<AtomicBool>) + Send + 'static,
    ) -> io::Result<Self> {
        Self::spawn_with_stack(name, None, task)
    }

    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "WorkerThread")
    )]
    pub(super) fn spawn_with_stack(
        name: &'static str,
        stack_size: Option<usize>,
        task: impl FnOnce(Arc<AtomicBool>) + Send + 'static,
    ) -> io::Result<Self> {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let mut builder = thread::Builder::new().name(name.to_owned());
        if let Some(stack_size) = stack_size {
            builder = builder.stack_size(stack_size);
        }
        let (done_tx, done_rx) = mpsc::channel();
        let join = builder.spawn(move || {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                task(thread_stop);
            }));
            let _ = done_tx.send(());
            if let Err(payload) = result {
                std::panic::resume_unwind(payload);
            }
        })?;
        Ok(Self {
            stop,
            wake: None,
            join,
            done_rx,
        })
    }

    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "WorkerThread")
    )]
    pub(super) fn spawn_interruptible(
        name: &'static str,
        wake: impl FnOnce() + Send + 'static,
        task: impl FnOnce(Arc<AtomicBool>) + Send + 'static,
    ) -> io::Result<Self> {
        let mut worker = Self::spawn(name, task)?;
        worker.wake = Some(Box::new(wake));
        Ok(worker)
    }

    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "WorkerThread")
    )]
    pub(super) fn abort(self) {
        self.abort_with_timeout(WORKER_JOIN_TIMEOUT);
    }

    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "WorkerThread")
    )]
    fn abort_with_timeout(mut self, join_timeout: Duration) {
        self.stop.store(true, Ordering::Release);
        if let Some(wake) = self.wake.take() {
            wake();
        }
        if self.join.thread().id() == thread::current().id() {
            return;
        }
        match self.done_rx.recv_timeout(join_timeout) {
            Ok(()) | Err(RecvTimeoutError::Disconnected) => {
                let _ = self.join.join();
            }
            Err(RecvTimeoutError::Timeout) => {
                // Detach a stuck worker instead of blocking loop shutdown.
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use super::WorkerThread;

    const TEST_TIMEOUT: Duration = Duration::from_secs(1);

    #[test]
    fn abort_joins_a_worker_that_has_already_completed() {
        let (completed_tx, completed_rx) = mpsc::channel();
        let worker = WorkerThread::spawn("rsloop-test-completed", move |_| {
            completed_tx.send(()).expect("report task completion");
        })
        .expect("spawn completed worker");
        completed_rx
            .recv_timeout(TEST_TIMEOUT)
            .expect("worker should complete");

        worker.abort_with_timeout(TEST_TIMEOUT);
    }

    #[test]
    fn abort_sets_the_stop_flag_before_joining() {
        let (observed_tx, observed_rx) = mpsc::channel();
        let worker = WorkerThread::spawn("rsloop-test-stop", move |stop| {
            while !stop.load(Ordering::Acquire) {
                std::thread::yield_now();
            }
            observed_tx.send(()).expect("report observed stop flag");
        })
        .expect("spawn stoppable worker");

        worker.abort_with_timeout(TEST_TIMEOUT);

        observed_rx
            .recv_timeout(TEST_TIMEOUT)
            .expect("worker should observe stop flag");
    }

    #[test]
    fn interruptible_abort_invokes_wake_once() {
        let wake_count = Arc::new(AtomicUsize::new(0));
        let wake_count_for_callback = Arc::clone(&wake_count);
        let (wake_tx, wake_rx) = mpsc::channel();
        let (stopped_tx, stopped_rx) = mpsc::channel();
        let worker = WorkerThread::spawn_interruptible(
            "rsloop-test-interruptible",
            move || {
                wake_count_for_callback.fetch_add(1, Ordering::AcqRel);
                wake_tx.send(()).expect("wake blocked worker");
            },
            move |stop| {
                wake_rx.recv().expect("receive wake signal");
                stopped_tx
                    .send(stop.load(Ordering::Acquire))
                    .expect("report stop ordering");
            },
        )
        .expect("spawn interruptible worker");

        worker.abort_with_timeout(TEST_TIMEOUT);

        assert!(
            stopped_rx
                .recv_timeout(TEST_TIMEOUT)
                .expect("worker should report stop ordering")
        );
        assert_eq!(wake_count.load(Ordering::Acquire), 1);
    }

    #[test]
    fn panicking_worker_is_joined_without_panicking_the_caller() {
        let (started_tx, started_rx) = mpsc::channel();
        let worker = WorkerThread::spawn("rsloop-test-panic", move |_| {
            started_tx.send(()).expect("report worker start");
            panic!("intentional worker panic");
        })
        .expect("spawn panicking worker");
        started_rx
            .recv_timeout(TEST_TIMEOUT)
            .expect("panicking worker should start");

        worker.abort_with_timeout(TEST_TIMEOUT);
    }

    #[test]
    fn worker_can_abort_its_own_handle_without_self_joining() {
        let (worker_tx, worker_rx) = mpsc::channel::<WorkerThread>();
        let (completed_tx, completed_rx) = mpsc::channel();
        let worker = WorkerThread::spawn("rsloop-test-self-abort", move |_| {
            let worker = worker_rx.recv().expect("receive own worker handle");
            worker.abort();
            completed_tx.send(()).expect("report self-abort completion");
        })
        .expect("spawn self-aborting worker");

        worker_tx.send(worker).expect("send worker its own handle");
        completed_rx
            .recv_timeout(TEST_TIMEOUT)
            .expect("self-abort should not deadlock");
    }

    #[test]
    fn abort_detaches_a_worker_that_exceeds_the_join_timeout() {
        let (release_tx, release_rx) = mpsc::channel();
        let (stopped_tx, stopped_rx) = mpsc::channel();
        let worker = WorkerThread::spawn("rsloop-test-detach", move |stop| {
            release_rx.recv().expect("receive worker release");
            stopped_tx
                .send(stop.load(Ordering::Acquire))
                .expect("report detached worker exit");
        })
        .expect("spawn blocked worker");

        let started = Instant::now();
        worker.abort_with_timeout(Duration::from_millis(10));
        assert!(started.elapsed() < TEST_TIMEOUT);

        release_tx.send(()).expect("release detached worker");
        assert!(
            stopped_rx
                .recv_timeout(TEST_TIMEOUT)
                .expect("detached worker should exit")
        );
    }
}
