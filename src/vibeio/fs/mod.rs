//! A file system module for `vibeio`.
//!
//! This module provides async versions of common file system operations:
//! - File operations: [`File`] with async read/write methods
//! - Path operations: [`canonicalize`], [`hard_link`], [`rename`], [`remove_dir`], [`remove_file`]
//! - Directory operations: [`create_dir`], [`create_dir_all`], [`symlink_dir`], [`symlink_file`]
//! - File content helpers: [`read`], [`read_to_string`], [`write()`]
//! - Metadata: [`metadata`], [`symlink_metadata`] for file information
//!
//! Implementation notes:
//! - On Linux with io_uring support, some operations use native async syscalls (e.g. `statx`, `linkat`)
//!   via the async driver. When io_uring completion is available, operations complete directly.
//! - For platforms without native async support, operations either offload to a blocking thread pool
//!   (if file I/O offload is enabled) or fall back to synchronous std::fs calls.
//! - Outside a runtime, filesystem operations use synchronous fallbacks when
//!   polled. Offloading inside a runtime requires a configured blocking pool.
//!
//! # Examples
//!
//! See the executable "Filesystem offload" example in
//! `tools/vibeio-check/EXAMPLES.md`. It configures a pool explicitly and confines
//! writes and cleanup to a newly created temporary directory.

mod file;
mod metadata;
mod open_options;

#[cfg(target_os = "linux")]
use std::ffi::CString;
use std::path::PathBuf;

pub use file::*;
pub use metadata::*;
pub use open_options::*;

use crate::vibeio::io::IoBuf;
use crate::vibeio::io::{AsyncRead, AsyncWrite};
#[cfg(target_os = "linux")]
use crate::vibeio::op::HardLinkOp;
#[cfg(target_os = "linux")]
use crate::vibeio::op::MkDirOp;
#[cfg(target_os = "linux")]
use crate::vibeio::op::Op;
#[cfg(target_os = "linux")]
use crate::vibeio::op::RenameOp;
#[cfg(target_os = "linux")]
use crate::vibeio::op::SymlinkOp;
#[cfg(target_os = "linux")]
use crate::vibeio::op::UnlinkOp;

/// Creates a symbolic link to a directory on Windows.
///
/// Creates the link at `path`, pointing to `target`, using the standard library.
/// For cross-platform symlink creation, use [`symlink_dir`] instead.
///
/// # Platform-specific behavior
///
/// - This function is only available on Windows.
/// - It creates a symbolic link to a directory using the Windows API.
///
/// # Errors
///
/// This function will return an error in the following situations:
/// - `path` already exists
/// - The process lacks permissions to create the symlink
/// - The platform does not support symbolic links
#[cfg(windows)]
pub fn windows_symlink_dir(path: String, target: String) -> std::io::Result<()> {
    std::os::windows::fs::symlink_dir(target, path)
}

/// Creates a symbolic link to a file on Windows.
///
/// Creates the link at `path`, pointing to `target`, using the standard library.
/// For cross-platform symlink creation, use [`symlink_file`] instead.
///
/// # Platform-specific behavior
///
/// - This function is only available on Windows.
/// - It creates a symbolic link to a file using the Windows API.
///
/// # Errors
///
/// This function will return an error in the following situations:
/// - `path` already exists
/// - The process lacks permissions to create the symlink
/// - The platform does not support symbolic links
#[cfg(windows)]
pub fn windows_symlink_file(path: String, target: String) -> std::io::Result<()> {
    std::os::windows::fs::symlink_file(target, path)
}

/// Returns the canonical form of a path with all components normalized.
///
/// This is the async version of [`std::fs::canonicalize`].
///
/// # Platform-specific behavior
///
/// - On most platforms, this either offloads to a blocking thread pool or falls back
///   to [`std::fs::canonicalize`].
///
/// # Errors
///
/// This function will return an error in the following situations:
/// - `path` does not exist
/// - A component in the path is not a directory
/// - The process lacks permissions to access components of the path
pub async fn canonicalize<P: AsRef<std::path::Path>>(path: P) -> std::io::Result<PathBuf> {
    let path = path.as_ref().to_path_buf();
    if crate::vibeio::executor::offload_fs() {
        crate::vibeio::spawn_blocking(move || path.canonicalize())
            .await
            .map_err(|_| crate::vibeio::fs::file::blocking_pool_io_error())?
    } else {
        path.canonicalize()
    }
}

/// Reads the entire contents of a file into a vector of bytes.
///
/// This is the async version of [`std::fs::read`].
///
/// # Platform-specific behavior
///
/// - On most platforms, this either offloads to a blocking thread pool or falls back
///   to [`std::fs::read`].
///
/// # Errors
///
/// This function will return an error in the following situations:
/// - `path` does not exist
/// - The process lacks permissions to read the file
pub async fn read(path: impl AsRef<std::path::Path>) -> std::io::Result<Vec<u8>> {
    let mut file: File = OpenOptions::new().read(true).open(path).await?;
    let mut bytes = Vec::new();
    let mut buf = [0u8; 8192];

    loop {
        let (read, returned_buf) = file.read(buf).await;
        let read = read?;
        buf = returned_buf;

        if read == 0 {
            break;
        }

        let slice =
            unsafe { std::slice::from_raw_parts(buf.as_buf_ptr(), buf.buf_len().min(read)) };
        bytes.extend_from_slice(slice);
    }

    Ok(bytes)
}

/// Reads the entire contents of a file into a string.
///
/// This is the async version of [`std::fs::read_to_string`].
///
/// # Platform-specific behavior
///
/// - On most platforms, this either offloads to a blocking thread pool or falls back
///   to [`std::fs::read_to_string`].
///
/// # Errors
///
/// This function will return an error in the following situations:
/// - [`read`] fails
/// - The file contents are not valid UTF-8
pub async fn read_to_string(path: impl AsRef<std::path::Path>) -> std::io::Result<String> {
    let bytes = read(path).await?;
    String::from_utf8(bytes)
        .map_err(|err| std::io::Error::new(std::io::ErrorKind::InvalidData, err.utf8_error()))
}

/// Writes a byte slice to a file, creating it if necessary.
///
/// This is the async version of [`std::fs::write`].
///
/// # Platform-specific behavior
///
/// - On most platforms, this either offloads to a blocking thread pool or falls back
///   to [`std::fs::write`].
///
/// # Errors
///
/// This function will return an error in the following situations:
/// - The file cannot be opened for writing
/// - The write operation fails
pub async fn write(
    path: impl AsRef<std::path::Path>,
    contents: impl AsRef<[u8]>,
) -> std::io::Result<()> {
    let mut file: File = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
        .await?;

    // The newly truncated file starts at offset zero. Retain one owned buffer
    // across partial writes and interruption retries instead of recopying its
    // remaining suffix for every attempt.
    file.write_exact_at(contents.as_ref().to_vec(), 0).await.0?;
    file.flush().await
}

