use std::cell::RefCell;
use std::future::poll_fn;
use std::io::{self, ErrorKind};
use std::mem::ManuallyDrop;
use std::path::Path;
use std::sync::{Arc, Mutex};

use mio::Interest;

#[cfg(unix)]
use std::os::fd::{AsRawFd, IntoRawFd, RawFd};
#[cfg(windows)]
use std::os::windows::io::{AsRawHandle, IntoRawHandle, RawHandle};

use crate::vibeio::fs::Metadata;
use crate::vibeio::io::{IoBuf, IoBufMut, IoBufWithCursor, iobuf_to_slice, read_into_buf};
use crate::vibeio::{
    driver::RegistrationMode,
    executor::current_driver,
    fd_inner::InnerRawHandle,
    io::{AsyncRead, AsyncWrite},
    op::{ReadAtOp, WriteAtOp},
};

#[cfg(windows)]
use crate::vibeio::fd_inner::RawOsHandle;

use crate::vibeio::fs::open_options::OpenOptions;

/// A file handle for asynchronous file I/O operations.
///
/// This struct provides async versions of common file operations like reading,
/// writing, and syncing. It supports both io_uring completion-based I/O on Linux
/// and blocking thread pool fallback for other platforms.
///
/// # Examples
///
/// ```ignore
/// use vibeio::fs::File;
///
/// // Open a file for reading
/// let file = File::open("hello.txt").await?;
///
/// // Read from the file
/// let mut buf = [0u8; 1024];
/// let (read, buf) = file.read_at(buf, 0).await;
/// let read = read?;
///
/// println!("Read {} bytes", read);
/// ```
enum FileIo {
    Completion(ManuallyDrop<InnerRawHandle>),
    Blocking,
}

/// A file handle for asynchronous file I/O operations.
///
/// This struct provides async versions of common file operations like reading,
/// writing, and syncing. It supports both io_uring completion-based I/O on Linux
/// and blocking thread pool fallback for other platforms.
///
/// # Examples
///
/// ```ignore
/// use vibeio::fs::File;
///
/// // Open a file for reading
/// let file = File::open("hello.txt").await?;
///
/// // Read from the file
/// let mut buf = [0u8; 1024];
/// let (read, buf) = file.read_at(buf, 0).await;
/// let read = read?;
///
/// println!("Read {} bytes", read);
/// ```
pub struct File {
    inner: std::fs::File,
    io: FileIo,
    cursor: u64,
}

impl File {
    /// Opens a file for reading.
    ///
    /// This is the async version of [`std::fs::File::open`].
    ///
    /// # Platform-specific behavior
    ///
    /// - On Linux with io_uring support, this uses the `openat` syscall directly.
    /// - On other platforms, this either offloads to a blocking thread pool or falls back
    ///   to [`std::fs::File::open`].
    ///
    /// # Errors
    ///
    /// This function will return an error in the following situations:
    /// - `path` does not exist
    /// - The process lacks permissions to read the file
    ///
    /// # Examples
    ///
    /// ```ignore
    /// use vibeio::fs::File;
    ///
    /// let file = File::open("hello.txt").await?;
    /// ```
    #[inline]
    pub async fn open(path: impl AsRef<Path>) -> io::Result<Self> {
        OpenOptions::new().read(true).open(path).await
    }

    /// Opens a file for writing, creating it if it does not exist.
    ///
    /// This is the async version of [`std::fs::File::create`].
    ///
    /// # Platform-specific behavior
    ///
    /// - On Linux with io_uring support, this uses the `openat` syscall directly.
    /// - On other platforms, this either offloads to a blocking thread pool or falls back
    ///   to [`std::fs::File::create`].
    ///
    /// # Errors
    ///
    /// This function will return an error in the following situations:
    /// - The file cannot be created
    /// - The process lacks permissions to create the file
    ///
    /// # Examples
    ///
    /// ```ignore
    /// use vibeio::fs::File;
    ///
    /// let file = File::create("hello.txt").await?;
    /// ```
    #[inline]
    pub async fn create(path: impl AsRef<Path>) -> io::Result<Self> {
        OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(path)
            .await
    }

