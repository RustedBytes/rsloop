use std::future::poll_fn;
use std::io::{self, ErrorKind};
use std::path::Path;

use crate::executor::current_driver;
use crate::op::Op;

#[cfg(target_os = "linux")]
use crate::op::OpenOp;

#[cfg(unix)]
use std::ffi::CString;
#[cfg(unix)]
use std::os::fd::FromRawFd;
#[cfg(unix)]
use std::os::unix::ffi::OsStrExt;

#[cfg(windows)]
use crate::fd_inner::RawOsHandle;

use crate::fs::file::File;

/// Options and flags for opening files.
///
/// This struct provides a builder-style interface for configuring how a file
/// should be opened, similar to [`std::fs::OpenOptions`]. It supports both
/// io_uring completion-based opening on Linux and blocking thread pool fallback
/// for other platforms.
///
/// # Examples
///
/// ```ignore
/// use vibeio::fs::OpenOptions;
///
/// // Open a file for reading
/// let file = OpenOptions::new()
///     .read(true)
///     .open("hello.txt")
///     .await?;
///
/// // Create a new file for writing (truncate if exists)
/// let file = OpenOptions::new()
///     .write(true)
///     .create(true)
///     .truncate(true)
///     .open("output.txt")
///     .await?;
///
/// ```
#[derive(Clone, Debug)]
pub struct OpenOptions {
    read: bool,
    write: bool,
    append: bool,
    truncate: bool,
    create: bool,
    create_new: bool,
}

impl OpenOptions {
    /// Creates a new `OpenOptions` with default values.
    ///
    /// By default, all options are set to `false`.
    #[inline]
    pub fn new() -> Self {
        Self {
            read: false,
            write: false,
            append: false,
            truncate: false,
            create: false,
            create_new: false,
        }
    }

    /// Sets whether the file should be opened for reading.
    #[inline]
    pub fn read(&mut self, read: bool) -> &mut Self {
        self.read = read;
        self
    }

    /// Sets whether the file should be opened for writing.
    #[inline]
    pub fn write(&mut self, write: bool) -> &mut Self {
        self.write = write;
        self
    }

    /// Sets whether the file should be opened in append mode.
    #[inline]
    pub fn append(&mut self, append: bool) -> &mut Self {
        self.append = append;
        self
    }

    /// Sets whether the file should be truncated if it already exists.
    #[inline]
    pub fn truncate(&mut self, truncate: bool) -> &mut Self {
        self.truncate = truncate;
        self
    }

    /// Sets whether the file should be created if it does not exist.
    #[inline]
    pub fn create(&mut self, create: bool) -> &mut Self {
        self.create = create;
        self
    }

    /// Sets whether the file should be created exclusively (fails if it already exists).
    #[inline]
    pub fn create_new(&mut self, create_new: bool) -> &mut Self {
        self.create_new = create_new;
        self
    }

