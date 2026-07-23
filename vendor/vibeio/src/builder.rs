#[cfg(feature = "blocking-default")]
use crate::blocking::DefaultBlockingThreadPool;
use crate::{blocking::BlockingThreadPool, driver::AnyDriver};

/// I/O driver selection for the async runtime.
///
/// This enum allows choosing which I/O driver to use when building the runtime.
#[derive(Clone)]
pub enum DriverKind {
    /// Uses the Mio driver for I/O operations (Unix only).
    #[cfg(unix)]
    Mio,
    /// Uses the IOCP driver for completion-based I/O operations (Windows only).
    #[cfg(windows)]
    Iocp,
    /// Uses the mock driver for testing purposes.
    Mock,
    /// Uses the io_uring driver (Linux only).
    #[cfg(target_os = "linux")]
    IoUring,
    /// Uses a custom io_uring driver (Linux only).
    #[cfg(target_os = "linux")]
    IoUringCustom(io_uring::Builder),
}

impl DriverKind {
    /// Creates a new runtime I/O driver from this kind.
    #[inline]
    pub(crate) fn into_driver(self) -> Result<AnyDriver, std::io::Error> {
        match self {
            #[cfg(unix)]
            DriverKind::Mio => AnyDriver::new_mio(),
            #[cfg(windows)]
            DriverKind::Iocp => AnyDriver::new_iocp(),
            DriverKind::Mock => Ok(AnyDriver::new_mock()),
            #[cfg(target_os = "linux")]
            DriverKind::IoUring => AnyDriver::new_uring(),
            #[cfg(target_os = "linux")]
            DriverKind::IoUringCustom(builder) => AnyDriver::new_uring_custom(builder),
        }
    }
}

/// Builder for configuring and creating an async runtime.
///
/// Provides a convenient way to configure the runtime's I/O driver
/// before building it.
///
/// # Examples
///
/// ```
/// use vibeio::RuntimeBuilder;
///
/// let runtime = RuntimeBuilder::new()
///     .build();
/// ```
pub struct RuntimeBuilder {
    driver_kind: Option<DriverKind>,
    enable_timer: bool,
    enable_fs_offload: bool,
    blocking_pool: Option<Box<dyn BlockingThreadPool>>,
}

impl RuntimeBuilder {
    /// Creates a new runtime builder with default configuration.
    ///
    /// By default, the builder will select the best available driver for the platform.
    pub fn new() -> Self {
        Self {
            driver_kind: None,
            enable_timer: false,
            enable_fs_offload: false,
            blocking_pool: None,
        }
    }

    /// Sets the I/O driver for the runtime.
    pub fn driver(mut self, driver_kind: DriverKind) -> Self {
        self.driver_kind = Some(driver_kind);
        self
    }

    /// Enables or disables the timer for the runtime.
    ///
    /// By default, the timer is disabled.
    pub fn enable_timer(mut self, enable: bool) -> Self {
        self.enable_timer = enable;
        self
    }

    /// Enables or disables the offload of file I/O to blocking threads for the runtime.
    ///
    /// By default, the fs offload is disabled.
    pub fn enable_fs_offload(mut self, enable: bool) -> Self {
        self.enable_fs_offload = enable;
        self
    }

    /// Sets the blocking thread pool for the runtime.
    pub fn blocking_pool(mut self, blocking_pool: Box<dyn BlockingThreadPool>) -> Self {
        self.blocking_pool = Some(blocking_pool);
        self
    }

    /// Sets the default blocking thread pool for the runtime with specified maximum number of threads.
    #[cfg(feature = "blocking-default")]
    pub fn default_blocking_pool(mut self, max_threads: usize) -> Self {
        self.blocking_pool = Some(Box::new(DefaultBlockingThreadPool::with_max_threads(
            max_threads,
        )));
        self
    }

    /// Builds the async runtime with the configured settings.
    ///
    /// If no driver was explicitly set, selects the best available driver for the platform.
    pub fn build(self) -> Result<crate::executor::Runtime, std::io::Error> {
        let driver = if let Some(driver_kind) = self.driver_kind {
            driver_kind.into_driver()?
        } else {
            AnyDriver::new_best()?
        };
        Ok(crate::executor::Runtime::with_options(
            driver,
            self.enable_timer,
            self.blocking_pool,
            self.enable_fs_offload,
        ))
    }
}

impl Default for RuntimeBuilder {
    fn default() -> Self {
        Self::new()
    }
}
