//! Async I/O wrapper for interoperability with tokio traits.
//!
//! This module provides `AsyncWrap`, a type that adapts `vibeio`'s `AsyncRead`
//! and `AsyncWrite` traits to the `tokio::io` traits. This enables using
//! `vibeio` types with tokio-based libraries that expect the tokio I/O traits.
//!
//! The wrapper buffers read operations and ensures write operations complete
//! fully (like `write_all`), making it suitable for bridging async runtimes.
//!
//! # Implementation notes
//! - Read operations are buffered with a 4KB buffer size.
//! - Write operations are completed fully before returning, splitting the
//!   buffer if necessary.
//! - Concurrent operations are rejected with an error.
//! - The wrapper is `Unpin` regardless of the inner type.

use std::{
    future::Future,
    mem::MaybeUninit,
    pin::Pin,
    task::{Context, Poll},
};

use futures_util::FutureExt;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

type Buffer = Box<[u8]>;
const BUFFER_SIZE: usize = 4096;

/// A wrapper that adapts `vibeio`'s `AsyncRead`/`AsyncWrite` to `tokio::io` traits.
///
/// This type bridges the gap between `vibeio`'s async I/O traits and tokio's
/// `AsyncRead`/`AsyncWrite` traits, allowing `vibeio` types to be used with
/// tokio-based libraries.
///
/// # Examples
/// ```ignore
/// use tokio::io::{AsyncReadExt, AsyncWriteExt};
/// use vibeio::util::AsyncWrap;
///
/// // Wrap a vibeio async reader
/// let mut reader = some_vibeio_reader();
/// let mut wrap = AsyncWrap::new(reader);
///
/// let mut buf = Vec::new();
/// wrap.read_to_end(&mut buf).await?;  // tokio method
/// ```
pub struct AsyncWrap<T> {
    inner: Option<T>,
    read_buf: Option<(Buffer, usize, usize)>,
    #[allow(clippy::type_complexity)]
    read_fut: Option<Pin<Box<dyn Future<Output = (Result<usize, std::io::Error>, Buffer, T)>>>>,
    #[allow(clippy::type_complexity)]
    write_fut: Option<Pin<Box<dyn Future<Output = (Result<usize, std::io::Error>, T)>>>>,
    #[allow(clippy::type_complexity)]
    flush_fut: Option<Pin<Box<dyn Future<Output = (Result<(), std::io::Error>, T)>>>>,
}

impl<T> AsyncWrap<T> {
    /// Create a new `AsyncWrap` wrapping the given inner value.
    #[inline]
    pub fn new(inner: T) -> Self {
        Self {
            inner: Some(inner),
            read_buf: None,
            read_fut: None,
            write_fut: None,
            flush_fut: None,
        }
    }
}

impl<T> AsyncRead for AsyncWrap<T>
where
    T: crate::io::AsyncRead + 'static,
{
    #[inline]
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }

        let this = self.get_mut();

        if this.read_fut.is_none() {
            let buf_read = this.read_buf.take();
            if let Some((buf_read, advanced, n)) = buf_read {
                let unfilled =
                    unsafe { &mut *(buf.unfilled_mut() as *mut [MaybeUninit<u8>] as *mut [u8]) };
                let copy_len = (n - advanced).min(unfilled.len());
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        buf_read.as_ptr().add(advanced),
                        unfilled.as_mut_ptr(),
                        copy_len,
                    )
                };
                unsafe { buf.assume_init(copy_len) };
                buf.advance(copy_len);
                if advanced + copy_len < n {
                    this.read_buf = Some((buf_read, advanced + copy_len, n));
                }
                return Poll::Ready(Ok(()));
            }
            let buf = {
                let slice = Buffer::new_uninit_slice(BUFFER_SIZE);
                // SAFETY: u8 is a primitive type and has no padding, so
                // uninitialized memory is safe to read as a `Buffer`.
                unsafe { slice.assume_init() }
            };
            let Some(mut inner) = this.inner.take() else {
                return Poll::Ready(Err(std::io::Error::other(
                    "another operation is already in progress",
                )));
            };
            let fut = Box::pin(async move {
                let (read, buf) = crate::io::AsyncRead::read(&mut inner, buf).await;
                (read, buf, inner)
            });
            this.read_fut = Some(fut);
        }
        let read_fut = this.read_fut.as_mut().expect("read_fut is None");
        let (read, buf_read, inner) = futures_util::ready!(read_fut.poll_unpin(cx));
        this.read_fut = None;
        this.inner = Some(inner);
        match read {
            Ok(n) => {
                // Put buf_read into buf
                let unfilled =
                    unsafe { &mut *(buf.unfilled_mut() as *mut [MaybeUninit<u8>] as *mut [u8]) };
                let copy_len = n.min(unfilled.len());
                unsafe {
                    std::ptr::copy_nonoverlapping(
                        buf_read.as_ptr(),
                        unfilled.as_mut_ptr(),
                        copy_len,
                    )
                };
                unsafe { buf.assume_init(copy_len) };
                buf.advance(copy_len);
                if copy_len < n {
                    this.read_buf = Some((buf_read, copy_len, n));
                }
                Poll::Ready(Ok(()))
            }
            Err(e) => Poll::Ready(Err(e)),
        }
    }
}

