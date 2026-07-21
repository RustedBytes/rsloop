use std::io;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// File metadata information.
///
/// This type mirrors a subset of [`std::fs::Metadata`]. On supported Linux
/// targets we back it by `statx` (via io_uring), otherwise we delegate to
/// `std::fs::metadata` on a blocking thread.
///
/// # Platform-specific behavior
///
/// - On Linux with io_uring support and glibc/musl v1.2.3+, this uses the `statx` syscall directly
///   for better async performance.
/// - On other platforms, this uses the standard library's `std::fs::Metadata`.
///
/// # Examples
///
/// ```ignore
/// use vibeio::fs;
///
/// let metadata = fs::metadata("hello.txt").await?;
/// println!("File size: {} bytes", metadata.len());
/// ```
#[derive(Clone, Debug)]
pub struct Metadata {
    inner: MetadataInner,
}

#[derive(Clone, Debug)]
enum MetadataInner {
    #[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
    Statx(libc::statx),
    Std(std::fs::Metadata),
}

impl Metadata {
    /// Creates a new `Metadata` from a standard library `std::fs::Metadata`.
    #[inline]
    pub(crate) fn from_std(md: std::fs::Metadata) -> Self {
        Self {
            inner: MetadataInner::Std(md),
        }
    }

    /// Creates a new `Metadata` from a `libc::statx` structure.
    #[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
    #[inline]
    pub(crate) fn from_statx(st: libc::statx) -> Self {
        Self {
            inner: MetadataInner::Statx(st),
        }
    }

    /// Returns the size of the file in bytes.
    #[allow(clippy::len_without_is_empty)]
    #[inline]
    pub fn len(&self) -> u64 {
        match &self.inner {
            #[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
            MetadataInner::Statx(st) => st.stx_size,
            MetadataInner::Std(md) => md.len(),
        }
    }

    /// Returns the file permissions.
    #[inline]
    pub fn permissions(&self) -> std::fs::Permissions {
        match &self.inner {
            #[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
            MetadataInner::Statx(st) => {
                use std::os::unix::fs::PermissionsExt;
                std::fs::Permissions::from_mode(st.stx_mode as u32)
            }
            MetadataInner::Std(md) => md.permissions(),
        }
    }

    /// Returns the file type.
    #[inline]
    pub fn file_type(&self) -> FileType {
        FileType {
            is_dir: self.is_dir(),
            is_file: self.is_file(),
            is_symlink: self.is_symlink(),
        }
    }

    /// Returns `true` if this metadata is for a directory.
    #[inline]
    pub fn is_dir(&self) -> bool {
        match &self.inner {
            #[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
            MetadataInner::Statx(st) => (st.stx_mode as u32 & libc::S_IFMT) == libc::S_IFDIR,
            MetadataInner::Std(md) => md.is_dir(),
        }
    }

    /// Returns `true` if this metadata is for a regular file.
    #[inline]
    pub fn is_file(&self) -> bool {
        match &self.inner {
            #[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
            MetadataInner::Statx(st) => (st.stx_mode as u32 & libc::S_IFMT) == libc::S_IFREG,
            MetadataInner::Std(md) => md.is_file(),
        }
    }

    /// Returns `true` if this metadata is for a symbolic link.
    #[inline]
    pub fn is_symlink(&self) -> bool {
        match &self.inner {
            #[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
            MetadataInner::Statx(st) => (st.stx_mode as u32 & libc::S_IFMT) == libc::S_IFLNK,
            MetadataInner::Std(md) => md.file_type().is_symlink(),
        }
    }

    /// Returns the time the file was last accessed.
    ///
    /// # Errors
    ///
    /// This function will return an error in the following situations:
    /// - The timestamp is invalid
    #[inline]
    pub fn accessed(&self) -> io::Result<SystemTime> {
        match &self.inner {
            #[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
            MetadataInner::Statx(st) => statx_timestamp_to_system_time(&st.stx_atime),
            MetadataInner::Std(md) => md.accessed(),
        }
    }

    /// Returns the time the file was created.
    ///
    /// # Errors
    ///
    /// This function will return an error in the following situations:
    /// - The timestamp is invalid
    #[inline]
    pub fn created(&self) -> io::Result<SystemTime> {
        match &self.inner {
            #[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
            MetadataInner::Statx(st) => statx_timestamp_to_system_time(&st.stx_btime),
            MetadataInner::Std(md) => md.created(),
        }
    }

    /// Returns the time the file was last modified.
    ///
    /// # Errors
    ///
    /// This function will return an error in the following situations:
    /// - The timestamp is invalid
    #[inline]
    pub fn modified(&self) -> io::Result<SystemTime> {
        match &self.inner {
            #[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
            MetadataInner::Statx(st) => statx_timestamp_to_system_time(&st.stx_mtime),
            MetadataInner::Std(md) => md.modified(),
        }
    }
}

/// Converts a `libc::statx_timestamp` to a `SystemTime`.
#[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
#[inline]
fn statx_timestamp_to_system_time(ts: &libc::statx_timestamp) -> io::Result<SystemTime> {
    let secs = ts.tv_sec;
    let nanos = ts.tv_nsec;

    if secs >= 0 {
        Ok(UNIX_EPOCH + Duration::new(secs as u64, nanos))
    } else {
        UNIX_EPOCH
            .checked_sub(Duration::new((-secs) as u64, nanos))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid statx timestamp"))
    }
}

/// A structure representing a type of file with accessors for each file type.
///
/// # Examples
///
/// ```ignore
/// use vibeio::fs;
///
/// let metadata = fs::metadata("hello.txt").await?;
/// let file_type = metadata.file_type();
///
/// if file_type.is_file() {
///     println!("It's a file!");
/// } else if file_type.is_dir() {
///     println!("It's a directory!");
/// }
/// ```
#[derive(Copy, Clone, PartialEq, Eq, Hash, Debug)]
pub struct FileType {
    is_dir: bool,
    is_file: bool,
    is_symlink: bool,
}

impl FileType {
    /// Test whether this file type represents a directory.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// use vibeio::fs;
    ///
    /// let metadata = fs::metadata("my_dir").await?;
    /// if metadata.file_type().is_dir() {
    ///     println!("It's a directory!");
    /// }
    /// ```
    #[inline]
    pub fn is_dir(&self) -> bool {
        self.is_dir
    }

    /// Test whether this file type represents a regular file.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// use vibeio::fs;
    ///
    /// let metadata = fs::metadata("hello.txt").await?;
    /// if metadata.file_type().is_file() {
    ///     println!("It's a file!");
    /// }
    /// ```
    #[inline]
    pub fn is_file(&self) -> bool {
        self.is_file
    }

    /// Test whether this file type represents a symbolic link.
    ///
    /// # Examples
    ///
    /// ```ignore
    /// use vibeio::fs;
    ///
    /// let metadata = fs::metadata("link_to_file").await?;
    /// if metadata.file_type().is_symlink() {
    ///     println!("It's a symlink!");
    /// }
    /// ```
    #[inline]
    pub fn is_symlink(&self) -> bool {
        self.is_symlink
    }
}