    /// Returns a new `OpenOptions` builder.
    ///
    /// This is a convenience method equivalent to `OpenOptions::new()`.
    #[inline]
    pub fn options() -> OpenOptions {
        OpenOptions::new()
    }

    /// Creates a new `File` from a standard library file.
    ///
    /// This is a convenience method equivalent to `File::from_std_with_cursor(inner, 0)`.
    #[inline]
    pub fn from_std(inner: std::fs::File) -> io::Result<Self> {
        Self::from_std_with_cursor(inner, 0)
    }

    /// Creates a new `File` from a standard library file with a specified cursor position.
    ///
    /// This is an internal method used to create a `File` with a custom cursor position.
    #[inline]
    pub(crate) fn from_std_with_cursor(inner: std::fs::File, cursor: u64) -> io::Result<Self> {
        let io = if let Some(driver) = current_driver() {
            if driver.supports_completion() {
                #[cfg(unix)]
                let raw_handle = inner.as_raw_fd();
                #[cfg(windows)]
                let raw_handle = RawOsHandle::Handle(inner.as_raw_handle());

                match InnerRawHandle::new_with_driver_and_mode(
                    &driver,
                    raw_handle,
                    Interest::READABLE | Interest::WRITABLE,
                    RegistrationMode::Completion,
                ) {
                    Ok(handle) => FileIo::Completion(ManuallyDrop::new(handle)),
                    Err(_) => FileIo::Blocking,
                }
            } else {
                FileIo::Blocking
            }
        } else {
            FileIo::Blocking
        };

        Ok(Self { inner, io, cursor })
    }

    /// Converts the `File` back into a standard library `std::fs::File`.
    #[inline]
    pub fn into_std(self) -> std::fs::File {
        let mut this = ManuallyDrop::new(self);
        unsafe {
            if let FileIo::Completion(handle) = &mut this.io {
                ManuallyDrop::drop(handle);
            }
            std::ptr::read(&this.inner)
        }
    }

    /// Returns the completion handle if this file is using io_uring completion.
    #[inline]
    fn completion_handle(&self) -> Option<&InnerRawHandle> {
        match &self.io {
            FileIo::Completion(handle) => Some(handle),
            FileIo::Blocking => None,
        }
    }

    /// Reads bytes from the file at a specific offset.
    ///
    /// This method reads into the provided buffer starting at the given offset.
    /// The cursor position of the file is not modified.
    ///
    /// # Platform-specific behavior
    ///
    /// - On Linux with io_uring support, this submits positional `Read` operations.
    /// - On other platforms, this either offloads to a blocking thread pool or falls back
    ///   to synchronous reading.
    ///
    /// # Errors
    ///
    /// This function will return an error in the following situations:
    /// - The read operation fails
    /// - The offset is invalid
    ///
    /// # Examples
    ///
    /// ```ignore
    /// use vibeio::fs::File;
    ///
    /// let file = File::open("hello.txt").await?;
    /// let mut buf = [0u8; 1024];
    /// let (read, buf) = file.read_at(buf, 0).await;
    /// let read = read?;
    /// ```
    #[inline]
    pub async fn read_at<B: IoBufMut>(&self, mut buf: B, offset: u64) -> (io::Result<usize>, B) {
        if buf.buf_capacity() == 0 {
            return (Ok(0), buf);
        }

        if let Some(handle) = self.completion_handle() {
            let mut op = ReadAtOp::new(handle, buf, offset);
            let result = poll_fn(|cx| handle.poll_op(cx, &mut op)).await;
            (result, op.take_bufs())
        } else if crate::vibeio::executor::offload_fs() && current_driver().is_some() {
            read_at_in_blocking_pool(&self.inner, buf, offset).await
        } else {
            let result = read_into_buf(&mut buf, |slice| {
                read_at_blocking(&self.inner, slice, offset)
            });
            (result, buf)
        }
    }

