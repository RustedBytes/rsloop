//! Adapter for blocking operations that cannot run on an async reactor thread.
//!
//! On Unix, [`spawn_bounded`] jobs (name resolution) run on the same pool as
//! [`run`], but at most `min(32, CPUs + 4)` at a time; the rest wait in
//! submission order, so a burst of lookups does not start a thread per lookup.

#[cfg(unix)]
use std::{
    collections::VecDeque,
    panic::{self, AssertUnwindSafe},
    sync::{LazyLock, Mutex, MutexGuard},
};

/// Run blocking work on the shared pool. Panic isolation retains the previous
/// dropped-worker error contract without spawning a thread for every call.
#[cfg_attr(feature = "profile", hotpath::measure(future = true))]
pub async fn run<T, F>(_name: impl Into<String>, task: F) -> Result<T, String>
where
    F: FnOnce() -> T + Send + 'static,
    T: Send + 'static,
{
    smol::unblock(move || std::panic::catch_unwind(std::panic::AssertUnwindSafe(task)))
        .await
        .map_err(|_| "blocking worker dropped".to_owned())
}

#[cfg(unix)]
type Job = Box<dyn FnOnce() + Send + 'static>;

/// The slots of [`spawn_bounded`].
#[cfg(unix)]
struct Bounded {
    /// `min(32, CPUs + 4)`, the size of asyncio's default executor (see
    /// [`usable_cpus`]).
    limit: usize,
    state: Mutex<BoundedState>,
}

#[cfg(unix)]
#[derive(Default)]
struct BoundedState {
    /// Jobs holding a slot.
    running: usize,
    /// Jobs waiting for a slot. A worker that finishes a bounded job takes
    /// the next of these itself.
    waiting: VecDeque<Job>,
}

#[cfg(unix)]
static BOUNDED: LazyLock<Bounded> = LazyLock::new(|| Bounded {
    limit: usable_cpus().saturating_add(4).min(32),
    state: Mutex::new(BoundedState::default()),
});

#[cfg(unix)]
impl Bounded {
    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "Bounded"))]
    fn lock(&self) -> MutexGuard<'_, BoundedState> {
        // Jobs run outside the lock, so a poisoned lock still has a consistent
        // state.
        self.state.lock().unwrap_or_else(|error| error.into_inner())
    }

    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "Bounded"))]
    fn submit(&'static self, job: Job) {
        let mut state = self.lock();
        if state.running >= self.limit {
            state.waiting.push_back(job);
            return;
        }
        state.running += 1;
        drop(state);
        smol::unblock(move || self.run_slot(job)).detach();
    }

    /// Runs `job`, then every job that waits for a slot when it finishes.
    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "Bounded"))]
    fn run_slot(&self, mut job: Job) {
        loop {
            // The panic hook has reported a panic; the slot still passes on.
            let _ = panic::catch_unwind(AssertUnwindSafe(job));
            let mut state = self.lock();
            match state.waiting.pop_front() {
                Some(next) => job = next,
                None => {
                    state.running -= 1;
                    return;
                }
            }
        }
    }
}

/// The CPUs this process may run on, as `os.process_cpu_count()` counts
/// them for asyncio's default executor: the affinity mask, not a cgroup CPU
/// quota. Lookups mostly wait on the network, so a container's `--cpus`
/// should not shrink how many run at once.
#[cfg(unix)]
#[cfg_attr(feature = "profile", hotpath::measure)]
fn usable_cpus() -> usize {
    #[cfg(target_os = "linux")]
    {
        // SAFETY: an all-zero cpu_set_t is an empty set, which
        // sched_getaffinity fills for the calling thread.
        let mut set: libc::cpu_set_t = unsafe { std::mem::zeroed() };
        // SAFETY: set is a valid cpu_set_t of the size passed.
        if unsafe { libc::sched_getaffinity(0, size_of::<libc::cpu_set_t>(), &raw mut set) } == 0 {
            // SAFETY: set was filled by a successful sched_getaffinity.
            let count = unsafe { libc::CPU_COUNT(&set) };
            if let Ok(count @ 1..) = usize::try_from(count) {
                return count;
            }
        }
    }
    std::thread::available_parallelism().map_or(1, std::num::NonZero::get)
}

