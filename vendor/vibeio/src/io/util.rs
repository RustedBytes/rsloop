//! Async I/O utilities.
//!
//! This module provides utility functions for async I/O operations:
//! - `copy()`: copy data from a reader to a writer.
//! - `split()`: split an I/O object into independent read/write halves.
//! - `copy_bidirectional()`: copy data in both directions between two I/O objects.
//!
//! # Examples
//!
//! ```ignore
//! use vibeio::io::{AsyncRead, AsyncWrite, copy};
//!
//! async fn copy_example<R: AsyncRead, W: AsyncWrite>(reader: &mut R, writer: &mut W) {
//!     let bytes_copied = copy(reader, writer).await?;
//!     println!("Copied {} bytes", bytes_copied);
//! }
//! ```

use std::io;
use std::sync::Arc;

use futures_util::lock::Mutex as AsyncMutex;

use super::{AsyncRead, AsyncWrite};
use crate::io::{IoBuf, IoBufMut, IoBufWithCursor};

/// Copy data from a reader to a writer.
///
/// This function reads from `reader` and writes to `writer` until EOF is reached.
/// Returns the number of bytes copied.
pub async fn copy<R, W>(reader: &mut R, writer: &mut W) -> Result<u64, io::Error>
where
    R: AsyncRead + ?Sized,
    W: AsyncWrite + ?Sized,
{
    let mut buffer = vec![0u8; 8192];
    buffer.clear();
    let mut copied = 0u64;

    loop {
        let (read, returned_buf) = reader.read(buffer).await;
        let read = read?;

        if read == 0 {
            break;
        }

        let mut cursor_buf = IoBufWithCursor::new(returned_buf);
        while cursor_buf.buf_len() > 0 {
            let (w, mut returned_buf) = writer.write(cursor_buf).await;
            let w = w?;
            if w == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "failed to write whole buffer",
                ));
            }
            returned_buf.advance(w);
            cursor_buf = returned_buf;
        }

        buffer = cursor_buf.into_inner();
        unsafe {
            buffer.set_buf_init(0);
        } // reset
        copied = copied.saturating_add(read as u64);
    }

    writer.flush().await?;
    Ok(copied)
}

/// Owned read half for a split I/O object.
///
/// The halves share ownership of the inner object via an `Arc<AsyncMutex<T>>`.
pub struct ReadHalf<T> {
    inner: Arc<AsyncMutex<T>>,
}

/// Owned write half for a split I/O object.
pub struct WriteHalf<T> {
    inner: Arc<AsyncMutex<T>>,
}

/// Split an object implementing both `AsyncRead` and `AsyncWrite` into two
/// independently usable halves.
///
/// The halves share ownership of the original object via an `Arc<AsyncMutex<T>>`
/// so they may be used concurrently in async contexts.
///
/// Note: this is a simple, owned split helper — it clones an `Arc` around
/// a mutex protecting the whole I/O object. It does not provide lock-free
/// simultaneous read/write on the underlying object; callers still need to
/// tolerate possible contention on the mutex.
pub fn split<T>(io: T) -> (ReadHalf<T>, WriteHalf<T>)
where
    T: AsyncRead + AsyncWrite + 'static,
{
    let inner = Arc::new(AsyncMutex::new(io));
    (
        ReadHalf {
            inner: inner.clone(),
        },
        WriteHalf { inner },
    )
}

impl<T> ReadHalf<T>
where
    T: AsyncRead + AsyncWrite + 'static,
{
    /// Consume the half and return the shared inner `Arc<AsyncMutex<T>>`.
    pub fn into_inner(self) -> Arc<AsyncMutex<T>> {
        self.inner
    }
}

impl<T> WriteHalf<T>
where
    T: AsyncRead + AsyncWrite + 'static,
{
    /// Consume the half and return the shared inner `Arc<AsyncMutex<T>>`.
    pub fn into_inner(self) -> Arc<AsyncMutex<T>> {
        self.inner
    }
}

impl<T> AsyncRead for ReadHalf<T>
where
    T: AsyncRead + AsyncWrite + 'static,
{
    async fn read<B: crate::io::IoBufMut>(&mut self, buf: B) -> (Result<usize, io::Error>, B) {
        let mut guard = self.inner.lock().await;
        // Forward the call to the underlying object.
        (*guard).read(buf).await
    }
}

impl<T> AsyncWrite for WriteHalf<T>
where
    T: AsyncRead + AsyncWrite + 'static,
{
    async fn write<B: crate::io::IoBuf>(&mut self, buf: B) -> (Result<usize, io::Error>, B) {
        let mut guard = self.inner.lock().await;
        (*guard).write(buf).await
    }

    async fn flush(&mut self) -> Result<(), io::Error> {
        let mut guard = self.inner.lock().await;
        (*guard).flush().await
    }
}

impl<R: AsyncRead + ?Sized> AsyncRead for Box<R> {
    #[inline]
    async fn read<B: crate::io::IoBufMut>(&mut self, buf: B) -> (Result<usize, std::io::Error>, B) {
        (**self).read(buf).await
    }
}

impl<R: AsyncRead + ?Sized> AsyncRead for &mut R {
    #[inline]
    async fn read<B: crate::io::IoBufMut>(&mut self, buf: B) -> (Result<usize, std::io::Error>, B) {
        (**self).read(buf).await
    }
}

impl<W: AsyncWrite + ?Sized> AsyncWrite for Box<W> {
    #[inline]
    async fn write<B: crate::io::IoBuf>(&mut self, buf: B) -> (Result<usize, std::io::Error>, B) {
        (**self).write(buf).await
    }

    #[inline]
    async fn flush(&mut self) -> Result<(), std::io::Error> {
        (**self).flush().await
    }
}

impl<W: AsyncWrite + ?Sized> AsyncWrite for &mut W {
    #[inline]
    async fn write<B: crate::io::IoBuf>(&mut self, buf: B) -> (Result<usize, std::io::Error>, B) {
        (**self).write(buf).await
    }

    #[inline]
    async fn flush(&mut self) -> Result<(), std::io::Error> {
        (**self).flush().await
    }
}

/// Copy data in both directions between two I/O objects that implement both
/// `AsyncRead` and `AsyncWrite`.
///
/// This function takes ownership of both objects, splits them into read/write
/// halves (so the copies may proceed concurrently), and runs two `copy`
/// operations in parallel:
/// - bytes read from `a` are written to `b`
/// - bytes read from `b` are written to `a`
///
/// Returns a tuple `(a_to_b, b_to_a)` with the number of bytes copied in each
/// direction. The function returns an error if either direction returns an
/// error.
pub async fn copy_bidirectional<A, B>(a: A, b: B) -> Result<(u64, u64), io::Error>
where
    A: AsyncRead + AsyncWrite + 'static,
    B: AsyncRead + AsyncWrite + 'static,
{
    // Split both objects into independent read/write halves.
    let (mut a_r, mut a_w) = split(a);
    let (mut b_r, mut b_w) = split(b);

    // Create the two copy futures. They borrow disjoint halves, so creating
    // both futures is allowed.
    let f1 = copy(&mut a_r, &mut b_w);
    let f2 = copy(&mut b_r, &mut a_w);

    // Run both copies concurrently and await their results.
    let (res1, res2) = futures_util::future::join(f1, f2).await;

    // Propagate any errors; on success return the number of bytes copied for
    // each direction.
    let n1 = res1?;
    let n2 = res2?;
    Ok((n1, n2))
}