    /// Reads bytes from the file at a specific offset, filling the entire buffer.
    ///
    /// This method fills the provided buffer's writable capacity starting at the
    /// given offset, including spare capacity in an empty Vec. The cursor
    /// position of the file is not modified.
    /// Interrupted reads are retried. Reaching EOF before filling the buffer
    /// returns [`io::ErrorKind::UnexpectedEof`].
    ///
    /// # Platform-specific behavior
    ///
    /// - On Linux with io_uring support, this submits positional `Read` operations.
    /// - On other platforms, this either offloads to a blocking thread pool or falls back
    ///   to synchronous reading.
    ///
    /// # Errors
    ///
    /// This function will return an error in the following situations:
    /// - The read operation fails
    /// - The offset is invalid
    /// - The file does not contain enough data to fill the buffer
    ///
    /// # Examples
    ///
    /// ```ignore
    /// use vibeio::fs::File;
    ///
    /// let file = File::open("hello.txt").await?;
    /// let mut buf = [0u8; 1024];
    /// let (result, buf) = file.read_exact_at(buf, 0).await;
    /// result?;
    /// ```
    #[inline]
    pub async fn read_exact_at<B: IoBufMut>(&self, buf: B, offset: u64) -> (io::Result<()>, B) {
        exact_at(buf, offset, ExactAt::Read, |buf, offset| {
            self.read_at(buf, offset)
        })
        .await
    }

    /// Writes bytes to the file at a specific offset.
    ///
    /// This method writes from the provided buffer starting at the given offset.
    /// The cursor position of the file is not modified.
    ///
    /// # Platform-specific behavior
    ///
    /// - On Linux with io_uring support, this submits positional `Write` operations.
    /// - On other platforms, this either offloads to a blocking thread pool or falls back
    ///   to synchronous writing.
    ///
    /// # Errors
    ///
    /// This function will return an error in the following situations:
    /// - The write operation fails
    /// - The offset is invalid
    ///
    /// # Examples
    ///
    /// ```ignore
    /// use vibeio::fs::File;
    ///
    /// let file = File::create("hello.txt").await?;
    /// let buf = b"Hello, world!";
    /// let (written, buf) = file.write_at(buf.to_vec(), 0).await;
    /// let written = written?;
    /// ```
    #[inline]
    pub async fn write_at<B: IoBuf>(&self, buf: B, offset: u64) -> (io::Result<usize>, B) {
        if buf.buf_len() == 0 {
            return (Ok(0), buf);
        }

        #[cfg(windows)]
        if let Err(error) = crate::vibeio::op::validate_windows_write_offset(offset) {
            return (Err(error), buf);
        }

        if let Some(handle) = self.completion_handle() {
            let mut op = WriteAtOp::new(handle, buf, offset);
            let result = poll_fn(|cx| handle.poll_op(cx, &mut op)).await;
            (result, op.take_bufs())
        } else if crate::vibeio::executor::offload_fs() && current_driver().is_some() {
            write_at_in_blocking_pool(&self.inner, buf, offset).await
        } else {
            let slice = iobuf_to_slice(&buf);
            (write_at_blocking(&self.inner, slice, offset), buf)
        }
    }

    /// Writes bytes to the file at a specific offset, writing the entire buffer.
    ///
    /// This method writes from the provided buffer starting at the given offset,
    /// ensuring the entire buffer is written. The cursor position of the file is not modified.
    /// Interrupted writes are retried. A write that makes no progress returns
    /// [`io::ErrorKind::WriteZero`].
    ///
    /// # Platform-specific behavior
    ///
    /// - On Linux with io_uring support, this submits positional `Write` operations.
    /// - On other platforms, this either offloads to a blocking thread pool or falls back
    ///   to synchronous writing.
    ///
    /// # Errors
    ///
    /// This function will return an error in the following situations:
    /// - The write operation fails
    /// - The offset is invalid
    /// - The write operation fails to write the entire buffer
    ///
    /// # Examples
    ///
    /// ```ignore
    /// use vibeio::fs::File;
    ///
    /// let file = File::create("hello.txt").await?;
    /// let buf = b"Hello, world!";
    /// let (result, buf) = file.write_exact_at(buf.to_vec(), 0).await;
    /// result?;
    /// ```
    #[inline]
    pub async fn write_exact_at<B: IoBuf>(&self, buf: B, offset: u64) -> (io::Result<()>, B) {
        exact_at(buf, offset, ExactAt::Write, |buf, offset| {
            self.write_at(buf, offset)
        })
        .await
    }