/// Creates a hard link at the destination path pointing to the source.
///
/// This is the async version of [`std::fs::hard_link`].
///
/// # Platform-specific behavior
///
/// - On Linux with io_uring support, this uses the `linkat` syscall directly.
/// - On other platforms, this either offloads to a blocking thread pool or falls back
///   to [`std::fs::hard_link`].
///
/// # Errors
///
/// This function will return an error in the following situations:
/// - `src` does not exist
/// - `dst` already exists
/// - The source and destination are on different filesystems
/// - The process lacks permissions
#[cfg(target_os = "linux")]
pub async fn hard_link(
    src: impl AsRef<std::path::Path>,
    dst: impl AsRef<std::path::Path>,
) -> std::io::Result<()> {
    let src = src.as_ref();
    let dst = dst.as_ref();

    let driver = crate::vibeio::executor::current_driver();
    if driver.as_ref().is_some_and(|d| d.supports_completion()) {
        let src_cstr = CString::new(src.as_os_str().as_encoded_bytes()).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("Invalid path: {}", e),
            )
        })?;
        let dst_cstr = CString::new(dst.as_os_str().as_encoded_bytes()).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("Invalid path: {}", e),
            )
        })?;

        let driver = driver.expect("invalid driver state");
        let mut op = HardLinkOp::new(driver.clone(), src_cstr, dst_cstr);
        std::future::poll_fn(|cx| op.poll_completion(cx, driver.as_ref())).await
    } else if crate::vibeio::executor::offload_fs() {
        let src = src.to_owned();
        let dst = dst.to_owned();
        crate::vibeio::spawn_blocking(move || std::fs::hard_link(src, dst))
            .await
            .map_err(|_| crate::vibeio::fs::file::blocking_pool_io_error())?
    } else {
        std::fs::hard_link(src, dst)
    }
}

/// Creates a hard link at the destination path pointing to the source.
///
/// This is the async version of [`std::fs::hard_link`].
///
/// # Platform-specific behavior
///
/// - On platforms other than Linux, this either offloads to a blocking thread pool
///   or falls back to [`std::fs::hard_link`].
///
/// # Errors
///
/// This function will return an error in the following situations:
/// - `src` does not exist
/// - `dst` already exists
/// - The source and destination are on different filesystems
/// - The process lacks permissions
#[cfg(not(target_os = "linux"))]
pub async fn hard_link(
    src: impl AsRef<std::path::Path>,
    dst: impl AsRef<std::path::Path>,
) -> std::io::Result<()> {
    if crate::vibeio::executor::offload_fs() {
        let src = src.as_ref().to_owned();
        let dst = dst.as_ref().to_owned();
        crate::vibeio::spawn_blocking(move || std::fs::hard_link(src, dst))
            .await
            .map_err(|_| crate::vibeio::fs::file::blocking_pool_io_error())?
    } else {
        std::fs::hard_link(src, dst)
    }
}

/// Creates a symbolic link to a directory.
///
/// This is the async version of [std::os::unix::fs::symlink](https://doc.rust-lang.org/std/os/unix/fs/fn.symlink.html) (on Unix) or
/// [`std::os::windows::fs::symlink_dir`] (on Windows).
///
/// # Platform-specific behavior
///
/// - On Linux with io_uring support, this uses the `symlinkat` syscall directly.
/// - On Windows, this uses [`std::os::windows::fs::symlink_dir`].
/// - On other Unix platforms, this either offloads to a blocking thread pool or falls back
///   to [std::os::unix::fs::symlink](https://doc.rust-lang.org/std/os/unix/fs/fn.symlink.html).
///
/// # Errors
///
/// This function will return an error in the following situations:
/// - `src` does not exist or is not a directory
/// - `dst` already exists
/// - The process lacks permissions to create the symlink
/// - The platform does not support symbolic links
#[cfg(windows)]
pub async fn symlink_dir(
    src: impl AsRef<std::path::Path>,
    dst: impl AsRef<std::path::Path>,
) -> std::io::Result<()> {
    if crate::vibeio::executor::offload_fs() {
        let src = src.as_ref().to_path_buf();
        let dst = dst.as_ref().to_path_buf();
        crate::vibeio::spawn_blocking(move || std::os::windows::fs::symlink_dir(src, dst))
            .await
            .map_err(|_| crate::vibeio::fs::file::blocking_pool_io_error())?
    } else {
        std::os::windows::fs::symlink_dir(src, dst)
    }
}

/// Creates a symbolic link to a directory.
///
/// This is the async version of [`std::os::unix::fs::symlink`].
///
/// # Platform-specific behavior
///
/// - On Linux with io_uring support, this uses the `symlinkat` syscall directly.
/// - On other Unix platforms, this either offloads to a blocking thread pool or falls back
///   to [`std::os::unix::fs::symlink`].
///
/// # Errors
///
/// This function will return an error in the following situations:
/// - `src` does not exist or is not a directory
/// - `dst` already exists
/// - The process lacks permissions to create the symlink
/// - The platform does not support symbolic links
#[cfg(target_os = "linux")]
pub async fn symlink_dir(
    src: impl AsRef<std::path::Path>,
    dst: impl AsRef<std::path::Path>,
) -> std::io::Result<()> {
    let src = src.as_ref();
    let dst = dst.as_ref();

    let driver = crate::vibeio::executor::current_driver();
    if driver.as_ref().is_some_and(|d| d.supports_completion()) {
        // On Linux with io_uring, use SymlinkOp
        let src_cstr = CString::new(src.as_os_str().as_encoded_bytes()).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("Invalid path: {}", e),
            )
        })?;
        let dst_cstr = CString::new(dst.as_os_str().as_encoded_bytes()).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("Invalid path: {}", e),
            )
        })?;
        let driver = driver.expect("invalid driver state");
        let mut op = SymlinkOp::new(driver.clone(), src_cstr, dst_cstr);
        std::future::poll_fn(|cx| op.poll_completion(cx, driver.as_ref())).await
    } else if crate::vibeio::executor::offload_fs() {
        let src = src.to_owned();
        let dst = dst.to_owned();
        crate::vibeio::spawn_blocking(move || std::os::unix::fs::symlink(src, dst))
            .await
            .map_err(|_| crate::vibeio::fs::file::blocking_pool_io_error())?
    } else {
        std::os::unix::fs::symlink(src, dst)
    }
}

