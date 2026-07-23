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
//! - The runtime must be active when calling these functions; otherwise they will panic.
//!
//! # Examples
//!
//! ```ignore
//! use vibeio::fs;
//!
//! // Write to a file
//! fs::write("hello.txt", b"Hello, world!").await?;
//!
//! // Read from a file
//! let contents = fs::read_to_string("hello.txt").await?;
//! println!("File contents: {}", contents);
//!
//! // Create a directory
//! fs::create_dir("my_dir").await?;
//!
//! ```

mod file;
mod metadata;
mod open_options;

#[cfg(target_os = "linux")]
use std::ffi::CString;
use std::path::PathBuf;

pub use file::*;
pub use metadata::*;
pub use open_options::*;

#[cfg(windows)]
use windows_sys::Win32::Storage::FileSystem::CreateSymbolicLinkW;

use crate::io::IoBuf;
use crate::io::{AsyncRead, AsyncWrite};
#[cfg(target_os = "linux")]
use crate::op::HardLinkOp;
#[cfg(target_os = "linux")]
use crate::op::MkDirOp;
#[cfg(target_os = "linux")]
use crate::op::Op;
#[cfg(target_os = "linux")]
use crate::op::RenameOp;
#[cfg(target_os = "linux")]
use crate::op::SymlinkOp;
#[cfg(target_os = "linux")]
use crate::op::UnlinkOp;

/// Creates a symbolic link to a directory on Windows.
///
/// This is a Windows-specific helper function that uses the `CreateSymbolicLinkW` API.
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
    let path_w: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    let target_w: Vec<u16> = target.encode_utf16().chain(std::iter::once(0)).collect();

    unsafe {
        let res = CreateSymbolicLinkW(
            path_w.as_ptr(),
            target_w.as_ptr(),
            1, // SYMBOLIC_LINK_FLAG_DIRECTORY
        );
        if !res {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
}

/// Creates a symbolic link to a file on Windows.
///
/// This is a Windows-specific helper function that uses the `CreateSymbolicLinkW` API.
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
    let path_w: Vec<u16> = path.encode_utf16().chain(std::iter::once(0)).collect();
    let target_w: Vec<u16> = target.encode_utf16().chain(std::iter::once(0)).collect();

    unsafe {
        let res = CreateSymbolicLinkW(
            path_w.as_ptr(),
            target_w.as_ptr(),
            0, // SYMBOLIC_LINK_FLAG_FILE
        );
        if !res {
            Err(std::io::Error::last_os_error())
        } else {
            Ok(())
        }
    }
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
    if crate::executor::offload_fs() {
        crate::spawn_blocking(move || path.canonicalize())
            .await
            .map_err(|_| crate::fs::file::blocking_pool_io_error())?
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

    let mut slice = contents.as_ref();
    while !slice.is_empty() {
        let (w, _) = file.write(slice.to_vec()).await;
        let w = w?;
        if w == 0 {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WriteZero,
                "failed to write whole buffer",
            ));
        }
        slice = &slice[w..];
    }
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

    let driver = crate::executor::current_driver();
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
        let mut op = HardLinkOp::new(src_cstr, dst_cstr);
        std::future::poll_fn(|cx| op.poll_completion(cx, driver.as_ref())).await
    } else if crate::executor::offload_fs() {
        let src = src.to_owned();
        let dst = dst.to_owned();
        crate::spawn_blocking(move || std::fs::hard_link(src, dst))
            .await
            .map_err(|_| crate::fs::file::blocking_pool_io_error())?
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
    if crate::executor::offload_fs() {
        let src = src.as_ref().to_owned();
        let dst = dst.as_ref().to_owned();
        crate::spawn_blocking(move || std::fs::hard_link(src, dst))
            .await
            .map_err(|_| crate::fs::file::blocking_pool_io_error())?
    } else {
        std::fs::hard_link(src, dst)
    }
}