    /// Synchronizes all data and metadata to disk.
    ///
    /// This is the async version of [`std::fs::File::sync_all`].
    ///
    /// # Platform-specific behavior
    ///
    /// - On Linux with io_uring support, this uses the `fsync` syscall directly.
    /// - On other platforms, this either offloads to a blocking thread pool or falls back
    ///   to [`std::fs::File::sync_all`].
    ///
    /// # Errors
    ///
    /// This function will return an error in the following situations:
    /// - The sync operation fails
    ///
    /// # Examples
    ///
    /// ```ignore
    /// use vibeio::fs::File;
    ///
    /// let file = File::create("hello.txt").await?;
    /// file.sync_all().await?;
    /// ```
    #[inline]
    pub async fn sync_all(&self) -> io::Result<()> {
        if let Some(handle) = self.completion_handle() {
            #[cfg(target_os = "linux")]
            {
                let mut op = crate::vibeio::op::FsyncOp::new(handle, false);
                poll_fn(move |cx| handle.poll_op(cx, &mut op)).await
            }
            #[cfg(not(target_os = "linux"))]
            {
                let _ = handle;
                sync_all_in_blocking_pool(&self.inner).await
            }
        } else if crate::vibeio::executor::offload_fs() && current_driver().is_some() {
            sync_all_in_blocking_pool(&self.inner).await
        } else {
            sync_all_blocking(&self.inner)
        }
    }

    /// Synchronizes file data to disk without necessarily syncing metadata.
    ///
    /// This is the async version of [`std::fs::File::sync_data`].
    ///
    /// # Platform-specific behavior
    ///
    /// - On Linux with io_uring support, this uses the `fsync` syscall directly.
    /// - On other platforms, this either offloads to a blocking thread pool or falls back
    ///   to [`std::fs::File::sync_data`].
    ///
    /// # Errors
    ///
    /// This function will return an error in the following situations:
    /// - The sync operation fails
    ///
    /// # Examples
    ///
    /// ```ignore
    /// use vibeio::fs::File;
    ///
    /// let file = File::create("hello.txt").await?;
    /// file.sync_data().await?;
    /// ```
    #[inline]
    pub async fn sync_data(&self) -> io::Result<()> {
        if let Some(handle) = self.completion_handle() {
            #[cfg(target_os = "linux")]
            {
                let mut op = crate::vibeio::op::FsyncOp::new(handle, true);
                poll_fn(move |cx| handle.poll_op(cx, &mut op)).await
            }
            #[cfg(not(target_os = "linux"))]
            {
                let _ = handle;
                sync_data_in_blocking_pool(&self.inner).await
            }
        } else if crate::vibeio::executor::offload_fs() && current_driver().is_some() {
            sync_data_in_blocking_pool(&self.inner).await
        } else {
            sync_data_blocking(&self.inner)
        }
    }

    /// Returns the metadata for this file.
    ///
    /// This is the async version of [`std::fs::File::metadata`].
    ///
    /// # Platform-specific behavior
    ///
    /// - On Linux with io_uring support and glibc/musl v1.2.3+, this uses the `statx` syscall directly.
    /// - On other platforms, this either offloads to a blocking thread pool or falls back
    ///   to [`std::fs::File::metadata`].
    ///
    /// # Errors
    ///
    /// This function will return an error in the following situations:
    /// - The metadata operation fails
    ///
    /// # Examples
    ///
    /// ```ignore
    /// use vibeio::fs::File;
    ///
    /// let file = File::open("hello.txt").await?;
    /// let metadata = file.metadata().await?;
    /// println!("File size: {} bytes", metadata.len());
    /// ```
    #[inline]
    pub async fn metadata(&self) -> io::Result<Metadata> {
        if let Some(handle) = self.completion_handle() {
            #[cfg(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3)))]
            {
                use std::ffi::CString;

                let mut op = crate::vibeio::op::StatxOp::new(
                    handle.driver_owner(),
                    handle.handle,
                    CString::new(b"").expect("invalid path"),
                    libc::AT_EMPTY_PATH,
                    libc::STATX_ALL,
                );
                let statx = poll_fn(move |cx| handle.poll_op(cx, &mut op)).await?;
                Ok(Metadata::from_statx(statx))
            }
            #[cfg(not(all(target_os = "linux", any(target_env = "gnu", musl_v1_2_3))))]
            {
                let _ = handle;
                metadata_in_blocking_pool(&self.inner).await
            }
        } else if crate::vibeio::executor::offload_fs() && current_driver().is_some() {
            metadata_in_blocking_pool(&self.inner).await
        } else {
            metadata_blocking(&self.inner)
        }
    }
}