/// Runs one blocking closure on the shared pool, without waiting for it, with
/// at most `min(32, CPUs + 4)` of these running at once: beyond that, the
/// closure waits in submission order for one of them to finish.
#[cfg(unix)]
#[cfg_attr(feature = "profile", hotpath::measure)]
pub fn spawn_bounded(task: impl FnOnce() + Send + 'static) {
    BOUNDED.submit(Box::new(task));
}

/// How many [`spawn_bounded`] jobs run at once in this process.
#[cfg(all(unix, test))]
pub(crate) fn bounded_limit() -> usize {
    BOUNDED.limit
}

#[cfg(all(unix, test))]
mod tests {
    use std::{
        pin::pin,
        sync::{Arc, atomic::Ordering, mpsc},
        task::{Context, Poll, Wake, Waker},
        thread,
        time::Duration,
    };

    use super::*;

    struct Unpark(thread::Thread);

    impl Wake for Unpark {
        fn wake(self: Arc<Self>) {
            self.0.unpark();
        }
    }

    fn block_on<T>(future: impl Future<Output = T>) -> T {
        let waker = Waker::from(Arc::new(Unpark(thread::current())));
        let mut context = Context::from_waker(&waker);
        let mut future = pin!(future);
        loop {
            if let Poll::Ready(value) = future.as_mut().poll(&mut context) {
                return value;
            }
            thread::park();
        }
    }

    #[test]
    fn bounded_jobs_run_at_most_the_limit_at_once_and_all_finish() {
        use std::sync::atomic::AtomicUsize;

        let limit = bounded_limit();
        let jobs = 4 * limit + 3;
        let running = Arc::new(AtomicUsize::new(0));
        let peak = Arc::new(AtomicUsize::new(0));
        let (done, finished) = mpsc::channel();
        for index in 0..jobs {
            let (running, peak, done) = (running.clone(), peak.clone(), done.clone());
            spawn_bounded(move || {
                let now = running.fetch_add(1, Ordering::SeqCst) + 1;
                peak.fetch_max(now, Ordering::SeqCst);
                thread::sleep(Duration::from_millis(5));
                running.fetch_sub(1, Ordering::SeqCst);
                let _ = done.send(index);
            });
        }
        let mut seen: Vec<_> = (0..jobs)
            .map(|_| finished.recv_timeout(Duration::from_secs(30)).unwrap())
            .collect();
        seen.sort_unstable();
        assert_eq!(seen, (0..jobs).collect::<Vec<_>>());
        // Other tests' bounded jobs may hold slots too, so the peak can stay
        // below the limit, never above it.
        let peak = peak.load(Ordering::SeqCst);
        assert!((1..=limit).contains(&peak), "{peak} of {limit}");
    }

    #[test]
    fn blocked_bounded_jobs_do_not_delay_other_jobs() {
        let limit = bounded_limit();
        let (release, blocked) = mpsc::channel::<()>();
        let blocked = Arc::new(Mutex::new(blocked));
        let (done, finished) = mpsc::channel();
        // Every bounded slot waits, and one more job queues behind them.
        for _ in 0..=limit {
            let (blocked, done) = (blocked.clone(), done.clone());
            spawn_bounded(move || {
                let _ = blocked
                    .lock()
                    .unwrap()
                    .recv_timeout(Duration::from_secs(30));
                let _ = done.send(());
            });
        }
        // A blocked job finishes only once released (or after its timeout,
        // which only a stall reaches), so `run` returning first shows it did
        // not wait for a bounded slot.
        assert_eq!(block_on(run("test", || 7)).unwrap(), 7);
        assert_eq!(finished.try_recv(), Err(mpsc::TryRecvError::Empty));
        for _ in 0..=limit {
            release.send(()).unwrap();
        }
        for _ in 0..=limit {
            finished.recv_timeout(Duration::from_secs(30)).unwrap();
        }
    }

    #[test]
    fn a_panicking_bounded_job_passes_its_slot_on() {
        let limit = bounded_limit();
        for _ in 0..2 * limit {
            spawn_bounded(|| panic!("bounded job panics"));
        }
        let (done, finished) = mpsc::channel();
        spawn_bounded(move || {
            let _ = done.send(9);
        });
        assert_eq!(finished.recv_timeout(Duration::from_secs(30)), Ok(9));
    }
}
