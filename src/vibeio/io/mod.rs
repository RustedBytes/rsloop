//! Async I/O traits and utilities.
//!
//! This module provides the core async I/O abstractions used by vibeio:
//! - `AsyncRead` and `AsyncWrite`: async traits for reading and writing.
//! - `AsyncReadPoll` and `AsyncWritePoll`: poll-based async read/write readiness interfaces.
//! - `IoBuf` and `IoBufMut`: buffer traits for async I/O operations.
//! - `pipe()`: create async-aware pipe endpoints.
//! - `splice()` and `sendfile_exact()`: zero-copy I/O operations (Linux only).
//! - `copy()`: copy data between async I/O objects.
//! - `split()`: split I/O objects into independent read/write halves.
//!
//! # Examples
//!
//! See the executable examples in `tools/vibeio-check/EXAMPLES.md` for buffer
//! ownership, pipes, and copying through EOF. Prefer [`copy`] for a transfer loop:
//! it writes only bytes actually read and handles partial writes before reusing
//! the buffer.
//!
//! # Implementation notes
//! - The `AsyncRead` and `AsyncWrite` traits are similar to tokio's but return
//!   `(Result<T, Error>, Buffer)` to support buffer reuse.
//! - The `IoBuf` trait is implemented for common buffer types like `Vec<u8>`,
//!   `String`, byte arrays, and `Box<[u8]>`.
//! - On Linux with the `splice` feature, `splice()` and `sendfile_exact()`
//!   provide zero-copy I/O.

#![allow(async_fn_in_trait)]

mod buf;
#[cfg(all(unix, feature = "pipe"))]
mod pipe;
#[cfg(all(target_os = "linux", feature = "splice"))]
mod splice;
#[cfg(feature = "stdio")]
mod stdio;
mod util;

use crate::vibeio::fd_inner::InnerRawHandle;

pub use self::buf::*;
#[cfg(all(unix, feature = "pipe"))]
pub use self::pipe::*;
#[cfg(all(target_os = "linux", feature = "splice"))]
pub use self::splice::*;
#[cfg(feature = "stdio")]
pub use self::stdio::*;
pub use self::util::*;

use std::io::{self, ErrorKind};

/// Async trait for reading data.
///
/// This trait is similar to `tokio::io::AsyncRead` but returns a tuple
/// `(Result<usize, Error>, Buffer)` to support buffer reuse.
pub trait AsyncRead {
    /// Read data into the buffer, returning the number of bytes read and the buffer.
    ///
    /// Reads may use the full writable capacity, including spare capacity in an
    /// empty vector. On success, the first returned-count bytes must be initialized.
    /// The buffer may contain additional initialized bytes; those are not part
    /// of this read. A zero count indicates EOF unless capacity was zero.
    async fn read<B: IoBufMut>(&mut self, buf: B) -> (Result<usize, io::Error>, B);

    /// Read data into vectored buffers.
    #[inline]
    async fn read_vectored<V: IoVectoredBufMut>(
        &mut self,
        bufs: V,
    ) -> (Result<usize, io::Error>, V) {
        (
            Err(io::Error::new(
                ErrorKind::Unsupported,
                "readv not implemented",
            )),
            bufs,
        )
    }
}

/// Async trait for writing data.
///
/// This trait is similar to `tokio::io::AsyncWrite` but returns a tuple
/// `(Result<usize, Error>, Buffer)` to support buffer reuse.
pub trait AsyncWrite {
    /// Write data from the buffer, returning the number of bytes written and the buffer.
    /// The returned count must not exceed the supplied initialized length.
    async fn write<B: IoBuf>(&mut self, buf: B) -> (Result<usize, io::Error>, B);

    /// Write data from vectored buffers.
    #[inline]
    async fn write_vectored<V: IoVectoredBuf>(&mut self, bufs: V) -> (Result<usize, io::Error>, V) {
        (
            Err(io::Error::new(
                ErrorKind::Unsupported,
                "writev not implemented",
            )),
            bufs,
        )
    }

    /// Flush the writer.
    #[inline]
    async fn flush(&mut self) -> Result<(), io::Error> {
        Ok(())
    }
}

/// Trait to get an inner raw handle reference.
pub trait AsInnerRawHandle<'a> {
    /// Returns a reference to the inner raw handle.
    #[allow(private_interfaces)]
    fn as_inner_raw_handle(&'a self) -> &'a crate::vibeio::fd_inner::InnerRawHandle;
}

impl<'a> AsInnerRawHandle<'a> for InnerRawHandle {
    #[inline]
    fn as_inner_raw_handle(&'a self) -> &'a InnerRawHandle {
        self
    }
}

/// Trait to poll for read readiness.
pub trait AsyncReadPoll {
    /// Polls to check if the reader is readable.
    fn poll_readable(&self, cx: &mut std::task::Context) -> std::task::Poll<io::Result<()>>;

    /// Returns a future that resolves when the reader becomes readable.
    #[inline]
    async fn readable(&self) -> Result<(), io::Error> {
        std::future::poll_fn(|cx| self.poll_readable(cx)).await
    }
}

/// Trait to poll for write readiness.
pub trait AsyncWritePoll {
    /// Polls to check if the writer is writable.
    fn poll_writable(&self, cx: &mut std::task::Context) -> std::task::Poll<io::Result<()>>;

    /// Returns a future that resolves when the writer becomes writable.
    #[inline]
    async fn writable(&self) -> Result<(), io::Error> {
        std::future::poll_fn(|cx| self.poll_writable(cx)).await
    }
}