enum ExactAt {
    Read,
    Write,
}

#[cfg(test)]
mod exact_at_tests {
    use super::*;

    fn run_script(
        mode: ExactAt,
        len: usize,
        offset: u64,
        results: Vec<io::Result<usize>>,
    ) -> (io::Result<()>, Vec<u8>, Vec<(u64, usize)>) {
        let runtime =
            crate::vibeio::executor::Runtime::new(crate::vibeio::driver::AnyDriver::new_mock());
        runtime.block_on(async move {
            let mut results = results.into_iter();
            let mut calls = Vec::new();
            let (result, buffer) = exact_at(vec![7u8; len], offset, mode, |buf, offset| {
                calls.push((offset, buf.buf_capacity()));
                std::future::ready((results.next().expect("unexpected I/O retry"), buf))
            })
            .await;
            assert!(results.next().is_none(), "unused scripted result");
            (result, buffer, calls)
        })
    }

    #[test]
    fn exact_io_retries_interruptions_without_advancing_position() {
        for mode in [ExactAt::Read, ExactAt::Write] {
            let (result, buffer, calls) = run_script(
                mode,
                4,
                10,
                vec![
                    Err(io::ErrorKind::Interrupted.into()),
                    Ok(1),
                    Err(io::ErrorKind::Interrupted.into()),
                    Ok(3),
                ],
            );
            result.unwrap();
            assert_eq!(buffer, vec![7; 4]);
            assert_eq!(calls, [(10, 4), (10, 4), (11, 3), (11, 3)]);
        }
    }

    #[test]
    fn exact_io_distinguishes_eof_from_write_zero() {
        for (mode, kind) in [
            (ExactAt::Read, ErrorKind::UnexpectedEof),
            (ExactAt::Write, ErrorKind::WriteZero),
        ] {
            let (result, buffer, calls) = run_script(mode, 4, 10, vec![Ok(2), Ok(0)]);
            assert_eq!(result.unwrap_err().kind(), kind);
            assert_eq!(buffer, vec![7; 4]);
            assert_eq!(calls, [(10, 4), (12, 2)]);
        }
    }

    #[test]
    fn exact_io_checks_offset_overflow_only_when_another_io_is_needed() {
        for mode in [ExactAt::Read, ExactAt::Write] {
            let (result, buffer, calls) = run_script(mode, 4, u64::MAX, vec![Ok(1)]);
            assert_eq!(result.unwrap_err().kind(), ErrorKind::InvalidInput);
            assert_eq!(buffer, vec![7; 4]);
            assert_eq!(calls, [(u64::MAX, 4)]);
        }
        let (result, _, calls) = run_script(ExactAt::Write, 1, u64::MAX, vec![Ok(1)]);
        result.unwrap();
        assert_eq!(calls, [(u64::MAX, 1)]);
    }

    #[test]
    fn exact_io_preserves_errors_rejects_excess_counts_and_skips_empty_buffers() {
        for mode in [ExactAt::Read, ExactAt::Write] {
            let (result, buffer, _) = run_script(
                mode,
                4,
                0,
                vec![Ok(1), Err(io::Error::from_raw_os_error(5))],
            );
            assert_eq!(result.unwrap_err().raw_os_error(), Some(5));
            assert_eq!(buffer, vec![7; 4]);
        }
        for mode in [ExactAt::Read, ExactAt::Write] {
            let (result, buffer, _) = run_script(mode, 4, 0, vec![Ok(5)]);
            assert_eq!(result.unwrap_err().kind(), ErrorKind::InvalidData);
            assert_eq!(buffer, vec![7; 4]);
        }
        for mode in [ExactAt::Read, ExactAt::Write] {
            let (result, buffer, calls) = run_script(mode, 0, u64::MAX, vec![]);
            result.unwrap();
            assert!(buffer.is_empty());
            assert!(calls.is_empty());
        }
    }
}

