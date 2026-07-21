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
    #[inline]
    pub fn with_max_threads(num_threads: usize) -> Self {
        Self {
            inner: rusty_pool::Builder::new().max_size(num_threads).build(),
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
