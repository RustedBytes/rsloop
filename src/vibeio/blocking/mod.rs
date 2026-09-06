//! Support for spawning blocking tasks in an async runtime.
//!
//! This module provides a trait for pluggable blocking thread pools and a default implementation
//! using a thread pool with automatically adjusted thread count and a work-stealing queue.

#[cfg(feature = "blocking-default")]
mod default;

use std::fmt;

#[cfg(feature = "blocking-default")]
pub use default::*;

/// Error returned when a blocking task fails to spawn.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SpawnBlockingError;

impl fmt::Display for SpawnBlockingError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "failed to spawn blocking task")
    }
}

impl std::error::Error for SpawnBlockingError {}

/// Offload a borrowed operation while retaining ownership through worker unwind.
/// Cancellation drops the caller's share; a queued/running worker retains its
/// share until it stops using the buffer. Partial mutations are not rolled back.
#[cfg(any(feature = "fs", feature = "stdio", feature = "process"))]
pub(crate) async fn with_buffer<B, R>(
    buf: B,
    operation: impl FnOnce(&mut B) -> R + Send + 'static,
) -> (Result<R, SpawnBlockingError>, B)
where
    B: Send + 'static,
    R: Send + 'static,
{
    use std::sync::{Arc, Mutex};

    let storage = Arc::new(Mutex::new(Some(buf)));
    let worker = storage.clone();
    let result = crate::vibeio::spawn_blocking(move || {
        // Borrow, don't take: unwinding must leave the buffer recoverable.
        let mut guard = worker.lock().unwrap_or_else(|error| error.into_inner());
        operation(guard.as_mut().expect("worker owns the buffer slot"))
    })
    .await;
    // The result channel completes only after the worker releases its guard,
    // including during unwinding. Poison does not invalidate buffer ownership.
    let returned = storage
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .take()
        .expect("only the caller removes the buffer");
    (result, returned)
}

#[cfg(all(test, any(feature = "fs", feature = "stdio", feature = "process")))]
mod buffer_tests {
    use super::*;
    use std::cell::RefCell;
    use std::future::Future;
    use std::rc::Rc;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use std::task::{Context, Poll, Waker};

    #[derive(Default)]
    struct QueuedPool(RefCell<Option<Box<dyn FnOnce() + Send>>>);

    impl BlockingThreadPool for Rc<QueuedPool> {
        fn spawn(&self, task: Box<dyn FnOnce() + Send>) {
            assert!(self.0.borrow_mut().replace(task).is_none());
        }
    }

    #[test]
    fn cancelled_buffer_offload_retains_storage_until_worker_releases_it() {
        struct Buffer(Arc<AtomicUsize>);
        impl Drop for Buffer {
            fn drop(&mut self) {
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        for execute in [false, true] {
            let pool = Rc::new(QueuedPool::default());
            let runtime = crate::vibeio::RuntimeBuilder::new()
                .driver(crate::vibeio::DriverKind::Mock)
                .blocking_pool(Box::new(pool.clone()))
                .build()
                .unwrap();
            let drops = Arc::new(AtomicUsize::new(0));
            runtime.block_on(async move {
                let mut future = Box::pin(with_buffer(Buffer(drops.clone()), |buf| {
                    assert_eq!(buf.0.load(Ordering::SeqCst), 0);
                }));
                let mut cx = Context::from_waker(Waker::noop());
                assert!(matches!(future.as_mut().poll(&mut cx), Poll::Pending));
                drop(future);
                assert_eq!(drops.load(Ordering::SeqCst), 0);
                let task = pool.0.borrow_mut().take().unwrap();
                if execute {
                    std::thread::spawn(task).join().unwrap();
                } else {
                    drop(task);
                }
                assert_eq!(drops.load(Ordering::SeqCst), 1);
            });
        }
    }
}

/// A trait for pluggable blocking thread pools.
///
/// This trait allows users to provide their own implementation of a thread pool for executing
/// blocking tasks. The thread pool must be able to spawn tasks and shut down gracefully.
pub trait BlockingThreadPool: 'static {
    /// Spawns a blocking task onto the thread pool.
    ///
    /// The task will be executed on one of the threads in the pool, and its output will be
    /// returned back to `spawn_blocking`.
    fn spawn(&self, task: Box<dyn FnOnce() + Send + 'static>);
}

/// Spawns a blocking task onto a blocking thread pool.
///
/// This function is a convenience wrapper around a blocking thread pool.
#[inline]
pub(crate) async fn spawn_blocking<T, F>(
    pool: &dyn BlockingThreadPool,
    f: F,
) -> Result<T, SpawnBlockingError>
where
    T: Send + 'static,
    F: FnOnce() -> T + Send + 'static,
{
    let (tx, rx) = oneshot::async_channel::<T>();
    let task: Box<dyn FnOnce() + Send + 'static> = Box::new(move || {
        let _ = tx.send(f());
    });
    pool.spawn(task);

    rx.await.map_err(|_| SpawnBlockingError)
}