impl<T> AsyncWrite for AsyncWrap<T>
where
    T: crate::io::AsyncWrite + 'static,
{
    #[inline]
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        if buf.is_empty() {
            return Poll::Ready(Ok(0));
        }

        let this = self.get_mut();

        if this.write_fut.is_none() {
            let buf: Vec<u8> = buf.into();
            let Some(mut inner) = this.inner.take() else {
                return Poll::Ready(Err(std::io::Error::other(
                    "another operation is already in progress",
                )));
            };
            let fut = Box::pin(async move {
                // write_all
                let mut buf = buf;
                let mut total_written = 0;
                while !buf.is_empty() {
                    let (written, mut buf_written) =
                        crate::io::AsyncWrite::write(&mut inner, buf).await;
                    match written {
                        Ok(n) => {
                            total_written += n;
                            buf = buf_written.split_off(n);
                        }
                        Err(e) => {
                            return (Err(e), inner);
                        }
                    }
                }
                (Ok(total_written), inner)
            });
            this.write_fut = Some(fut);
        }
        let write_fut = this.write_fut.as_mut().expect("read_fut is None");
        let (written, inner) = futures_util::ready!(write_fut.poll_unpin(cx));
        this.write_fut = None;
        this.inner = Some(inner);
        Poll::Ready(written)
    }

    #[inline]
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();

        if this.flush_fut.is_none() {
            let Some(mut inner) = this.inner.take() else {
                return Poll::Ready(Err(std::io::Error::other(
                    "another operation is already in progress",
                )));
            };
            let fut = Box::pin(async move {
                let flush = crate::io::AsyncWrite::flush(&mut inner).await;
                (flush, inner)
            });
            this.flush_fut = Some(fut);
        }
        let flush_fut = this.flush_fut.as_mut().expect("read_fut is None");
        let (flush, inner) = futures_util::ready!(flush_fut.poll_unpin(cx));
        this.flush_fut = None;
        this.inner = Some(inner);
        Poll::Ready(flush)
    }

    #[inline]
    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        // No-op
        Poll::Ready(Ok(()))
    }
}

impl<T> Unpin for AsyncWrap<T> {}