/// Creates a symbolic link to a directory.
///
/// This is the async version of [`std::os::unix::fs::symlink`].
///
/// # Platform-specific behavior
///
/// - On other Unix platforms (not Linux or Windows), this either offloads to a
///   blocking thread pool or falls back to [`std::os::unix::fs::symlink`].
///
/// # Errors
///
/// This function will return an error in the following situations:
/// - `src` does not exist or is not a directory
/// - `dst` already exists
/// - The process lacks permissions to create the symlink
/// - The platform does not support symbolic links
#[cfg(not(any(windows, target_os = "linux")))]
pub async fn symlink_dir(
    src: impl AsRef<std::path::Path>,
    dst: impl AsRef<std::path::Path>,
) -> std::io::Result<()> {
    if crate::vibeio::executor::offload_fs() {
        let src = src.as_ref().to_owned();
        let dst = dst.as_ref().to_owned();
        crate::vibeio::spawn_blocking(move || std::os::unix::fs::symlink(src, dst))
            .await
            .map_err(|_| crate::vibeio::fs::file::blocking_pool_io_error())?
    } else {
        std::os::unix::fs::symlink(src, dst)
    }
}

/// Creates a symbolic link to a file.
///
/// This is the async version of [std::os::unix::fs::symlink](https://doc.rust-lang.org/std/os/unix/fs/fn.symlink.html) (on Unix) or
/// [`std::os::windows::fs::symlink_file`] (on Windows).
///
/// # Platform-specific behavior
///
/// - On Linux with io_uring support, this uses the `symlinkat` syscall directly.
/// - On Windows, this uses [`std::os::windows::fs::symlink_file`].
/// - On other Unix platforms, this either offloads to a blocking thread pool or falls back
///   to [std::os::unix::fs::symlink](https://doc.rust-lang.org/std/os/unix/fs/fn.symlink.html).
///
/// # Errors
///
/// This function will return an error in the following situations:
/// - `src` does not exist or is not a file
/// - `dst` already exists
/// - The process lacks permissions to create the symlink
/// - The platform does not support symbolic links
#[cfg(windows)]
pub async fn symlink_file(
    src: impl AsRef<std::path::Path>,
    dst: impl AsRef<std::path::Path>,
) -> std::io::Result<()> {
    if crate::vibeio::executor::offload_fs() {
        let src = src.as_ref().to_path_buf();
        let dst = dst.as_ref().to_path_buf();
        crate::vibeio::spawn_blocking(move || std::os::windows::fs::symlink_file(src, dst))
            .await
            .map_err(|_| crate::vibeio::fs::file::blocking_pool_io_error())?
    } else {
        std::os::windows::fs::symlink_file(src, dst)
    }
}

/// Creates a symbolic link to a file.
///
/// This is the async version of [`std::os::unix::fs::symlink`].
///
/// # Platform-specific behavior
///
/// - On Linux with io_uring support, this uses the `symlinkat` syscall directly.
/// - On other Unix platforms, this either offloads to a blocking thread pool or falls back
///   to [`std::os::unix::fs::symlink`].
///
/// # Errors
///
/// This function will return an error in the following situations:
/// - `src` does not exist or is not a file
/// - `dst` already exists
/// - The process lacks permissions to create the symlink
/// - The platform does not support symbolic links
#[cfg(target_os = "linux")]
pub async fn symlink_file(
    src: impl AsRef<std::path::Path>,
    dst: impl AsRef<std::path::Path>,
) -> std::io::Result<()> {
    let src = src.as_ref();
    let dst = dst.as_ref();

    let driver = crate::vibeio::executor::current_driver();
    if driver.as_ref().is_some_and(|d| d.supports_completion()) {
        // On Linux with io_uring, use SymlinkOp
        let src_cstr = CString::new(src.as_os_str().as_encoded_bytes()).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("Invalid path: {}", e),
            )
        })?;
        let dst_cstr = CString::new(dst.as_os_str().as_encoded_bytes()).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("Invalid path: {}", e),
            )
        })?;
        let driver = driver.expect("invalid driver state");
        let mut op = SymlinkOp::new(driver.clone(), src_cstr, dst_cstr);
        std::future::poll_fn(|cx| op.poll_completion(cx, driver.as_ref())).await
    } else if crate::vibeio::executor::offload_fs() {
        let src = src.to_owned();
        let dst = dst.to_owned();
        crate::vibeio::spawn_blocking(move || std::os::unix::fs::symlink(src, dst))
            .await
            .map_err(|_| crate::vibeio::fs::file::blocking_pool_io_error())?
    } else {
        std::os::unix::fs::symlink(src, dst)
    }
}

/// Creates a symbolic link to a file.
///
/// This is the async version of [`std::os::unix::fs::symlink`].
///
/// # Platform-specific behavior
///
/// - On other Unix platforms (not Linux or Windows), this either offloads to a
///   blocking thread pool or falls back to [`std::os::unix::fs::symlink`].
///
/// # Errors
///
/// This function will return an error in the following situations:
/// - `src` does not exist or is not a file
/// - `dst` already exists
/// - The process lacks permissions to create the symlink
/// - The platform does not support symbolic links
#[cfg(not(any(windows, target_os = "linux")))]
pub async fn symlink_file(
    src: impl AsRef<std::path::Path>,
    dst: impl AsRef<std::path::Path>,
) -> std::io::Result<()> {
    if crate::vibeio::executor::offload_fs() {
        let src = src.as_ref().to_owned();
        let dst = dst.as_ref().to_owned();
        crate::vibeio::spawn_blocking(move || std::os::unix::fs::symlink(src, dst))
            .await
            .map_err(|_| crate::vibeio::fs::file::blocking_pool_io_error())?
    } else {
        std::os::unix::fs::symlink(src, dst)
    }
}

/// Creates a symbolic link.
///
/// This is a convenience function that calls [`symlink_file`]. Use this when you
/// don't know or don't care whether the source is a file or directory.
///
/// For explicit symlink creation, use [`symlink_file`] or [`symlink_dir`] instead.
///
/// # Platform-specific behavior
///
/// See [`symlink_file`] for platform-specific behavior details.
///
/// # Errors
///
/// See [`symlink_file`] for error conditions.
pub async fn symlink(
    src: impl AsRef<std::path::Path>,
    dst: impl AsRef<std::path::Path>,
) -> std::io::Result<()> {
    symlink_file(src, dst).await
}

/// Renames a file or directory to a new location.
///
/// This is the async version of [`std::fs::rename`].
///
/// # Platform-specific behavior
///
/// - On Linux with io_uring support, this uses the `renameat` syscall directly.
/// - On other platforms, this either offloads to a blocking thread pool or falls back
///   to [`std::fs::rename`].
///
/// # Errors
///
/// This function will return an error in the following situations:
/// - `from` does not exist
/// - `to` already exists and is not overwritable
/// - The source and destination are on different filesystems
/// - The process lacks permissions
#[cfg(target_os = "linux")]
pub async fn rename(
    from: impl AsRef<std::path::Path>,
    to: impl AsRef<std::path::Path>,
) -> std::io::Result<()> {
    let from = from.as_ref();
    let to = to.as_ref();

    let driver = crate::vibeio::executor::current_driver();
    if driver.as_ref().is_some_and(|d| d.supports_completion()) {
        let from_cstr = CString::new(from.as_os_str().as_encoded_bytes()).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("Invalid path: {}", e),
            )
        })?;
        let to_cstr = CString::new(to.as_os_str().as_encoded_bytes()).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("Invalid path: {}", e),
            )
        })?;
        let driver = driver.expect("invalid driver state");
        let mut op = RenameOp::new(driver.clone(), from_cstr, to_cstr);
        std::future::poll_fn(|cx| op.poll_completion(cx, driver.as_ref())).await
    } else if crate::vibeio::executor::offload_fs() {
        let from = from.to_owned();
        let to = to.to_owned();
        crate::vibeio::spawn_blocking(move || std::fs::rename(from, to))
            .await
            .map_err(|_| crate::vibeio::fs::file::blocking_pool_io_error())?
    } else {
        std::fs::rename(from, to)
    }
}