impl ExactAt {
    fn remaining(&self, buf: &impl IoBuf) -> usize {
        match self {
            Self::Read => buf.buf_capacity(),
            Self::Write => buf.buf_len(),
        }
    }
}

async fn exact_at<B, F, Fut>(
    buf: B,
    mut offset: u64,
    mode: ExactAt,
    mut operation: F,
) -> (io::Result<()>, B)
where
    B: IoBuf,
    F: FnMut(IoBufWithCursor<B>, u64) -> Fut,
    Fut: std::future::Future<Output = (io::Result<usize>, IoBufWithCursor<B>)>,
{
    let mut buf = IoBufWithCursor::new(buf);
    while mode.remaining(&buf) > 0 {
        let remaining = mode.remaining(&buf);
        let (result, returned) = operation(buf, offset).await;
        buf = returned;
        let count = match result {
            Err(error) if error.kind() == ErrorKind::Interrupted => continue,
            Err(error) => return (Err(error), buf.into_inner()),
            Ok(0) => {
                let kind = match mode {
                    ExactAt::Read => ErrorKind::UnexpectedEof,
                    ExactAt::Write => ErrorKind::WriteZero,
                };
                return (
                    Err(io::Error::new(kind, "failed to complete positional I/O")),
                    buf.into_inner(),
                );
            }
            Ok(count) if count > remaining => {
                return (
                    Err(io::Error::new(
                        ErrorKind::InvalidData,
                        "I/O count exceeds remaining buffer",
                    )),
                    buf.into_inner(),
                );
            }
            Ok(count) => count,
        };
        buf.advance(count);
        if mode.remaining(&buf) > 0 {
            let Some(next) = offset.checked_add(count as u64) else {
                return (
                    Err(io::Error::new(
                        ErrorKind::InvalidInput,
                        "file offset overflow",
                    )),
                    buf.into_inner(),
                );
            };
            offset = next;
        }
    }
    (Ok(()), buf.into_inner())
}

#[cfg(unix)]
#[inline]
fn read_at_blocking(file: &std::fs::File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    use std::os::unix::fs::FileExt;
    file.read_at(buf, offset)
}

#[cfg(windows)]
#[inline]
fn read_at_blocking(file: &std::fs::File, buf: &mut [u8], offset: u64) -> io::Result<usize> {
    use std::os::windows::fs::FileExt;
    file.seek_read(buf, offset)
}

#[cfg(unix)]
#[inline]
fn write_at_blocking(file: &std::fs::File, buf: &[u8], offset: u64) -> io::Result<usize> {
    use std::os::unix::fs::FileExt;
    file.write_at(buf, offset)
}

#[cfg(windows)]
#[inline]
fn write_at_blocking(file: &std::fs::File, buf: &[u8], offset: u64) -> io::Result<usize> {
    use std::os::windows::fs::FileExt;
    file.seek_write(buf, offset)
}

#[inline]
fn sync_all_blocking(file: &std::fs::File) -> io::Result<()> {
    file.sync_all()
}

#[inline]
fn sync_data_blocking(file: &std::fs::File) -> io::Result<()> {
    file.sync_data()
}

#[inline]
fn metadata_blocking(file: &std::fs::File) -> io::Result<Metadata> {
    Ok(Metadata::from_std(file.metadata()?))
}

#[inline]
pub(crate) fn blocking_pool_io_error() -> io::Error {
    io::Error::other("can't spawn blocking task for file I/O")
}