    /// Validates the open options.
    ///
    /// This is an internal method used to ensure the options are valid.
    #[inline]
    fn validate(&self) -> io::Result<()> {
        let writing = self.write || self.append;

        if !self.read && !writing {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "must enable read, write, or append access",
            ));
        }
        if (self.truncate || self.create || self.create_new) && !writing {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "truncate/create options require write or append access",
            ));
        }

        Ok(())
    }

    /// Returns the initial cursor position for the file.
    ///
    /// If append mode is enabled, the cursor is set to the end of the file.
    #[inline]
    fn initial_cursor_for_append(&self, file: &std::fs::File) -> io::Result<u64> {
        if self.append {
            Ok(file.metadata()?.len())
        } else {
            Ok(0)
        }
    }

    /// Opens a file with the configured options.
    ///
    /// This is the async version of [`std::fs::OpenOptions::open`].
    ///
    /// # Platform-specific behavior
    ///
    /// - On Linux with io_uring support, this uses the `openat` syscall directly.
    /// - On other platforms, this either offloads to a blocking thread pool or falls back
    ///   to [`std::fs::OpenOptions::open`].
    ///
    /// # Errors
    ///
    /// This function will return an error in the following situations:
    /// - The file cannot be opened with the specified options
    /// - The process lacks permissions to open the file
    ///
    /// # Examples
    ///
    /// ```ignore
    /// use vibeio::fs::OpenOptions;
    ///
    /// let file = OpenOptions::new()
    ///     .read(true)
    ///     .open("hello.txt")
    ///     .await?;
    /// ```
    #[inline]
    pub async fn open(&self, path: impl AsRef<Path>) -> io::Result<File> {
        self.validate()?;
        let path = path.as_ref();

        let std_file = if let Some(driver) = current_driver() {
            #[cfg(target_os = "linux")]
            {
                if driver.supports_completion() {
                    let mut op = self.build_open_op(path)?;
                    let raw = poll_fn(move |cx| op.poll(cx, &driver)).await?;
                    unsafe { std::fs::File::from_raw_fd(raw) }
                } else if crate::offload_fs() {
                    self.open_in_blocking_pool(path).await?
                } else {
                    self.open_blocking(path)?
                }
            }

            #[cfg(not(target_os = "linux"))]
            {
                let _ = driver;
                if crate::offload_fs() {
                    self.open_in_blocking_pool(path).await?
                } else {
                    self.open_blocking(path)?
                }
            }
        } else {
            self.open_blocking(path)?
        };

        let cursor = self.initial_cursor_for_append(&std_file)?;
        File::from_std_with_cursor(std_file, cursor)
    }

    /// Opens a file in the blocking thread pool.
    ///
    /// This is an internal method used when io_uring is not available.
    #[inline]
    async fn open_in_blocking_pool(&self, path: &Path) -> io::Result<std::fs::File> {
        let path = path.to_path_buf();
        let read = self.read;
        let write = self.write;
        let append = self.append;
        let truncate = self.truncate;
        let create = self.create;
        let create_new = self.create_new;

        crate::spawn_blocking(move || {
            #[cfg(windows)]
            use std::os::windows::fs::OpenOptionsExt;
            #[cfg(windows)]
            use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OVERLAPPED;

            let mut options = std::fs::OpenOptions::new();
            options
                .read(read)
                .write(write)
                .append(append)
                .truncate(truncate)
                .create(create)
                .create_new(create_new);
            #[cfg(windows)]
            options.attributes(FILE_FLAG_OVERLAPPED);
            options.open(path)
        })
        .await
        .map_err(|_| crate::fs::file::blocking_pool_io_error())?
    }

    /// Opens a file synchronously.
    ///
    /// This is an internal method used when io_uring is not available.
    #[inline]
    fn open_blocking(&self, path: &Path) -> io::Result<std::fs::File> {
        #[cfg(windows)]
        use std::os::windows::fs::OpenOptionsExt;
        #[cfg(windows)]
        use windows_sys::Win32::Storage::FileSystem::FILE_FLAG_OVERLAPPED;

        let mut options = std::fs::OpenOptions::new();
        options
            .read(self.read)
            .write(self.write)
            .append(self.append)
            .truncate(self.truncate)
            .create(self.create)
            .create_new(self.create_new);
        #[cfg(windows)]
        options.attributes(FILE_FLAG_OVERLAPPED);
        options.open(path)
    }

    /// Builds an `OpenOp` for use with io_uring.
    ///
    /// This is an internal method used on Linux with io_uring support.
    #[cfg(target_os = "linux")]
    #[inline]
    fn build_open_op(&self, path: &Path) -> io::Result<OpenOp> {
        let writing = self.write || self.append;
        let mut flags = match (self.read, writing) {
            (true, false) => libc::O_RDONLY,
            (false, true) => libc::O_WRONLY,
            (true, true) => libc::O_RDWR,
            (false, false) => {
                return Err(io::Error::new(
                    ErrorKind::InvalidInput,
                    "must enable read, write, or append access",
                ))
            }
        };

        if self.append {
            flags |= libc::O_APPEND;
        }
        if self.truncate {
            flags |= libc::O_TRUNC;
        }
        if self.create_new {
            flags |= libc::O_CREAT | libc::O_EXCL;
        } else if self.create {
            flags |= libc::O_CREAT;
        }
        flags |= libc::O_CLOEXEC;

        let path_bytes = path.as_os_str().as_bytes();
        if path_bytes.contains(&0) {
            return Err(io::Error::new(
                ErrorKind::InvalidInput,
                "path contains interior NUL byte",
            ));
        }

        let path = CString::new(path_bytes).map_err(|_| {
            io::Error::new(ErrorKind::InvalidInput, "path contains interior NUL byte")
        })?;

        Ok(OpenOp::new(path, flags, 0o666))
    }
}

impl Default for OpenOptions {
    /// Returns the default `OpenOptions`.
    ///
    /// This is equivalent to `OpenOptions::new()`.
    #[inline]
    fn default() -> Self {
        Self::new()
    }
}