/// Renames a file or directory to a new location.
///
/// This is the async version of [`std::fs::rename`].
///
/// # Platform-specific behavior
///
/// - On platforms other than Linux, this either offloads to a blocking thread pool
///   or falls back to [`std::fs::rename`].
///
/// # Errors
///
/// This function will return an error in the following situations:
/// - `from` does not exist
/// - `to` already exists and is not overwritable
/// - The source and destination are on different filesystems
/// - The process lacks permissions
#[cfg(not(target_os = "linux"))]
pub async fn rename(
    from: impl AsRef<std::path::Path>,
    to: impl AsRef<std::path::Path>,
) -> std::io::Result<()> {
    if crate::vibeio::executor::offload_fs() {
        let from = from.as_ref().to_owned();
        let to = to.as_ref().to_owned();
        crate::vibeio::spawn_blocking(move || std::fs::rename(from, to))
            .await
            .map_err(|_| crate::vibeio::fs::file::blocking_pool_io_error())?
    } else {
        std::fs::rename(from, to)
    }
}

/// Removes an empty directory.
///
/// This is the async version of [`std::fs::remove_dir`].
///
/// # Platform-specific behavior
///
/// - On Linux with io_uring support, this uses the `unlinkat` syscall directly.
/// - On other platforms, this either offloads to a blocking thread pool or falls back
///   to [`std::fs::remove_dir`].
///
/// # Errors
///
/// This function will return an error in the following situations:
/// - `path` does not exist
/// - `path` is not a directory
/// - The directory is not empty
/// - The process lacks permissions
#[cfg(target_os = "linux")]
pub async fn remove_dir(path: impl AsRef<std::path::Path>) -> std::io::Result<()> {
    let path = path.as_ref();

    let driver = crate::vibeio::executor::current_driver();
    if driver.as_ref().is_some_and(|d| d.supports_completion()) {
        let path_cstr = CString::new(path.as_os_str().as_encoded_bytes()).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("Invalid path: {}", e),
            )
        })?;
        let driver = driver.expect("invalid driver state");
        let mut op = UnlinkOp::new(driver.clone(), path_cstr, true);
        std::future::poll_fn(|cx| op.poll_completion(cx, driver.as_ref())).await
    } else if crate::vibeio::executor::offload_fs() {
        let path = path.to_owned();
        crate::vibeio::spawn_blocking(move || std::fs::remove_dir(path))
            .await
            .map_err(|_| crate::vibeio::fs::file::blocking_pool_io_error())?
    } else {
        std::fs::remove_dir(path)
    }
}

/// Removes an empty directory.
///
/// This is the async version of [`std::fs::remove_dir`].
///
/// # Platform-specific behavior
///
/// - On platforms other than Linux, this either offloads to a blocking thread pool
///   or falls back to [`std::fs::remove_dir`].
///
/// # Errors
///
/// This function will return an error in the following situations:
/// - `path` does not exist
/// - `path` is not a directory
/// - The directory is not empty
/// - The process lacks permissions
#[cfg(not(target_os = "linux"))]
pub async fn remove_dir(path: impl AsRef<std::path::Path>) -> std::io::Result<()> {
    if crate::vibeio::executor::offload_fs() {
        let path = path.as_ref().to_owned();
        crate::vibeio::spawn_blocking(move || std::fs::remove_dir(path))
            .await
            .map_err(|_| crate::vibeio::fs::file::blocking_pool_io_error())?
    } else {
        std::fs::remove_dir(path)
    }
}

/// Removes a file.
///
/// This is the async version of [`std::fs::remove_file`].
///
/// # Platform-specific behavior
///
/// - On Linux with io_uring support, this uses the `unlinkat` syscall directly.
/// - On other platforms, this either offloads to a blocking thread pool or falls back
///   to [`std::fs::remove_file`].
///
/// # Errors
///
/// This function will return an error in the following situations:
/// - `path` does not exist
/// - The process lacks permissions
#[cfg(target_os = "linux")]
pub async fn remove_file(path: impl AsRef<std::path::Path>) -> std::io::Result<()> {
    let path = path.as_ref();

    let driver = crate::vibeio::executor::current_driver();
    if driver.as_ref().is_some_and(|d| d.supports_completion()) {
        let path_cstr = CString::new(path.as_os_str().as_encoded_bytes()).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("Invalid path: {}", e),
            )
        })?;
        let driver = driver.expect("invalid driver state");
        let mut op = UnlinkOp::new(driver.clone(), path_cstr, false);
        std::future::poll_fn(|cx| op.poll_completion(cx, driver.as_ref())).await
    } else if crate::vibeio::executor::offload_fs() {
        let path = path.to_owned();
        crate::vibeio::spawn_blocking(move || std::fs::remove_file(path))
            .await
            .map_err(|_| crate::vibeio::fs::file::blocking_pool_io_error())?
    } else {
        std::fs::remove_file(path)
    }
}

/// Removes a file.
///
/// This is the async version of [`std::fs::remove_file`].
///
/// # Platform-specific behavior
///
/// - On platforms other than Linux, this either offloads to a blocking thread pool
///   or falls back to [`std::fs::remove_file`].
///
/// # Errors
///
/// This function will return an error in the following situations:
/// - `path` does not exist
/// - The process lacks permissions
#[cfg(not(target_os = "linux"))]
pub async fn remove_file(path: impl AsRef<std::path::Path>) -> std::io::Result<()> {
    if crate::vibeio::executor::offload_fs() {
        let path = path.as_ref().to_owned();
        crate::vibeio::spawn_blocking(move || std::fs::remove_file(path))
            .await
            .map_err(|_| crate::vibeio::fs::file::blocking_pool_io_error())?
    } else {
        std::fs::remove_file(path)
    }
}

