//! Async I/O utilities.
//!
//! This module provides utility functions for async I/O operations:
//! - `copy()`: copy data from a reader to a writer.
//! - `split()`: split an I/O object into independent read/write halves.
//! - `copy_bidirectional()`: copy data in both directions between two I/O objects.
//!
//! # Examples
//!
//! See the executable "Copy through EOF" example in
//! `tools/vibeio-check/EXAMPLES.md`. [`copy`] handles partial writes, propagates
//! errors, and flushes the destination after the source reaches EOF.

use std::io;
use std::sync::Arc;

use futures_util::lock::Mutex as AsyncMutex;

use super::{AsyncRead, AsyncWrite};
use crate::vibeio::io::{IoBuf, IoBufMut, IoBufWithCursor};

/// Copy data from a reader to a writer.
///
/// This function reads from `reader` and writes to `writer` until EOF is reached.
/// Returns the number of bytes copied.
pub async fn copy<R, W>(reader: &mut R, writer: &mut W) -> Result<u64, io::Error>
where
    R: AsyncRead + ?Sized,
    W: AsyncWrite + ?Sized,
{
    let mut buffer = Vec::with_capacity(8192);
    let mut copied = 0u64;

    loop {
        let (read, mut returned_buf) = reader.read(buffer).await;
        let read = read?;

        if read > returned_buf.len() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "reader reported more bytes than its initialized buffer",
            ));
        }

        if read == 0 {
            break;
        }

        // Initialized storage may extend beyond the bytes read in this call.
        returned_buf.truncate(read);
        let mut cursor_buf = IoBufWithCursor::new(returned_buf);
        while cursor_buf.buf_len() > 0 {
            let remaining = cursor_buf.buf_len();
            let (w, mut returned_buf) = writer.write(cursor_buf).await;
            let w = w?;
            if w > remaining || w > returned_buf.buf_len() {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "writer reported more bytes than it was given",
                ));
            }
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
        buffer.clear();
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
/// A pending read prevents writes through the other half. Do not use this helper
/// for full-duplex protocols that need a write to unblock a pending read; use
/// poll-based streams with `tokio::io::split` or `copy_bidirectional` instead.
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
    async fn read<B: crate::vibeio::io::IoBufMut>(
        &mut self,
        buf: B,
    ) -> (Result<usize, io::Error>, B) {
        let mut guard = self.inner.lock().await;
        // Forward the call to the underlying object.
        (*guard).read(buf).await
    }
}

impl<T> AsyncWrite for WriteHalf<T>
where
    T: AsyncRead + AsyncWrite + 'static,
{
    async fn write<B: crate::vibeio::io::IoBuf>(
        &mut self,
        buf: B,
    ) -> (Result<usize, io::Error>, B) {
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
    async fn read<B: crate::vibeio::io::IoBufMut>(
        &mut self,
        buf: B,
    ) -> (Result<usize, std::io::Error>, B) {
        (**self).read(buf).await
    }
}

impl<R: AsyncRead + ?Sized> AsyncRead for &mut R {
    #[inline]
    async fn read<B: crate::vibeio::io::IoBufMut>(
        &mut self,
        buf: B,
    ) -> (Result<usize, std::io::Error>, B) {
        (**self).read(buf).await
    }
}

impl<W: AsyncWrite + ?Sized> AsyncWrite for Box<W> {
    #[inline]
    async fn write<B: crate::vibeio::io::IoBuf>(
        &mut self,
        buf: B,
    ) -> (Result<usize, std::io::Error>, B) {
        (**self).write(buf).await
    }

    #[inline]
    async fn flush(&mut self) -> Result<(), std::io::Error> {
        (**self).flush().await
    }
}

impl<W: AsyncWrite + ?Sized> AsyncWrite for &mut W {
    #[inline]
    async fn write<B: crate::vibeio::io::IoBuf>(
        &mut self,
        buf: B,
    ) -> (Result<usize, std::io::Error>, B) {
        (**self).write(buf).await
    }

    #[inline]
    async fn flush(&mut self) -> Result<(), std::io::Error> {
        (**self).flush().await
    }
}

/// Copy bytes concurrently in both directions, shutting down each destination's
/// write half when its source reaches EOF.
///
/// Uses poll-based I/O so a pending read never holds a whole-object mutex across
/// an await. PollTcpStream and PollUnixStream implement these Tokio-compatible
/// traits. Buffer-owning AsyncRead/AsyncWrite alone cannot guarantee full duplex.
///
/// Returns (a_to_b, b_to_a). An error in either direction ends the copy promptly.
pub async fn copy_bidirectional<A, B>(mut a: A, mut b: B) -> Result<(u64, u64), io::Error>
where
    A: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    B: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    tokio::io::copy_bidirectional(&mut a, &mut b).await
}

#[cfg(test)]
mod copy_tests {
    use super::*;
    use crate::vibeio::{driver::AnyDriver, executor::Runtime};

