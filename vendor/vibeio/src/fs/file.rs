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

use crate::fs::Metadata;
use crate::io::{iobuf_to_slice, iobufmut_to_slice, IoBuf, IoBufMut, IoBufWithCursor};
use crate::{
    driver::RegistrationMode,
    executor::current_driver,
    fd_inner::InnerRawHandle,
    io::{AsyncRead, AsyncWrite},
    op::{ReadAtOp, WriteAtOp},
};

#[cfg(windows)]
use crate::fd_inner::RawOsHandle;

use crate::fs::open_options::OpenOptions;

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
    /// - On Linux with io_uring support, this uses the `readv` syscall directly.
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
        if buf.buf_len() == 0 {
            return (Ok(0), buf);
        }

        if let Some(handle) = self.completion_handle() {
            let mut op = ReadAtOp::new(handle, buf, offset);
            let result = poll_fn(|cx| handle.poll_op(cx, &mut op)).await;
            (result, op.take_bufs())
        } else if crate::executor::offload_fs() && current_driver().is_some() {
            read_at_in_blocking_pool(&self.inner, buf, offset).await
        } else {
            let slice = iobufmut_to_slice(&mut buf);
            (read_at_blocking(&self.inner, slice, offset), buf)
        }
    }

    /// Reads bytes from the file at a specific offset, filling the entire buffer.
    ///
    /// This method reads into the provided buffer starting at the given offset,
    /// ensuring the entire buffer is filled. The cursor position of the file is not modified.
    ///
    /// # Platform-specific behavior
    ///
    /// - On Linux with io_uring support, this uses the `readv` syscall directly.
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
    pub async fn read_exact_at<B: IoBufMut>(&self, buf: B, mut offset: u64) -> (io::Result<()>, B) {
        let mut buf = IoBufWithCursor::new(buf);
        while buf.buf_len() > 0 {
            let (read, mut buf_returned) = self.read_at(buf, offset).await;
            let read = match read {
                Ok(read) => read,
                Err(err) => {
                    return (Err(err), buf_returned.into_inner());
                }
            };
            if read == 0 {
                return (
                    Err(io::Error::new(
                        ErrorKind::UnexpectedEof,
                        "failed to fill whole buffer",
                    )),
                    buf_returned.into_inner(),
                );
            }

            offset = offset.saturating_add(read as u64);
            buf_returned.advance(read);
            buf = buf_returned
        }

        (Ok(()), buf.into_inner())
    }

    /// Writes bytes to the file at a specific offset.
    ///
    /// This method writes from the provided buffer starting at the given offset.
    /// The cursor position of the file is not modified.
    ///
    /// # Platform-specific behavior
    ///
    /// - On Linux with io_uring support, this uses the `writev` syscall directly.
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

        if let Some(handle) = self.completion_handle() {
            let mut op = WriteAtOp::new(handle, buf, offset);
            let result = poll_fn(|cx| handle.poll_op(cx, &mut op)).await;
            (result, op.take_bufs())
        } else if crate::executor::offload_fs() && current_driver().is_some() {
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
    ///
    /// # Platform-specific behavior
    ///
    /// - On Linux with io_uring support, this uses the `writev` syscall directly.
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
    pub async fn write_exact_at<B: IoBuf>(&self, buf: B, mut offset: u64) -> (io::Result<()>, B) {
        let mut buf = IoBufWithCursor::new(buf);
        while buf.buf_len() > 0 {
            let (written, mut buf_returned) = self.write_at(buf, offset).await;
            let written = match written {
                Ok(written) => written,
                Err(err) => {
                    return (Err(err), buf_returned.into_inner());
                }
            };
            if written == 0 {
                return (
                    Err(io::Error::new(
                        ErrorKind::UnexpectedEof,
                        "failed to write whole buffer",
                    )),
                    buf_returned.into_inner(),
                );
            }

            offset = offset.saturating_add(written as u64);
            buf_returned.advance(written);
            buf = buf_returned;
        }

        (Ok(()), buf.into_inner())
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
                let mut op = crate::op::FsyncOp::new(handle, false);
                poll_fn(move |cx| handle.poll_op(cx, &mut op)).await
            }
            #[cfg(not(target_os = "linux"))]
            {
                let _ = handle;
                sync_all_in_blocking_pool(&self.inner).await
            }
        } else if crate::executor::offload_fs() && current_driver().is_some() {
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
                let mut op = crate::op::FsyncOp::new(handle, true);
                poll_fn(move |cx| handle.poll_op(cx, &mut op)).await
            }
            #[cfg(not(target_os = "linux"))]
            {
                let _ = handle;
                sync_data_in_blocking_pool(&self.inner).await
            }
        } else if crate::executor::offload_fs() && current_driver().is_some() {
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

                let mut op = crate::op::StatxOp::new(
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
        } else if crate::executor::offload_fs() && current_driver().is_some() {
            metadata_in_blocking_pool(&self.inner).await
        } else {
            metadata_blocking(&self.inner)
        }
    }
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
    crate::spawn_blocking(move || {
        let mut buf = buf_clone
            .try_lock()
            .ok()
            .and_then(|rc| rc.take())
            .expect("buf is none");
        let temp_slice = iobufmut_to_slice(&mut buf);
        let result = read_at_blocking(&file, temp_slice, offset);
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
    crate::spawn_blocking(move || {
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
    crate::spawn_blocking(move || sync_all_blocking(&file))
        .await
        .map_err(|_| blocking_pool_io_error())?
}

#[inline]
async fn sync_data_in_blocking_pool(file: &std::fs::File) -> io::Result<()> {
    let file = file.try_clone()?;
    crate::spawn_blocking(move || sync_data_blocking(&file))
        .await
        .map_err(|_| blocking_pool_io_error())?
}

#[inline]
async fn metadata_in_blocking_pool(file: &std::fs::File) -> io::Result<Metadata> {
    let file = file.try_clone()?;
    crate::spawn_blocking(move || metadata_blocking(&file))
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