/// Creates a directory.
///
/// This is the async version of [`std::fs::create_dir`].
///
/// # Platform-specific behavior
///
/// - On Linux with io_uring support, this uses the `mkdirat` syscall directly.
/// - On other platforms, this either offloads to a blocking thread pool or falls back
///   to [`std::fs::create_dir`].
///
/// # Errors
///
/// This function will return an error in the following situations:
/// - A component in the path does not exist
/// - A component in the path is not a directory
/// - The process lacks permissions
/// - The directory already exists
#[cfg(target_os = "linux")]
pub async fn create_dir(path: impl AsRef<std::path::Path>) -> std::io::Result<()> {
    let path = path.as_ref();

    let driver = crate::vibeio::executor::current_driver();
    if driver.as_ref().is_some_and(|d| d.supports_completion()) {
        let path_cstr = CString::new(path.as_os_str().as_encoded_bytes()).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("Invalid path: {}", e),
            )
        })?;
        let driver = driver.expect("invalid driver state");
        // mode 0o777 is standard for mkdir, umask will be applied
        let mut op = MkDirOp::new(driver.clone(), path_cstr, 0o777);
        std::future::poll_fn(|cx| op.poll_completion(cx, driver.as_ref())).await
    } else if crate::vibeio::executor::offload_fs() {
        let path = path.to_owned();
        crate::vibeio::spawn_blocking(move || std::fs::create_dir(path))
            .await
            .map_err(|_| crate::vibeio::fs::file::blocking_pool_io_error())?
    } else {
        std::fs::create_dir(path)
    }
}

/// Creates a directory.
///
/// This is the async version of [`std::fs::create_dir`].
///
/// # Platform-specific behavior
///
/// - On platforms other than Linux, this either offloads to a blocking thread pool
///   or falls back to [`std::fs::create_dir`].
///
/// # Errors
///
/// This function will return an error in the following situations:
/// - A component in the path does not exist
/// - A component in the path is not a directory
/// - The process lacks permissions
/// - The directory already exists
#[cfg(not(target_os = "linux"))]
pub async fn create_dir(path: impl AsRef<std::path::Path>) -> std::io::Result<()> {
    if crate::vibeio::executor::offload_fs() {
        let path = path.as_ref().to_owned();
        crate::vibeio::spawn_blocking(move || std::fs::create_dir(path))
            .await
            .map_err(|_| crate::vibeio::fs::file::blocking_pool_io_error())?
    } else {
        std::fs::create_dir(path)
    }
}

/// Creates a new, empty directory and all its parent components if they don't exist.
///
/// This is the async version of [`std::fs::create_dir_all`].
///
/// # Platform-specific behavior
///
/// - This function internally calls [`create_dir`] for each directory component,
///   so it inherits the platform-specific behavior of that function.
///
/// # Errors
///
/// This function will return an error in the following situations:
/// - A component in the path cannot be created
/// - A component in the path is not a directory
/// - The process lacks permissions
pub async fn create_dir_all(path: impl AsRef<std::path::Path>) -> std::io::Result<()> {
    let path = path.as_ref();
    let mut stack = Vec::new();
    let mut p = path;

    // Build stack of missing directories
    loop {
        // Try to create current path
        match create_dir(p).await {
            Ok(()) => break,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // Exists. Check if dir.
                if let Ok(metadata) = metadata(p).await {
                    if metadata.is_dir() {
                        break;
                    }
                }
                return Err(e);
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                // Parent missing.
                stack.push(p);
                match p.parent() {
                    Some(parent) => p = parent,
                    None => return Err(e),
                }
            }
            Err(e) => return Err(e),
        }
    }

    // Now create directories in stack in reverse order (top to bottom)
    while let Some(p) = stack.pop() {
        match create_dir(p).await {
            Ok(()) => continue,
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                if let Ok(metadata) = metadata(p).await {
                    if metadata.is_dir() {
                        continue;
                    }
                }
                return Err(e);
            }
            Err(e) => return Err(e),
        }
    }

    Ok(())
}

/// Returns metadata about a file or directory.
///
/// This is the async version of [`std::fs::metadata`].
///
/// # Platform-specific behavior
///
/// - On Linux with io_uring support and glibc/musl v1.2.3+, this uses the `statx` syscall directly
///   for better async performance.
/// - On other platforms, this either offloads to a blocking thread pool or falls back
///   to [`std::fs::metadata`].
///
/// # Errors
///
/// This function will return an error in the following situations:
/// - `path` does not exist
/// - The process lacks permissions to access the path
#[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
pub async fn metadata(path: impl AsRef<std::path::Path>) -> std::io::Result<Metadata> {
    let path = path.as_ref();

    let driver = crate::vibeio::executor::current_driver();
    if driver.as_ref().is_some_and(|d| d.supports_completion()) {
        let path_cstr = CString::new(path.as_os_str().as_encoded_bytes()).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("Invalid path: {}", e),
            )
        })?;
        let driver = driver.expect("invalid driver state");
        let mut op = crate::vibeio::op::StatxOp::new(
            driver.clone(),
            libc::AT_FDCWD,
            path_cstr,
            0,
            libc::STATX_ALL,
        );
        let statx = std::future::poll_fn(move |cx| op.poll_completion(cx, &driver)).await?;
        Ok(Metadata::from_statx(statx))
    } else if crate::vibeio::executor::offload_fs() {
        let path = path.to_owned();
        Ok(Metadata::from_std(
            crate::vibeio::spawn_blocking(move || std::fs::metadata(path))
                .await
                .map_err(|_| crate::vibeio::fs::file::blocking_pool_io_error())??,
        ))
    } else {
        Ok(Metadata::from_std(std::fs::metadata(path)?))
    }
}

/// Returns metadata about a file or directory.
///
/// This is the async version of [`std::fs::metadata`].
///
/// # Platform-specific behavior
///
/// - On platforms other than Linux with glibc/musl v1.2.3+, this either offloads
///   to a blocking thread pool or falls back to [`std::fs::metadata`].
///
/// # Errors
///
/// This function will return an error in the following situations:
/// - `path` does not exist
/// - The process lacks permissions to access the path
#[cfg(not(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3))))]
pub async fn metadata(path: impl AsRef<std::path::Path>) -> std::io::Result<Metadata> {
    if crate::vibeio::executor::offload_fs() {
        let path = path.as_ref().to_owned();
        Ok(Metadata::from_std(
            crate::vibeio::spawn_blocking(move || std::fs::metadata(path))
                .await
                .map_err(|_| crate::vibeio::fs::file::blocking_pool_io_error())??,
        ))
    } else {
        Ok(Metadata::from_std(std::fs::metadata(path)?))
    }
}

