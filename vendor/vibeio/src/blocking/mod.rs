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
