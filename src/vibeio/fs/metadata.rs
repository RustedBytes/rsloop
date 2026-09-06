use std::io;
use std::time::SystemTime;

#[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
use std::time::{Duration, UNIX_EPOCH};

/// File metadata information.
///
/// This type mirrors a subset of [`std::fs::Metadata`]. On supported Linux
/// targets completion-backed queries use `statx` (via io_uring). Fallback
/// queries use standard-library metadata, offloaded when filesystem offload is
/// enabled or queried synchronously otherwise.
///
/// # Platform-specific behavior
///
/// - On supported Linux targets, completion-backed queries use `statx` directly.
/// - Other queries use the standard library's `std::fs::Metadata`.
///
/// # Examples
///
/// See "Filesystem offload" in `tools/vibeio-check/EXAMPLES.md` for executable
/// file-size, directory and symlink metadata checks using isolated paths.
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

    if nanos >= 1_000_000_000 {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid nanosecond value in statx timestamp",
        ));
    }

    if secs >= 0 {
        UNIX_EPOCH
            .checked_add(Duration::new(secs as u64, nanos))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid statx timestamp"))
    } else {
        // statx uses a signed seconds field plus a positive fractional
        // component. For example, (-1, 500ms) is half a second before the
        // epoch, not one and a half seconds before it.
        let whole_seconds = UNIX_EPOCH
            .checked_sub(Duration::from_secs(secs.unsigned_abs()))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid statx timestamp"))?;
        whole_seconds
            .checked_add(Duration::from_nanos(nanos as u64))
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "invalid statx timestamp"))
    }
}

#[cfg(all(test, target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
mod tests {
    use super::*;

    #[test]
    fn statx_timestamp_handles_fractional_time_before_epoch() {
        let mut ts: libc::statx_timestamp = unsafe { std::mem::zeroed() };
        ts.tv_sec = -1;
        ts.tv_nsec = 500_000_000;
        let converted = statx_timestamp_to_system_time(&ts).expect("valid timestamp");
        assert_eq!(
            UNIX_EPOCH.duration_since(converted).expect("before epoch"),
            Duration::from_millis(500)
        );
    }
}

/// A structure representing a type of file with accessors for each file type.
///
/// # Examples
///
/// See "Filesystem offload" in `tools/vibeio-check/EXAMPLES.md` for executable
/// file-type checks, including the difference between metadata and symlink_metadata.
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
    /// The "Filesystem offload" harness example creates and checks a directory.
    #[inline]
    pub fn is_dir(&self) -> bool {
        self.is_dir
    }

    /// Test whether this file type represents a regular file.
    ///
    /// # Examples
    ///
    /// The "Filesystem offload" harness example checks a regular file's type.
    #[inline]
    pub fn is_file(&self) -> bool {
        self.is_file
    }

    /// Test whether this file type represents a symbolic link.
    ///
    /// # Examples
    ///
    /// Use [`super::symlink_metadata`] to inspect the link itself;
    /// [`super::metadata`] follows it and reports the target's file type.
    /// See the Unix symlink checks in "Filesystem offload" in
    /// `tools/vibeio-check/EXAMPLES.md`.
    #[inline]
    pub fn is_symlink(&self) -> bool {
        self.is_symlink
    }
}