/// Returns metadata about a file or directory without following symlinks.
///
/// This is the async version of [`std::fs::symlink_metadata`].
///
/// # Platform-specific behavior
///
/// - On Linux with io_uring support and glibc/musl v1.2.3+, this uses the `statx` syscall directly
///   with `AT_SYMLINK_NOFOLLOW` flag for better async performance.
/// - On other platforms, this either offloads to a blocking thread pool or falls back
///   to [`std::fs::symlink_metadata`].
///
/// # Errors
///
/// This function will return an error in the following situations:
/// - `path` does not exist
/// - The process lacks permissions to access the path
#[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
pub async fn symlink_metadata(path: impl AsRef<std::path::Path>) -> std::io::Result<Metadata> {
    let path = path.as_ref();

    let driver = crate::vibeio::executor::current_driver();
    if driver.as_ref().is_some_and(|d| d.supports_completion()) {
        let path_cstr = CString::new(path.as_os_str().as_encoded_bytes()).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("Invalid path: {}", e),
            )
        })?;
        let driver = driver.expect("invalid driver state");
        let mut op = crate::vibeio::op::StatxOp::new(
            driver.clone(),
            libc::AT_FDCWD,
            path_cstr,
            libc::AT_SYMLINK_NOFOLLOW,
            libc::STATX_ALL,
        );
        let statx = std::future::poll_fn(move |cx| op.poll_completion(cx, &driver)).await?;
        Ok(Metadata::from_statx(statx))
    } else if crate::vibeio::executor::offload_fs() {
        let path = path.to_owned();
        Ok(Metadata::from_std(
            crate::vibeio::spawn_blocking(move || std::fs::symlink_metadata(path))
                .await
                .map_err(|_| crate::vibeio::fs::file::blocking_pool_io_error())??,
        ))
    } else {
        Ok(Metadata::from_std(std::fs::symlink_metadata(path)?))
    }
}