    struct Reader {
        count: usize,
        done: bool,
    }
    impl AsyncRead for Reader {
        async fn read<B: IoBufMut>(&mut self, mut buf: B) -> (io::Result<usize>, B) {
            if self.done {
                return (Ok(0), buf);
            }
            self.done = true;
            assert!(buf.buf_capacity() >= 5);
            // SAFETY: IoBufMut supplies exclusive writable capacity. All five
            // bytes are initialized before publishing that prefix's length.
            unsafe {
                buf.as_buf_mut_ptr()
                    .copy_from_nonoverlapping(b"abcXX".as_ptr(), 5);
                buf.set_buf_init(5);
            }
            (Ok(self.count), buf)
        }
    }

    #[derive(Default)]
    struct Writer {
        data: Vec<u8>,
        flushes: usize,
        invalid_count: Option<usize>,
    }
    impl AsyncWrite for Writer {
        async fn write<B: IoBuf>(&mut self, buf: B) -> (io::Result<usize>, B) {
            if let Some(count) = self.invalid_count {
                return (Ok(count), buf);
            }
            assert!(buf.buf_len() > 0);
            // SAFETY: IoBuf guarantees this nonempty prefix is initialized.
            self.data.push(unsafe { *buf.as_buf_ptr() });
            (Ok(1), buf)
        }
        async fn flush(&mut self) -> io::Result<()> {
            self.flushes += 1;
            Ok(())
        }
    }

    #[test]
    fn copy_uses_reported_read_count_and_handles_partial_writes() {
        Runtime::new(AnyDriver::new_mock()).block_on(async {
            let mut reader = Reader {
                count: 3,
                done: false,
            };
            let mut writer = Writer::default();
            assert_eq!(copy(&mut reader, &mut writer).await.unwrap(), 3);
            assert_eq!(writer.data, b"abc");
            assert_eq!(writer.flushes, 1);
        });
    }

    #[test]
    fn copy_rejects_invalid_progress_without_panicking_or_flushing() {
        Runtime::new(AnyDriver::new_mock()).block_on(async {
            for (read, write, expected) in [
                (6, None, io::ErrorKind::InvalidData),
                (3, Some(4), io::ErrorKind::InvalidData),
                (3, Some(0), io::ErrorKind::WriteZero),
            ] {
                let mut reader = Reader {
                    count: read,
                    done: false,
                };
                let mut writer = Writer {
                    invalid_count: write,
                    ..Writer::default()
                };
                assert_eq!(
                    copy(&mut reader, &mut writer).await.unwrap_err().kind(),
                    expected
                );
                assert!(writer.data.is_empty());
                assert_eq!(writer.flushes, 0);
            }
        });
    }
}

#[cfg(test)]
mod duplex_tests {
    use super::*;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    #[test]
    fn bidirectional_copy_supports_request_response_and_half_close() {
        let runtime =
            crate::vibeio::executor::Runtime::new(crate::vibeio::driver::AnyDriver::new_mock());
        runtime.block_on(async {
            // One-byte capacity forces backpressure and interleaved polls.
            let (mut client, a) = tokio::io::duplex(1);
            let (b, mut server) = tokio::io::duplex(1);
            let relay = copy_bidirectional(a, b);
            let client = async move {
                client.write_all(b"ping").await.unwrap();
                client.shutdown().await.unwrap();
                let mut response = Vec::new();
                client.read_to_end(&mut response).await.unwrap();
                assert_eq!(response, b"pong");
            };
            let server = async move {
                let mut request = Vec::new();
                server.read_to_end(&mut request).await.unwrap();
                assert_eq!(request, b"ping");
                server.write_all(b"pong").await.unwrap();
                server.shutdown().await.unwrap();
            };
            let (result, (), ()) =
                crate::vibeio::time::timeout(std::time::Duration::from_secs(2), async {
                    futures_util::join!(relay, client, server)
                })
                .await
                .unwrap();
            assert_eq!(result.unwrap(), (4, 4));
        });
    }

    struct ReadError;
    impl tokio::io::AsyncRead for ReadError {
        fn poll_read(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            _: &mut tokio::io::ReadBuf<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::task::Poll::Ready(Err(io::Error::other("injected read error")))
        }
    }
    impl tokio::io::AsyncWrite for ReadError {
        fn poll_write(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
            _: &[u8],
        ) -> std::task::Poll<io::Result<usize>> {
            std::task::Poll::Pending
        }
        fn poll_flush(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::task::Poll::Pending
        }
        fn poll_shutdown(
            self: std::pin::Pin<&mut Self>,
            _: &mut std::task::Context<'_>,
        ) -> std::task::Poll<io::Result<()>> {
            std::task::Poll::Pending
        }
    }

    #[test]
    fn bidirectional_error_does_not_wait_for_other_direction() {
        use std::future::Future;
        let (peer, endpoint) = tokio::io::duplex(1);
        let mut copy = Box::pin(copy_bidirectional(ReadError, endpoint));
        let result = copy
            .as_mut()
            .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()));
        assert!(
            matches!(result, std::task::Poll::Ready(Err(err)) if err.to_string() == "injected read error")
        );
        drop(peer);
    }
}