/// Creates a symbolic link to a directory.
///
/// This is the async version of [`std::os::unix::fs::symlink`] (on Unix) or
/// [`std::os::windows::fs::symlink_dir`] (on Windows).
///
/// # Platform-specific behavior
///
/// - On Linux with io_uring support, this uses the `symlinkat` syscall directly.
/// - On Windows, this uses the [`windows_symlink_dir`] helper.
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
#[cfg(windows)]
pub async fn symlink_dir(
    src: impl AsRef<std::path::Path>,
    dst: impl AsRef<std::path::Path>,
) -> std::io::Result<()> {
    if crate::executor::offload_fs() {
        let src = src.as_ref();
        let dst = dst.as_ref();

        let src_str = src.to_string_lossy().to_string();
        let dst_str = dst.to_string_lossy().to_string();
        crate::spawn_blocking(move || windows_symlink_dir(src_str, dst_str))
            .await
            .map_err(|_| crate::fs::file::blocking_pool_io_error())?
    } else {
        let src = src.as_ref();
        let dst = dst.as_ref();

        let src_str = src.to_string_lossy().to_string();
        let dst_str = dst.to_string_lossy().to_string();

        windows_symlink_dir(src_str, dst_str)
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

    let driver = crate::executor::current_driver();
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
        let mut op = SymlinkOp::new(src_cstr, dst_cstr);
        std::future::poll_fn(|cx| op.poll_completion(cx, driver.as_ref())).await
    } else if crate::executor::offload_fs() {
        let src = src.to_owned();
        let dst = dst.to_owned();
        crate::spawn_blocking(move || std::os::unix::fs::symlink(src, dst))
            .await
            .map_err(|_| crate::fs::file::blocking_pool_io_error())?
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
    if crate::executor::offload_fs() {
        let src = src.as_ref().to_owned();
        let dst = dst.as_ref().to_owned();
        crate::spawn_blocking(move || std::os::unix::fs::symlink(src, dst))
            .await
            .map_err(|_| crate::fs::file::blocking_pool_io_error())?
    } else {
        std::os::unix::fs::symlink(src, dst)
    }
}

/// Creates a symbolic link to a file.
///
/// This is the async version of [`std::os::unix::fs::symlink`] (on Unix) or
/// [`std::os::windows::fs::symlink_file`] (on Windows).
///
/// # Platform-specific behavior
///
/// - On Linux with io_uring support, this uses the `symlinkat` syscall directly.
/// - On Windows, this uses the [`windows_symlink_file`] helper.
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
#[cfg(windows)]
pub async fn symlink_file(
    src: impl AsRef<std::path::Path>,
    dst: impl AsRef<std::path::Path>,
) -> std::io::Result<()> {
    if crate::executor::offload_fs() {
        let src = src.as_ref();
        let dst = dst.as_ref();

        let src_str = src.to_string_lossy().to_string();
        let dst_str = dst.to_string_lossy().to_string();
        crate::spawn_blocking(move || windows_symlink_file(src_str, dst_str))
            .await
            .map_err(|_| crate::fs::file::blocking_pool_io_error())?
    } else {
        let src = src.as_ref();
        let dst = dst.as_ref();

        let src_str = src.to_string_lossy().to_string();
        let dst_str = dst.to_string_lossy().to_string();

        windows_symlink_file(src_str, dst_str)
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

    let driver = crate::executor::current_driver();
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
        let mut op = SymlinkOp::new(src_cstr, dst_cstr);
        std::future::poll_fn(|cx| op.poll_completion(cx, driver.as_ref())).await
    } else if crate::executor::offload_fs() {
        let src = src.to_owned();
        let dst = dst.to_owned();
        crate::spawn_blocking(move || std::os::unix::fs::symlink(src, dst))
            .await
            .map_err(|_| crate::fs::file::blocking_pool_io_error())?
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
    if crate::executor::offload_fs() {
        let src = src.as_ref().to_owned();
        let dst = dst.as_ref().to_owned();
        crate::spawn_blocking(move || std::os::unix::fs::symlink(src, dst))
            .await
            .map_err(|_| crate::fs::file::blocking_pool_io_error())?
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

    let driver = crate::executor::current_driver();
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
        let mut op = RenameOp::new(from_cstr, to_cstr);
        std::future::poll_fn(|cx| op.poll_completion(cx, driver.as_ref())).await
    } else if crate::executor::offload_fs() {
        let from = from.to_owned();
        let to = to.to_owned();
        crate::spawn_blocking(move || std::fs::rename(from, to))
            .await
            .map_err(|_| crate::fs::file::blocking_pool_io_error())?
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
    if crate::executor::offload_fs() {
        let from = from.as_ref().to_owned();
        let to = to.as_ref().to_owned();
        crate::spawn_blocking(move || std::fs::rename(from, to))
            .await
            .map_err(|_| crate::fs::file::blocking_pool_io_error())?
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

    let driver = crate::executor::current_driver();
    if driver.as_ref().is_some_and(|d| d.supports_completion()) {
        let path_cstr = CString::new(path.as_os_str().as_encoded_bytes()).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("Invalid path: {}", e),
            )
        })?;
        let driver = driver.expect("invalid driver state");
        let mut op = UnlinkOp::new(path_cstr, true);
        std::future::poll_fn(|cx| op.poll_completion(cx, driver.as_ref())).await
    } else if crate::executor::offload_fs() {
        let path = path.to_owned();
        crate::spawn_blocking(move || std::fs::remove_dir(path))
            .await
            .map_err(|_| crate::fs::file::blocking_pool_io_error())?
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
    if crate::executor::offload_fs() {
        let path = path.as_ref().to_owned();
        crate::spawn_blocking(move || std::fs::remove_dir(path))
            .await
            .map_err(|_| crate::fs::file::blocking_pool_io_error())?
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

    let driver = crate::executor::current_driver();
    if driver.as_ref().is_some_and(|d| d.supports_completion()) {
        let path_cstr = CString::new(path.as_os_str().as_encoded_bytes()).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("Invalid path: {}", e),
            )
        })?;
        let driver = driver.expect("invalid driver state");
        let mut op = UnlinkOp::new(path_cstr, false);
        std::future::poll_fn(|cx| op.poll_completion(cx, driver.as_ref())).await
    } else if crate::executor::offload_fs() {
        let path = path.to_owned();
        crate::spawn_blocking(move || std::fs::remove_file(path))
            .await
            .map_err(|_| crate::fs::file::blocking_pool_io_error())?
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
    if crate::executor::offload_fs() {
        let path = path.as_ref().to_owned();
        crate::spawn_blocking(move || std::fs::remove_file(path))
            .await
            .map_err(|_| crate::fs::file::blocking_pool_io_error())?
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

    let driver = crate::executor::current_driver();
    if driver.as_ref().is_some_and(|d| d.supports_completion()) {
        let path_cstr = CString::new(path.as_os_str().as_encoded_bytes()).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("Invalid path: {}", e),
            )
        })?;
        let driver = driver.expect("invalid driver state");
        // mode 0o777 is standard for mkdir, umask will be applied
        let mut op = MkDirOp::new(path_cstr, 0o777);
        std::future::poll_fn(|cx| op.poll_completion(cx, driver.as_ref())).await
    } else if crate::executor::offload_fs() {
        let path = path.to_owned();
        crate::spawn_blocking(move || std::fs::create_dir(path))
            .await
            .map_err(|_| crate::fs::file::blocking_pool_io_error())?
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
    if crate::executor::offload_fs() {
        let path = path.as_ref().to_owned();
        crate::spawn_blocking(move || std::fs::create_dir(path))
            .await
            .map_err(|_| crate::fs::file::blocking_pool_io_error())?
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

    let driver = crate::executor::current_driver();
    if driver.as_ref().is_some_and(|d| d.supports_completion()) {
        let path_cstr = CString::new(path.as_os_str().as_encoded_bytes()).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("Invalid path: {}", e),
            )
        })?;
        let driver = driver.expect("invalid driver state");
        let mut op = crate::op::StatxOp::new(libc::AT_FDCWD, path_cstr, 0, libc::STATX_ALL);
        let statx = std::future::poll_fn(move |cx| op.poll_completion(cx, &driver)).await?;
        Ok(Metadata::from_statx(statx))
    } else if crate::executor::offload_fs() {
        let path = path.to_owned();
        Ok(Metadata::from_std(
            crate::spawn_blocking(move || std::fs::metadata(path))
                .await
                .map_err(|_| crate::fs::file::blocking_pool_io_error())??,
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
    if crate::executor::offload_fs() {
        let path = path.as_ref().to_owned();
        Ok(Metadata::from_std(
            crate::spawn_blocking(move || std::fs::metadata(path))
                .await
                .map_err(|_| crate::fs::file::blocking_pool_io_error())??,
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

    let driver = crate::executor::current_driver();
    if driver.as_ref().is_some_and(|d| d.supports_completion()) {
        let path_cstr = CString::new(path.as_os_str().as_encoded_bytes()).map_err(|e| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidInput,
                format!("Invalid path: {}", e),
            )
        })?;
        let driver = driver.expect("invalid driver state");
        let mut op = crate::op::StatxOp::new(
            libc::AT_FDCWD,
            path_cstr,
            libc::AT_SYMLINK_NOFOLLOW,
            libc::STATX_ALL,
        );
        let statx = std::future::poll_fn(move |cx| op.poll_completion(cx, &driver)).await?;
        Ok(Metadata::from_statx(statx))
    } else if crate::executor::offload_fs() {
        let path = path.to_owned();
        Ok(Metadata::from_std(
            crate::spawn_blocking(move || std::fs::symlink_metadata(path))
                .await
                .map_err(|_| crate::fs::file::blocking_pool_io_error())??,
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
    if crate::executor::offload_fs() {
        let path = path.as_ref().to_owned();
        Ok(Metadata::from_std(
            crate::spawn_blocking(move || std::fs::symlink_metadata(path))
                .await
                .map_err(|_| crate::fs::file::blocking_pool_io_error())??,
        ))
    } else {
        Ok(Metadata::from_std(std::fs::symlink_metadata(path)?))
    }
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;
    use std::time::{Duration, SystemTime, UNIX_EPOCH};

    use crate::{
        driver::AnyDriver,
        executor::Runtime,
        fs::{metadata, read, read_to_string, write, File, OpenOptions},
        io::AsyncWrite,
    };

    fn unique_path(name: &str) -> PathBuf {
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("system clock should be after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!("vibeio_{name}_{now}.tmp"))
    }

    #[test]
    fn fs_read_write_helpers_work() {
        let runtime = Runtime::new(AnyDriver::new_mock());
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
        let runtime = Runtime::new(AnyDriver::new_mock());
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

    #[test]
    fn metadata_basic_properties() {
        let runtime = Runtime::new(AnyDriver::new_mock());
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
        let runtime = Runtime::new(AnyDriver::new_mock());
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
        let runtime = Runtime::new(AnyDriver::new_mock());
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
        let runtime = Runtime::new(AnyDriver::new_mock());
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
        let runtime = Runtime::new(AnyDriver::new_mock());
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
        let runtime = Runtime::new(AnyDriver::new_mock());
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
        let runtime = Runtime::new(AnyDriver::new_mock());
        runtime.block_on(async {
            let path = unique_path("create_dir");
            crate::fs::create_dir(&path)
                .await
                .expect("create_dir should succeed");

            let md = metadata(&path).await.expect("metadata should succeed");
            assert!(md.is_dir());

            crate::fs::remove_dir(path)
                .await
                .expect("remove_dir should succeed");
        });
    }

    #[test]
    fn create_dir_all_works() {
        let runtime = Runtime::new(AnyDriver::new_mock());
        runtime.block_on(async {
            let base = unique_path("create_dir_all");
            let path = base.join("a/b/c");

            crate::fs::create_dir_all(&path)
                .await
                .expect("create_dir_all should succeed");

            let md = metadata(&path).await.expect("metadata should succeed");
            assert!(md.is_dir());

            // Clean up
            crate::fs::remove_dir(base.join("a/b/c"))
                .await
                .expect("remove_dir c");
            crate::fs::remove_dir(base.join("a/b"))
                .await
                .expect("remove_dir b");
            crate::fs::remove_dir(base.join("a"))
                .await
                .expect("remove_dir a");
            crate::fs::remove_dir(&base).await.expect("remove_dir base");
        });
    }

    #[cfg(unix)]
    #[test]
    fn symlink_metadata_works() {
        let runtime = Runtime::new(AnyDriver::new_mock());
        runtime.block_on(async {
            let target = unique_path("symlink_target");
            let link = unique_path("symlink");

            // Create a file and a symlink to it
            write(&target, b"test content")
                .await
                .expect("write should succeed");
            crate::fs::symlink_file(&target, &link)
                .await
                .expect("symlink_file should succeed");

            // symlink_metadata on the symlink should return info about the symlink itself
            let md = crate::fs::symlink_metadata(&link)
                .await
                .expect("symlink_metadata should succeed");
            assert!(md.is_symlink());

            // metadata on the symlink should follow the link and return info about the target
            let target_md = metadata(&link).await.expect("metadata should succeed");
            assert!(target_md.is_file());
            assert!(!target_md.is_symlink());
            assert_eq!(target_md.len(), 12);

            // Clean up
            crate::fs::remove_file(&link)
                .await
                .expect("remove_file should succeed");
            crate::fs::remove_file(&target)
                .await
                .expect("remove_file should succeed");
        });
    }

    #[test]
    fn symlink_metadata_on_regular_file() {
        let runtime = Runtime::new(AnyDriver::new_mock());
        runtime.block_on(async {
            let path = unique_path("regular_file");
            write(&path, b"content")
                .await
                .expect("write should succeed");

            // symlink_metadata on a regular file should work the same as metadata
            let md = crate::fs::symlink_metadata(&path)
                .await
                .expect("symlink_metadata should succeed");
            assert!(md.is_file());
            assert!(!md.is_symlink());
            assert_eq!(md.len(), 7);

            let _ = std::fs::remove_file(path);
        });
    }
}
