//! Zero-copy I/O utilities using `splice` and `sendfile`.
//!
//! This module provides async-aware zero-copy I/O operations:
//! - `splice()`: transfer data between file descriptors without copying to userspace.
//! - `splice_exact()`: transfer exactly `len` bytes using `splice`.
//! - `sendfile_exact()`: transfer data from a file to a socket using a pipe as an intermediary.
//!
//! These operations are only available on Linux with the `splice` feature enabled.
//!
//! # Examples
//!
//! ```ignore
//! use vibeio::io::{splice, AsyncRead, AsyncWrite, pipe};
//!
//! async fn zero_copy_example() {
//!     let (reader, writer) = pipe().unwrap();
//!     let mut file = std::fs::File::open("data.txt").unwrap();
//!
//!     // Transfer 1024 bytes from file to pipe
//!     let n = splice(&file, &writer, 1024).await.unwrap();
//!     println!("Spliced {} bytes", n);
//! }
//! ```

use std::{
    mem::ManuallyDrop,
    os::fd::{AsRawFd, BorrowedFd, FromRawFd, OwnedFd, RawFd},
};

use mio::Interest;

use crate::{fd_inner::InnerRawHandle, io::AsInnerRawHandle, op::SpliceOp};

/// Transfer data from one file descriptor to another using `splice`.
///
/// This function uses the kernel's `splice` system call to transfer data
/// between file descriptors without copying to userspace.
pub async fn splice<'a, 'b>(
    from: &'a impl AsRawFd,
    to: &'b impl AsInnerRawHandle<'b>,
    len: usize,
) -> Result<usize, std::io::Error> {
    let from_handle = unsafe { BorrowedFd::borrow_raw(from.as_raw_fd()) };
    let to_handle = to.as_inner_raw_handle();

    let mut op = SpliceOp::new(from_handle, to_handle, len);
    let result = std::future::poll_fn(move |cx| to_handle.poll_op(cx, &mut op)).await;
    result
}

/// Transfer exactly `len` bytes from one file descriptor to another using `splice`.
///
/// This function calls `splice()` repeatedly until `len` bytes have been transferred
/// or EOF is reached.
pub async fn splice_exact<'a, 'b>(
    from: &'a impl AsRawFd,
    to: &'b impl AsInnerRawHandle<'b>,
    len: u64,
) -> Result<u64, std::io::Error> {
    let mut total = 0;
    while total < len {
        let n = splice(from, to, (len - total).min(usize::MAX as u64) as usize).await?;
        if n == 0 {
            break;
        }
        total += n as u64;
    }

    Ok(total)
}

/// Transfer data from a file to a socket using `sendfile` semantics.
///
/// This function implements `sendfile`-like behavior using `splice` with an
/// intermediate pipe, allowing data to be transferred from a regular file to
/// a socket without copying to userspace.
pub async fn sendfile_exact<'a, 'b>(
    from: &'a impl AsRawFd,
    to: &'b impl AsInnerRawHandle<'b>,
    len: u64,
) -> Result<u64, std::io::Error> {
    // splice() requires at least one of the file descriptors to be a pipe.
    // Therefore, we need to create a pipe and use it as the destination.
    let mut fds: [RawFd; 2] = [0; 2];
    if unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        // pipe2 failed, can't continue
        return Err(std::io::Error::last_os_error());
    }
    let pipe_reader = unsafe { OwnedFd::from_raw_fd(fds[0]) };
    let pipe_writer = unsafe { OwnedFd::from_raw_fd(fds[1]) };

    // We only need to poll the pipe writer for writability.
    let pipe_writer_handle = WriteOwnedFd::new(pipe_writer)?;

    let mut total_from_file = 0;
    let mut total_to_socket = 0;
    let mut file_eof = false;
    while (total_from_file < len && !file_eof) || total_to_socket < total_from_file {
        let splice_from_file_len = (len - total_from_file).min(usize::MAX as u64) as usize;
        if !file_eof && splice_from_file_len > 0 {
            let n = splice(from, &pipe_writer_handle, splice_from_file_len).await?;
            if n == 0 {
                file_eof = true;
            } else {
                total_from_file += n as u64;
            }
        }

        let splice_to_socket_len =
            (total_from_file - total_to_socket).min(usize::MAX as u64) as usize;
        if splice_to_socket_len > 0 {
            let n = splice(&pipe_reader, to, splice_to_socket_len).await?;
            if n == 0 {
                break;
            }
            total_to_socket += n as u64;
        }
    }

    drop(pipe_reader);
    drop(pipe_writer_handle);

    Ok(total_to_socket)
}

struct WriteOwnedFd {
    _writer: OwnedFd,
    handle: ManuallyDrop<InnerRawHandle>,
}

impl WriteOwnedFd {
    fn new(writer: OwnedFd) -> std::io::Result<Self> {
        let handle =
            ManuallyDrop::new(InnerRawHandle::new(writer.as_raw_fd(), Interest::WRITABLE)?);
        if !handle.uses_completion() {
            // Set the pipe write side to non-blocking mode.
            let flags = unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_GETFL) };
            if flags != -1 {
                unsafe { libc::fcntl(writer.as_raw_fd(), libc::F_SETFL, flags | libc::O_NONBLOCK) };
            }
        }
        Ok(Self {
            _writer: writer,
            handle,
        })
    }
}

impl<'a> AsInnerRawHandle<'a> for WriteOwnedFd {
    #[inline]
    fn as_inner_raw_handle(&'a self) -> &'a InnerRawHandle {
        &self.handle
    }
}

impl Drop for WriteOwnedFd {
    #[inline]
    fn drop(&mut self) {
        // Safety: The struct is dropped after the handle is dropped.
        unsafe {
            ManuallyDrop::drop(&mut self.handle);
        }
    }
}
