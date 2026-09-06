#[cfg(feature = "blocking-default")]
use crate::vibeio::blocking::DefaultBlockingThreadPool;
use crate::vibeio::{blocking::BlockingThreadPool, driver::AnyDriver};

#[cfg(target_os = "linux")]
fn ensure_rsloop_platform() -> Result<(), std::io::Error> {
    // Linux vendors frequently backport io_uring features, while containers
    // can block its syscalls on otherwise supported kernels. Let driver
    // initialization probe the actual capabilities and fall back as needed.
    Ok(())
}

#[cfg(target_os = "macos")]
fn ensure_rsloop_platform() -> Result<(), std::io::Error> {
    use std::ffi::CStr;

    let name = c"kern.osproductversion";
    let mut buffer = [0_i8; 64];
    let mut length = buffer.len();
    if unsafe {
        libc::sysctlbyname(
            name.as_ptr(),
            buffer.as_mut_ptr().cast(),
            &mut length,
            std::ptr::null_mut(),
            0,
        )
    } != 0
    {
        return Err(std::io::Error::last_os_error());
    }
    let release = unsafe { CStr::from_ptr(buffer.as_ptr()) }.to_string_lossy();
    let major = release
        .split('.')
        .next()
        .and_then(|part| part.parse::<u32>().ok())
        .unwrap_or(0);
    if major < 13 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            format!("rsloop requires macOS 13 or newer; detected {release}"),
        ));
    }
    Ok(())
}

#[cfg(windows)]
fn ensure_rsloop_platform() -> Result<(), std::io::Error> {
    #[allow(non_snake_case)]
    #[repr(C)]
    struct OsVersionInfo {
        dwOSVersionInfoSize: u32,
        dwMajorVersion: u32,
        dwMinorVersion: u32,
        dwBuildNumber: u32,
        dwPlatformId: u32,
        szCSDVersion: [u16; 128],
    }

    #[link(name = "ntdll")]
    unsafe extern "system" {
        fn RtlGetVersion(info: *mut OsVersionInfo) -> i32;
    }

    let mut info: OsVersionInfo = unsafe { std::mem::zeroed() };
    info.dwOSVersionInfoSize = std::mem::size_of::<OsVersionInfo>() as u32;
    let status = unsafe { RtlGetVersion(&mut info) };
    if status < 0 {
        return Err(std::io::Error::other(format!(
            "RtlGetVersion failed with NTSTATUS {status:#x}"
        )));
    }
    ensure_supported_windows_version(info.dwMajorVersion, info.dwMinorVersion, info.dwBuildNumber)
}

#[cfg(any(windows, test))]
fn ensure_supported_windows_version(
    major: u32,
    minor: u32,
    build: u32,
) -> Result<(), std::io::Error> {
    if major < 10 {
        return Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            format!("rsloop requires Windows 10 or newer; detected {major}.{minor}.{build}"),
        ));
    }
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn ensure_rsloop_platform() -> Result<(), std::io::Error> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "the rsloop runtime profile supports Linux, macOS, and Windows only",
    ))
}

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
/// ```ignore
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
    rsloop_profile: bool,
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
            rsloop_profile: false,
        }
    }

    /// Selects the scheduler profile used by rsloop.
    ///
    /// The profile keeps bounded task batches and polls timers during long
    /// batches so Python callbacks, kernel completions, and deadlines cannot
    /// starve one another. It is intentionally an explicit opt-in because the
    /// vendored crate is also built by its own tests and examples.
    #[inline]
    pub fn rsloop_profile(mut self) -> Self {
        self.rsloop_profile = true;
        self
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
    pub fn build(self) -> Result<crate::vibeio::executor::Runtime, std::io::Error> {
        if self.rsloop_profile {
            ensure_rsloop_platform()?;
        }
        let driver = if let Some(driver_kind) = self.driver_kind {
            driver_kind.into_driver()?
        } else {
            AnyDriver::new_best()?
        };
        Ok(crate::vibeio::executor::Runtime::with_options(
            driver,
            self.enable_timer,
            self.blocking_pool,
            self.enable_fs_offload,
            self.rsloop_profile,
        ))
    }
}

impl Default for RuntimeBuilder {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::ensure_supported_windows_version;

    #[test]
    fn windows_10_releases_are_supported() {
        assert!(ensure_supported_windows_version(10, 0, 10_240).is_ok());
        assert!(ensure_supported_windows_version(10, 0, 19_045).is_ok());
    }

    #[test]
    fn windows_versions_before_10_are_rejected() {
        let error = ensure_supported_windows_version(6, 3, 9_600)
            .expect_err("Windows 8.1 must remain unsupported");

        assert_eq!(error.kind(), std::io::ErrorKind::Unsupported);
        assert_eq!(
            error.to_string(),
            "rsloop requires Windows 10 or newer; detected 6.3.9600"
        );
    }
}