#[inline]
async fn read_at_in_blocking_pool<B: IoBufMut>(
    file: &std::fs::File,
    buf: B,
    offset: u64,
) -> (io::Result<usize>, B) {
    let file = match file.try_clone() {
        Ok(file) => file,
        Err(e) => return (Err(e), buf),
    };
    let buf = Arc::new(Mutex::new(RefCell::new(Some(buf))));
    let buf_clone = buf.clone();
    crate::vibeio::spawn_blocking(move || {
        let mut buf = buf_clone
            .try_lock()
            .ok()
            .and_then(|rc| rc.take())
            .expect("buf is none");
        let result = read_into_buf(&mut buf, |slice| read_at_blocking(&file, slice, offset));
        (result, buf)
    })
    .await
    .unwrap_or_else(|_| {
        (
            Err(blocking_pool_io_error()),
            buf.try_lock()
                .ok()
                .and_then(|rc| rc.take())
                .expect("buf is none"),
        )
    })
}

#[inline]
async fn write_at_in_blocking_pool<B: IoBuf>(
    file: &std::fs::File,
    buf: B,
    offset: u64,
) -> (io::Result<usize>, B) {
    let file = match file.try_clone() {
        Ok(file) => file,
        Err(e) => return (Err(e), buf),
    };
    let buf = Arc::new(Mutex::new(RefCell::new(Some(buf))));
    let buf_clone = buf.clone();
    crate::vibeio::spawn_blocking(move || {
        let buf = buf_clone
            .try_lock()
            .ok()
            .and_then(|rc| rc.take())
            .expect("buf is none");
        let temp_slice = iobuf_to_slice(&buf);
        let result = write_at_blocking(&file, temp_slice, offset);
        (result, buf)
    })
    .await
    .unwrap_or_else(|_| {
        (
            Err(blocking_pool_io_error()),
            buf.try_lock()
                .ok()
                .and_then(|rc| rc.take())
                .expect("buf is none"),
        )
    })
}

#[inline]
async fn sync_all_in_blocking_pool(file: &std::fs::File) -> io::Result<()> {
    let file = file.try_clone()?;
    crate::vibeio::spawn_blocking(move || sync_all_blocking(&file))
        .await
        .map_err(|_| blocking_pool_io_error())?
}

#[inline]
async fn sync_data_in_blocking_pool(file: &std::fs::File) -> io::Result<()> {
    let file = file.try_clone()?;
    crate::vibeio::spawn_blocking(move || sync_data_blocking(&file))
        .await
        .map_err(|_| blocking_pool_io_error())?
}

#[inline]
async fn metadata_in_blocking_pool(file: &std::fs::File) -> io::Result<Metadata> {
    let file = file.try_clone()?;
    crate::vibeio::spawn_blocking(move || metadata_blocking(&file))
        .await
        .map_err(|_| blocking_pool_io_error())?
}

impl AsyncRead for File {
    #[inline]
    async fn read<B: IoBufMut>(&mut self, buf: B) -> (Result<usize, io::Error>, B) {
        let (read, buf) = self.read_at(buf, self.cursor).await;
        if let Ok(read) = read {
            self.cursor = self.cursor.saturating_add(read as u64);
        }
        (read, buf)
    }
}

impl AsyncWrite for File {
    #[inline]
    async fn write<B: IoBuf>(&mut self, buf: B) -> (Result<usize, io::Error>, B) {
        let (written, buf) = self.write_at(buf, self.cursor).await;
        if let Ok(written) = written {
            self.cursor = self.cursor.saturating_add(written as u64);
        }
        (written, buf)
    }

    #[inline]
    async fn flush(&mut self) -> Result<(), io::Error> {
        Ok(())
    }
}

impl Drop for File {
    #[inline]
    fn drop(&mut self) {
        unsafe {
            if let FileIo::Completion(handle) = &mut self.io {
                ManuallyDrop::drop(handle);
            }
        }
    }
}

#[cfg(unix)]
impl AsRawFd for File {
    #[inline]
    fn as_raw_fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }
}

#[cfg(unix)]
impl IntoRawFd for File {
    #[inline]
    fn into_raw_fd(self) -> RawFd {
        self.into_std().into_raw_fd()
    }
}

#[cfg(windows)]
impl AsRawHandle for File {
    #[inline]
    fn as_raw_handle(&self) -> RawHandle {
        self.inner.as_raw_handle()
    }
}

#[cfg(windows)]
impl IntoRawHandle for File {
    #[inline]
    fn into_raw_handle(self) -> RawHandle {
        self.into_std().into_raw_handle()
    }
}
