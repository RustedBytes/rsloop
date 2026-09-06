use super::BlockingThreadPool;

/// A default implementation of `BlockingThreadPool` using `rusty_pool` crate.
pub struct DefaultBlockingThreadPool {
    inner: rusty_pool::ThreadPool,
}

impl DefaultBlockingThreadPool {
    /// Creates a new `DefaultBlockingThreadPool` with the default maximum number of threads.
    #[inline]
    pub fn new() -> Self {
        Self::with_max_threads(512)
    }

    /// Creates a new `DefaultBlockingThreadPool` with the specified maximum number of threads.
    ///
    /// # Panics
    ///
    /// Panics if the maximum is zero or exceeds the pool's supported size.
    #[inline]
    pub fn with_max_threads(num_threads: usize) -> Self {
        Self {
            // rusty_pool's default core size is the CPU count. Cap it too,
            // otherwise a valid small maximum panics on a multicore machine.
            inner: rusty_pool::Builder::new()
                .core_size(
                    std::thread::available_parallelism()
                        .map(usize::from)
                        .unwrap_or(1)
                        .min(num_threads),
                )
                .max_size(num_threads)
                .build(),
        }
    }
}

impl BlockingThreadPool for DefaultBlockingThreadPool {
    #[inline]
    fn spawn(&self, task: Box<dyn FnOnce() + Send + 'static>) {
        self.inner.execute(move || {
            task();
        });
    }
}

impl Default for DefaultBlockingThreadPool {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn single_worker_pool_executes_tasks_on_multicore_hosts() {
        let pool = DefaultBlockingThreadPool::with_max_threads(1);
        let (send, receive) = std::sync::mpsc::channel();
        pool.spawn(Box::new(move || send.send(42).unwrap()));
        assert_eq!(
            receive
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap(),
            42
        );
    }
}