/// Returns metadata about a file or directory without following symlinks.
///
/// This is the async version of [`std::fs::symlink_metadata`].
///
/// # Platform-specific behavior
///
/// - On platforms other than Linux with glibc/musl v1.2.3+, this either offloads
///   to a blocking thread pool or falls back to [`std::fs::symlink_metadata`].
///
/// # Errors
///
/// This function will return an error in the following situations:
/// - `path` does not exist
/// - The process lacks permissions to access the path
#[cfg(not(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3))))]
pub async fn symlink_metadata(path: impl AsRef<std::path::Path>) -> std::io::Result<Metadata> {
    if crate::vibeio::executor::offload_fs() {
        let path = path.as_ref().to_owned();
        Ok(Metadata::from_std(
            crate::vibeio::spawn_blocking(move || std::fs::symlink_metadata(path))
                .await
                .map_err(|_| crate::vibeio::fs::file::blocking_pool_io_error())??,
        ))
    } else {
        Ok(Metadata::from_std(std::fs::symlink_metadata(path)?))
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use crate::vibeio::{
        executor::Runtime,
        fs::{File, OpenOptions, metadata, read, read_to_string, write},
        io::AsyncWrite,
    };

    // Exercise offloaded filesystem operations without requiring the optional
    // default pool implementation to be enabled by an unrelated feature.
    fn filesystem_test_runtime() -> Runtime {
        struct TestPool;
        impl crate::vibeio::blocking::BlockingThreadPool for TestPool {
            fn spawn(&self, task: Box<dyn FnOnce() + Send>) {
                std::thread::spawn(task);
            }
        }
        crate::vibeio::RuntimeBuilder::new()
            .driver(crate::vibeio::DriverKind::Mock)
            .blocking_pool(Box::new(TestPool))
            .build()
            .unwrap()
    }

    fn unique_path(name: &str) -> PathBuf {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("vibeio_{name}_{now}.tmp"))
    }

    #[test]
    fn fs_read_write_helpers_work() {
        let runtime = filesystem_test_runtime();
        runtime.block_on(async {
            let path = unique_path("helpers");
            write(&path, b"hello world")
                .await
                .expect("write helper should succeed");

            let bytes = read(&path).await.expect("read helper should succeed");
            assert_eq!(bytes, b"hello world");

            let string = read_to_string(&path)
                .await
                .expect("read_to_string helper should succeed");
            assert_eq!(string, "hello world");

            let _ = std::fs::remove_file(path);
        });
    }

    #[test]
    fn file_read_at_and_write_exact_at_work() {
        let runtime = filesystem_test_runtime();
        runtime.block_on(async {
            let path = unique_path("offset");
            let file = OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(true)
                .open(&path)
                .await
                .expect("open for write should succeed");
            file.write_exact_at(b"abcdef".to_vec(), 0)
                .await
                .0
                .expect("write_exact_at should succeed");

            let file = File::open(&path)
                .await
                .expect("open for read should succeed");
            let (read, out) = file.read_exact_at([0u8; 4], 2).await;
            read.expect("read_exact_at should succeed");
            assert_eq!(&out, b"cdef");

            let _ = std::fs::remove_file(path);
        });
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn append_and_truncate_rejection_preserves_existing_file() {
        for driver in [
            crate::vibeio::DriverKind::Mock,
            crate::vibeio::DriverKind::IoUring,
        ] {
            let path = unique_path("invalid_append_truncate");
            std::fs::write(&path, b"preserve this").unwrap();
            let input = path.clone();
            let runtime = crate::vibeio::RuntimeBuilder::new()
                .driver(driver)
                .build()
                .unwrap();
            let (result, exclusive) = runtime.block_on(async move {
                let result = OpenOptions::new()
                    .append(true)
                    .truncate(true)
                    .open(&input)
                    .await
                    .map(drop);
                let exclusive = OpenOptions::new()
                    .append(true)
                    .truncate(true)
                    .create_new(true)
                    .open(&input)
                    .await
                    .map(drop);
                (result, exclusive)
            });
            let contents = std::fs::read(&path).unwrap();
            std::fs::remove_file(path).unwrap();
            assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::InvalidInput);
            assert_eq!(
                exclusive.unwrap_err().kind(),
                std::io::ErrorKind::AlreadyExists
            );
            assert_eq!(contents, b"preserve this");
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_symlinks_reject_embedded_nuls() {
        for (path, target) in [("\0", "target"), ("link", "\0")] {
            assert_eq!(
                super::windows_symlink_file(path.into(), target.into())
                    .unwrap_err()
                    .kind(),
                std::io::ErrorKind::InvalidInput
            );
            assert_eq!(
                super::windows_symlink_dir(path.into(), target.into())
                    .unwrap_err()
                    .kind(),
                std::io::ErrorKind::InvalidInput
            );
        }
        filesystem_test_runtime().block_on(async {
            for (source, destination) in [("\0", "link"), ("target", "\0")] {
                assert_eq!(
                    super::symlink_file(source, destination)
                        .await
                        .unwrap_err()
                        .kind(),
                    std::io::ErrorKind::InvalidInput
                );
                assert_eq!(
                    super::symlink_dir(source, destination)
                        .await
                        .unwrap_err()
                        .kind(),
                    std::io::ErrorKind::InvalidInput
                );
            }
        });
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn all_open_flag_combinations_match_standard_filesystem_behavior() {
        let root = unique_path("open_flag_matrix");
        std::fs::create_dir(&root).unwrap();
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(root.clone());
        for (backend, driver) in [
            crate::vibeio::DriverKind::Mock,
            crate::vibeio::DriverKind::IoUring,
        ]
        .into_iter()
        .enumerate()
        {
            let root = root.clone();
            let runtime = crate::vibeio::RuntimeBuilder::new()
                .driver(driver)
                .build()
                .unwrap();
            runtime.block_on(async move {
                for flags in 0..64u8 {
                    for exists in [false, true] {
                        let reference = root.join(format!("std-{backend}-{flags}-{exists}"));
                        let candidate = root.join(format!("vibeio-{backend}-{flags}-{exists}"));
                        if exists {
                            std::fs::write(&reference, b"preserve").unwrap();
                            std::fs::write(&candidate, b"preserve").unwrap();
                        }
                        let enabled = |bit: u32| flags & (1u8 << bit) != 0;
                        let expected = std::fs::OpenOptions::new()
                            .read(enabled(0))
                            .write(enabled(1))
                            .append(enabled(2))
                            .truncate(enabled(3))
                            .create(enabled(4))
                            .create_new(enabled(5))
                            .open(&reference)
                            .map(drop)
                            .map_err(|e| e.kind());
                        let actual = OpenOptions::new()
                            .read(enabled(0))
                            .write(enabled(1))
                            .append(enabled(2))
                            .truncate(enabled(3))
                            .create(enabled(4))
                            .create_new(enabled(5))
                            .open(&candidate)
                            .await
                            .map(drop)
                            .map_err(|e| e.kind());
                        assert_eq!(
                            actual, expected,
                            "backend={backend}, flags={flags:06b}, exists={exists}"
                        );
                        assert_eq!(
                            std::fs::read(&candidate).map_err(|e| e.kind()),
                            std::fs::read(&reference).map_err(|e| e.kind()),
                            "file effects: backend={backend}, flags={flags:06b}, exists={exists}"
                        );
                    }
                }
            });
        }
    }

    #[cfg(windows)]
    #[test]
    fn windows_symlinks_preserve_source_and_destination() {
        use std::os::windows::ffi::OsStringExt;
        let root = unique_path("symlink_order");
        std::fs::create_dir(&root).unwrap();
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(root.clone());
        // An unpaired surrogate cannot survive a lossy UTF-8 round trip.
        let source = root.join(std::ffi::OsString::from_wide(&[0x6f, 0xd800]));
        std::fs::write(&source, b"unchanged").unwrap();
        // Probe capability using the standard API, not the implementation under
        // test. Some Windows installations require symlink privileges.
        if let Err(error) = std::os::windows::fs::symlink_file(&source, root.join("probe")) {
            if error.raw_os_error() == Some(1314) {
                eprintln!("symlink execution unavailable: {error}");
                return;
            }
            panic!("symlink capability probe failed: {error}");
        }
        filesystem_test_runtime().block_on(async move {
            let destination = root.join("file-link");
            super::symlink_file(&source, &destination).await.unwrap();
            assert_eq!(std::fs::read_link(&destination).unwrap(), source);
            assert_eq!(std::fs::read(&source).unwrap(), b"unchanged");
            let directory = root.join("directory");
            std::fs::create_dir(&directory).unwrap();
            let destination = root.join("directory-link");
            super::symlink_dir(&directory, &destination).await.unwrap();
            assert_eq!(std::fs::read_link(destination).unwrap(), directory);
        });
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn write_helper_preserves_binary_data_and_truncates_existing_files() {
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
        for driver in [
            crate::vibeio::DriverKind::Mock,
            crate::vibeio::DriverKind::IoUring,
        ] {
            let path = unique_path("write_helper");
            let _cleanup = Cleanup(path.clone());
            let runtime = crate::vibeio::RuntimeBuilder::new()
                .driver(driver)
                .build()
                .unwrap();
            runtime.block_on(async move {
                let data = (0..131_072).map(|i| (i % 251) as u8).collect::<Vec<_>>();
                write(&path, &data).await.unwrap();
                assert_eq!(std::fs::read(&path).unwrap(), data);
                write(&path, b"short\0binary").await.unwrap();
                assert_eq!(std::fs::read(&path).unwrap(), b"short\0binary");
                write(&path, []).await.unwrap();
                assert_eq!(std::fs::metadata(&path).unwrap().len(), 0);
            });
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn positional_io_rejects_cursor_sentinel_without_touching_file() {
        use std::io::{Read, Seek, SeekFrom, Write};
        for driver in [
            crate::vibeio::DriverKind::Mock,
            crate::vibeio::DriverKind::IoUring,
        ] {
            let path = unique_path("positional_sentinel");
            let mut backing = std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .create_new(true)
                .open(&path)
                .unwrap();
            // Only this newly created file is unlinked; open handles retain it
            // until the test ends, including on assertion failure.
            std::fs::remove_file(path).unwrap();
            backing.write_all(b"abcdef").unwrap();
            backing.seek(SeekFrom::Start(2)).unwrap();
            let inner = backing.try_clone().unwrap();
            let runtime = crate::vibeio::RuntimeBuilder::new()
                .driver(driver)
                .build()
                .unwrap();
            runtime.block_on(async move {
                let file = File::from_std(inner).unwrap();
                let (result, buffer) = file.read_at(vec![7u8; 3], u64::MAX).await;
                assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::InvalidInput);
                assert_eq!(buffer, vec![7; 3]);
                let (result, buffer) = file.write_at(b"BAD".to_vec(), u64::MAX).await;
                assert_eq!(result.unwrap_err().kind(), std::io::ErrorKind::InvalidInput);
                assert_eq!(buffer, b"BAD");
                let (result, buffer) = file.read_at(Vec::with_capacity(2), 1).await;
                assert_eq!(result.unwrap(), 2);
                assert_eq!(buffer, b"bc");
                assert_eq!(file.write_at(b"e".to_vec(), 4).await.0.unwrap(), 1);
            });
            assert_eq!(backing.stream_position().unwrap(), 2);
            backing.rewind().unwrap();
            let mut content = Vec::new();
            backing.read_to_end(&mut content).unwrap();
            assert_eq!(content, b"abcdef");
        }
    }

    #[cfg(feature = "blocking-default")]
    #[test]
    fn file_reads_use_spare_capacity_in_direct_and_offloaded_paths() {
        for offload in [false, true] {
            let path = unique_path("spare_capacity");
            std::fs::write(&path, b"abcdef").unwrap();
            let runtime = crate::vibeio::RuntimeBuilder::new()
                .driver(crate::vibeio::DriverKind::Mock)
                .enable_fs_offload(offload)
                .default_blocking_pool(1)
                .build()
                .unwrap();
            let input = path.clone();
            runtime.block_on(async move {
                let file = File::open(input).await.unwrap();
                let (result, buf) = file.read_at(Vec::with_capacity(3), 1).await;
                assert_eq!(result.unwrap(), 3);
                assert_eq!(buf, b"bcd");
                let (result, buf) = file.read_exact_at(Vec::with_capacity(4), 2).await;
                result.unwrap();
                assert_eq!(buf, b"cdef");
                let (result, buf) = file.read_exact_at(Vec::with_capacity(8), 4).await;
                assert_eq!(
                    result.unwrap_err().kind(),
                    std::io::ErrorKind::UnexpectedEof
                );
                assert_eq!(buf, b"ef");
                let (result, buf) = file.read_at(vec![7; 4], 6).await;
                assert_eq!(result.unwrap(), 0);
                assert!(buf.is_empty());
            });
            std::fs::remove_file(path).unwrap();
        }
    }

    #[test]
    fn metadata_basic_properties() {
        let runtime = filesystem_test_runtime();
        runtime.block_on(async {
            let path = unique_path("metadata");
            write(&path, b"test content")
                .await
                .expect("write should succeed");

            let md = metadata(&path).await.expect("metadata should succeed");
            assert_eq!(md.len(), 12);
            assert!(md.is_file());
            assert!(!md.is_dir());
            assert!(!md.is_symlink());

            let _ = std::fs::remove_file(path);
        });
    }

    #[test]
    fn metadata_directory() {
        let runtime = filesystem_test_runtime();
        runtime.block_on(async {
            let path = unique_path("dir");
            std::fs::create_dir(&path).expect("create_dir should succeed");

            let md = metadata(&path).await.expect("metadata should succeed");
            assert!(md.is_dir());
            assert!(!md.is_file());

            let _ = std::fs::remove_dir(path);
        });
    }

    #[test]
    fn metadata_timestamps() {
        let runtime = filesystem_test_runtime();
        runtime.block_on(async {
            let path = unique_path("timestamps");
            write(&path, b"test").await.expect("write should succeed");

            let md = metadata(&path).await.expect("metadata should succeed");

            // All timestamp methods should return valid SystemTime
            let accessed = md.accessed().expect("accessed should succeed");
            let modified = md.modified().expect("modified should succeed");
            let created = md.created().expect("created should succeed");

            // Timestamps should be reasonable (not in the far future)
            let now = SystemTime::now();
            assert!(accessed <= now || accessed + Duration::from_secs(1) >= now);
            assert!(modified <= now || modified + Duration::from_secs(1) >= now);
            assert!(created <= now || created + Duration::from_secs(1) >= now);

            let _ = std::fs::remove_file(path);
        });
    }

    #[test]
    fn metadata_permissions() {
        let runtime = filesystem_test_runtime();
        runtime.block_on(async {
            let path = unique_path("perms");
            write(&path, b"test").await.expect("write should succeed");

            let md = metadata(&path).await.expect("metadata should succeed");
            let perms = md.permissions();

            // Should be readable and writable by owner
            assert!(!perms.readonly(), "file should be writable");

            let _ = std::fs::remove_file(path);
        });
    }

    #[test]
    fn metadata_file_type() {
        let runtime = filesystem_test_runtime();
        runtime.block_on(async {
            let path = unique_path("type");
            write(&path, b"test").await.expect("write should succeed");

            let md = metadata(&path).await.expect("metadata should succeed");
            let file_type = md.file_type();

            assert!(file_type.is_file());
            assert!(!file_type.is_dir());
            assert!(!file_type.is_symlink());

            let _ = std::fs::remove_file(path);
        });
    }

    #[test]
    fn metadata_empty_file() {
        let runtime = filesystem_test_runtime();
        runtime.block_on(async {
            let path = unique_path("empty");
            let mut file = OpenOptions::new()
                .write(true)
                .create(true)
                .open(&path)
                .await
                .expect("open should succeed");
            file.flush().await.expect("flush should succeed");

            let md = metadata(&path).await.expect("metadata should succeed");
            assert_eq!(md.len(), 0);
            assert!(md.is_file());

            let _ = std::fs::remove_file(path);
        });
    }

    #[test]
    fn create_dir_works() {
        let runtime = filesystem_test_runtime();
        runtime.block_on(async {
            let path = unique_path("create_dir");
            crate::vibeio::fs::create_dir(&path)
                .await
                .expect("create_dir should succeed");

            let md = metadata(&path).await.expect("metadata should succeed");
            assert!(md.is_dir());

            crate::vibeio::fs::remove_dir(path)
                .await
                .expect("remove_dir should succeed");
        });
    }

    #[test]
    fn create_dir_all_works() {
        let runtime = filesystem_test_runtime();
        runtime.block_on(async {
            let base = unique_path("create_dir_all");
            let path = base.join("a/b/c");

            crate::vibeio::fs::create_dir_all(&path)
                .await
                .expect("create_dir_all should succeed");

            let md = metadata(&path).await.expect("metadata should succeed");
            assert!(md.is_dir());

            // Clean up
            crate::vibeio::fs::remove_dir(base.join("a/b/c"))
                .await
                .expect("remove_dir c");
            crate::vibeio::fs::remove_dir(base.join("a/b"))
                .await
                .expect("remove_dir b");
            crate::vibeio::fs::remove_dir(base.join("a"))
                .await
                .expect("remove_dir a");
            crate::vibeio::fs::remove_dir(&base)
                .await
                .expect("remove_dir base");
        });
    }

    #[cfg(unix)]
    #[test]
    fn symlink_metadata_works() {
        let runtime = filesystem_test_runtime();
        runtime.block_on(async {
            let target = unique_path("symlink_target");
            let link = unique_path("symlink");

            // Create a file and a symlink to it
            write(&target, b"test content")
                .await
                .expect("write should succeed");
            crate::vibeio::fs::symlink_file(&target, &link)
                .await
                .expect("symlink_file should succeed");

            // symlink_metadata on the symlink should return info about the symlink itself
            let md = crate::vibeio::fs::symlink_metadata(&link)
                .await
                .expect("symlink_metadata should succeed");
            assert!(md.is_symlink());

            // metadata on the symlink should follow the link and return info about the target
            let target_md = metadata(&link).await.expect("metadata should succeed");
            assert!(target_md.is_file());
            assert!(!target_md.is_symlink());
            assert_eq!(target_md.len(), 12);

            // Clean up
            crate::vibeio::fs::remove_file(&link)
                .await
                .expect("remove_file should succeed");
            crate::vibeio::fs::remove_file(&target)
                .await
                .expect("remove_file should succeed");
        });
    }

    #[test]
    fn symlink_metadata_on_regular_file() {
        let runtime = filesystem_test_runtime();
        runtime.block_on(async {
            let path = unique_path("regular_file");
            write(&path, b"content")
                .await
                .expect("write should succeed");

            // symlink_metadata on a regular file should work the same as metadata
            let md = crate::vibeio::fs::symlink_metadata(&path)
                .await
                .expect("symlink_metadata should succeed");
            assert!(md.is_file());
            assert!(!md.is_symlink());
            assert_eq!(md.len(), 7);

            let _ = std::fs::remove_file(path);
        });
    }
}