#[cfg(test)]
mod tests {
    use std::io;
    use std::pin::Pin;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};
    use std::task::{Context, Poll};

    use futures_util::task::noop_waker;
    use tokio::io::{
        AsyncRead as TokioAsyncRead, AsyncReadExt, AsyncWrite as TokioAsyncWrite, AsyncWriteExt,
        ReadBuf,
    };

    use super::AsyncWrap;
    use crate::io::{AsyncRead, AsyncWrite, IoBuf, IoBufMut};

    struct CountingReader {
        data: Vec<u8>,
        offset: usize,
        reads: Arc<AtomicUsize>,
    }

    impl CountingReader {
        fn new(data: &[u8], reads: Arc<AtomicUsize>) -> Self {
            Self {
                data: data.to_vec(),
                offset: 0,
                reads,
            }
        }
    }

    impl AsyncRead for CountingReader {
        async fn read<B: IoBufMut>(&mut self, mut buf: B) -> (Result<usize, io::Error>, B) {
            self.reads.fetch_add(1, Ordering::SeqCst);
            if self.offset >= self.data.len() {
                return (Ok(0), buf);
            }

            let remaining = self.data.len() - self.offset;
            let cap = buf.buf_capacity();
            let read_len = remaining.min(cap);

            unsafe {
                let ptr = buf.as_buf_mut_ptr();
                std::ptr::copy_nonoverlapping(self.data[self.offset..].as_ptr(), ptr, read_len);
                buf.set_buf_init(read_len);
            }

            self.offset += read_len;
            (Ok(read_len), buf)
        }
    }

    struct WriterState {
        data: Vec<u8>,
        writes: usize,
        flushed: bool,
    }

    struct ChunkedWriter {
        state: Arc<Mutex<WriterState>>,
        chunk_size: usize,
    }

    impl ChunkedWriter {
        fn new(state: Arc<Mutex<WriterState>>, chunk_size: usize) -> Self {
            Self { state, chunk_size }
        }
    }

    impl AsyncWrite for ChunkedWriter {
        async fn write<B: IoBuf>(&mut self, buf: B) -> (Result<usize, io::Error>, B) {
            let len = buf.buf_len();
            if len == 0 {
                return (Ok(0), buf);
            }

            let write_len = len.min(self.chunk_size.max(1));
            let slice = unsafe { std::slice::from_raw_parts(buf.as_buf_ptr(), write_len) };

            let mut guard = self.state.lock().expect("lock writer state");
            guard.writes += 1;
            guard.data.extend_from_slice(slice);

            (Ok(write_len), buf)
        }

        async fn flush(&mut self) -> Result<(), io::Error> {
            let mut guard = self.state.lock().expect("lock writer state");
            guard.flushed = true;
            Ok(())
        }
    }

    struct PendingIo;

    impl AsyncRead for PendingIo {
        async fn read<B: IoBufMut>(&mut self, buf: B) -> (Result<usize, io::Error>, B) {
            let _ = futures_util::future::pending::<()>().await;
            (Ok(0), buf)
        }
    }

    impl AsyncWrite for PendingIo {
        async fn write<B: IoBuf>(&mut self, buf: B) -> (Result<usize, io::Error>, B) {
            (Ok(0), buf)
        }

        async fn flush(&mut self) -> Result<(), io::Error> {
            Ok(())
        }
    }

    #[test]
    fn async_wrap_read_buffers_leftover() {
        let runtime = crate::executor::Runtime::new(crate::driver::AnyDriver::new_mock());
        runtime.block_on(async {
            let reads = Arc::new(AtomicUsize::new(0));
            let reader = CountingReader::new(b"abcdefghij", reads.clone());
            let mut wrap = AsyncWrap::new(reader);

            let mut buf1 = [0u8; 3];
            let n1 = wrap.read(&mut buf1).await.expect("read should succeed");
            assert_eq!(n1, 3);
            assert_eq!(&buf1[..n1], b"abc");

            let mut buf2 = [0u8; 4];
            let n2 = wrap.read(&mut buf2).await.expect("read should succeed");
            assert_eq!(n2, 4);
            assert_eq!(&buf2[..n2], b"defg");

            let mut buf3 = [0u8; 4];
            let n3 = wrap.read(&mut buf3).await.expect("read should succeed");
            assert_eq!(n3, 3);
            assert_eq!(&buf3[..n3], b"hij");

            assert_eq!(reads.load(Ordering::SeqCst), 1);
        });
    }

    #[test]
    fn async_wrap_write_writes_all_and_flushes() {
        let runtime = crate::executor::Runtime::new(crate::driver::AnyDriver::new_mock());
        runtime.block_on(async {
            let state = Arc::new(Mutex::new(WriterState {
                data: Vec::new(),
                writes: 0,
                flushed: false,
            }));
            let writer = ChunkedWriter::new(state.clone(), 2);
            let mut wrap = AsyncWrap::new(writer);

            let payload = b"hello world";
            let n = wrap.write(payload).await.expect("write should succeed");
            assert_eq!(n, payload.len());
            wrap.flush().await.expect("flush should succeed");

            let guard = state.lock().expect("lock writer state");
            assert_eq!(guard.data, payload);
            assert!(guard.flushed);
            assert!(guard.writes > 1);
        });
    }

    #[test]
    fn async_wrap_rejects_concurrent_operations() {
        let mut wrap = AsyncWrap::new(PendingIo);
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        let mut buf = [0u8; 8];
        let mut read_buf = ReadBuf::new(&mut buf);
        let poll = Pin::new(&mut wrap).poll_read(&mut cx, &mut read_buf);
        assert!(matches!(poll, Poll::Pending));

        let poll_write: Poll<io::Result<usize>> = Pin::new(&mut wrap).poll_write(&mut cx, b"hi");
        match poll_write {
            Poll::Ready(Err(err)) => {
                assert_eq!(err.kind(), io::ErrorKind::Other);
            }
            _ => panic!("expected concurrent write to return an error"),
        }
    }
}
