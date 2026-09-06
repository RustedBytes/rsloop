//! Async I/O wrapper for interoperability with tokio traits.
//!
//! This module provides `AsyncWrap`, a type that adapts `vibeio`'s `AsyncRead`
//! and `AsyncWrite` traits to the `tokio::io` traits. This enables using
//! `vibeio` types with tokio-based libraries that expect the tokio I/O traits.
//!
//! The wrapper buffers reads and accepts writes into bounded owned storage.
//! Flush (or shut down) explicitly to drain accepted bytes to the inner writer.
//!
//! # Implementation notes
//! - Read operations are buffered with a 4KB buffer size.
//! - Writes accept at most 4KB per call; subsequent writes drain the previous batch.
//! - A pending write accepts no bytes from its caller, so cancellation and a
//!   different buffer on the next call cannot misattribute an old completion.
//! - Errors writing accepted bytes are reported by the next write, read, flush,
//!   or shutdown that drains them. Dropping the wrapper does not flush.
//! - A failed write drain is terminal: later delivery operations return the same
//!   error kind rather than acknowledging more bytes after data was discarded.
//! - Concurrent operations are rejected with an error.
//! - The wrapper is `Unpin` regardless of the inner type.

use std::{
    future::Future,
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
/// An in-flight operation owns the inner object until completion. This adapter
/// does not support concurrent full-duplex operations; prefer native poll-based
/// stream types for that use case.
/// Writes are buffered: successful counts acknowledge owned bytes, not completed
/// kernel writes. Call flush or shutdown before drop to finish delivery. Shutdown
/// flushes but cannot half-close the inner stream: the buffer-owning trait has no
/// shutdown operation.
/// A write-drain error permanently fails the adapter. The first error retains
/// its original details; subsequent operations that drain writes return its
/// error kind. Interrupted writes are retried internally and do not fail it.
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
    write_error: Option<std::io::ErrorKind>,
    read_buf: Option<(Buffer, usize, usize)>,
    #[allow(
        clippy::type_complexity,
        reason = "Spell out the owned read state returned by the in-flight future"
    )]
    read_fut: Option<Pin<Box<dyn Future<Output = (Result<usize, std::io::Error>, Buffer, T)>>>>,
    #[allow(
        clippy::type_complexity,
        reason = "Spell out the owned write state returned by the in-flight future"
    )]
    write_fut: Option<Pin<Box<dyn Future<Output = (Result<(), std::io::Error>, T)>>>>,
    #[allow(
        clippy::type_complexity,
        reason = "Spell out the owned flush state returned by the in-flight future"
    )]
    flush_fut: Option<Pin<Box<dyn Future<Output = (Result<(), std::io::Error>, T)>>>>,
}

impl<T> AsyncWrap<T> {
    fn poll_pending_write(&mut self, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        if let Some(kind) = self.write_error {
            return Poll::Ready(Err(std::io::Error::new(
                kind,
                "a previous write drain failed; accepted data could not be delivered",
            )));
        }
        let Some(future) = self.write_fut.as_mut() else {
            return Poll::Ready(Ok(()));
        };
        let (result, inner) = futures_util::ready!(future.poll_unpin(cx));
        self.write_fut = None;
        self.inner = Some(inner);
        if let Err(error) = &result {
            self.write_error = Some(error.kind());
        }
        Poll::Ready(result)
    }

    fn poll_pending_flush(&mut self, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let Some(future) = self.flush_fut.as_mut() else {
            return Poll::Ready(Ok(()));
        };
        let (result, inner) = futures_util::ready!(future.poll_unpin(cx));
        self.flush_fut = None;
        self.inner = Some(inner);
        Poll::Ready(result)
    }

    /// Create a new `AsyncWrap` wrapping the given inner value.
    #[inline]
    pub fn new(inner: T) -> Self {
        Self {
            inner: Some(inner),
            write_error: None,
            read_buf: None,
            read_fut: None,
            write_fut: None,
            flush_fut: None,
        }
    }
}

impl<T> AsyncRead for AsyncWrap<T>
where
    T: crate::vibeio::io::AsyncRead + 'static,
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
            futures_util::ready!(this.poll_pending_flush(cx))?;
            futures_util::ready!(this.poll_pending_write(cx))?;
            let buf_read = this.read_buf.take();
            if let Some((buf_read, advanced, n)) = buf_read {
                let copy_len = (n - advanced).min(buf.remaining());
                buf.put_slice(&buf_read[advanced..advanced + copy_len]);
                if advanced + copy_len < n {
                    this.read_buf = Some((buf_read, advanced + copy_len, n));
                }
                return Poll::Ready(Ok(()));
            }
            // `Box<[u8]>` represents initialized bytes. Turning an
            // uninitialized allocation into one would violate the validity
            // invariant of `u8`, even if the reader is expected to overwrite
            // it before it is observed.
            let buf = vec![0; BUFFER_SIZE].into_boxed_slice();
            let Some(mut inner) = this.inner.take() else {
                return Poll::Ready(Err(std::io::Error::other(
                    "another operation is already in progress",
                )));
            };
            let fut = Box::pin(async move {
                let (read, buf) = crate::vibeio::io::AsyncRead::read(&mut inner, buf).await;
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
                if n > buf_read.len() {
                    return Poll::Ready(Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "reader returned more bytes than the supplied buffer can hold",
                    )));
                }
                let copy_len = n.min(buf.remaining());
                buf.put_slice(&buf_read[..copy_len]);
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
    T: crate::vibeio::io::AsyncWrite + 'static,
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
        futures_util::ready!(this.poll_pending_flush(cx))?;
        futures_util::ready!(this.poll_pending_write(cx))?;
        {
            let accepted = buf.len().min(BUFFER_SIZE);
            let buf = buf[..accepted].to_vec();
            let Some(mut inner) = this.inner.take() else {
                return Poll::Ready(Err(std::io::Error::other(
                    "another operation is already in progress",
                )));
            };
            let fut = Box::pin(async move {
                use crate::vibeio::io::{IoBuf, IoBufWithCursor};
                let mut buf = IoBufWithCursor::new(buf);
                while buf.buf_len() > 0 {
                    let supplied = buf.buf_len();
                    let (written, mut returned) =
                        crate::vibeio::io::AsyncWrite::write(&mut inner, buf).await;
                    let count = match written {
                        Err(error) if error.kind() == std::io::ErrorKind::Interrupted => {
                            // These bytes were already acknowledged to the caller.
                            // Retain the returned buffer and retry at the same cursor.
                            buf = returned;
                            continue;
                        }
                        Err(error) => return (Err(error), inner),
                        Ok(0) => return (Err(std::io::ErrorKind::WriteZero.into()), inner),
                        Ok(count) if count > supplied || count > returned.buf_len() => {
                            return (Err(std::io::ErrorKind::InvalidData.into()), inner);
                        }
                        Ok(count) => count,
                    };
                    returned.advance(count);
                    buf = returned;
                }
                (Ok(()), inner)
            });
            this.write_fut = Some(fut);
            // This batch is accepted now, not when its future completes. A
            // later Pending poll consumes no bytes from that later caller.
            Poll::Ready(Ok(accepted))
        }
    }

    #[inline]
    fn poll_flush(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        futures_util::ready!(this.poll_pending_write(cx))?;

        if this.flush_fut.is_none() {
            let Some(mut inner) = this.inner.take() else {
                return Poll::Ready(Err(std::io::Error::other(
                    "another operation is already in progress",
                )));
            };
            let fut = Box::pin(async move {
                let flush = crate::vibeio::io::AsyncWrite::flush(&mut inner).await;
                (flush, inner)
            });
            this.flush_fut = Some(fut);
        }
        this.poll_pending_flush(cx)
    }

    #[inline]
    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        self.poll_flush(cx)
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
    use crate::vibeio::io::{AsyncRead, AsyncWrite, IoBuf, IoBufMut};

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

    struct InterruptedWriter {
        inner: ChunkedWriter,
        attempts: Arc<AtomicUsize>,
    }

    impl AsyncWrite for InterruptedWriter {
        async fn write<B: IoBuf>(&mut self, buf: B) -> (io::Result<usize>, B) {
            if self
                .attempts
                .fetch_add(1, Ordering::SeqCst)
                .is_multiple_of(2)
            {
                (Err(io::ErrorKind::Interrupted.into()), buf)
            } else {
                self.inner.write(buf).await
            }
        }

        async fn flush(&mut self) -> io::Result<()> {
            self.inner.flush().await
        }
    }

    #[test]
    fn async_wrap_retries_interrupted_writes_without_losing_accepted_bytes() {
        let runtime =
            crate::vibeio::executor::Runtime::new(crate::vibeio::driver::AnyDriver::new_mock());
        runtime.block_on(async {
            let state = Arc::new(Mutex::new(WriterState {
                data: Vec::new(),
                writes: 0,
                flushed: false,
            }));
            let attempts = Arc::new(AtomicUsize::new(0));
            let mut wrap = AsyncWrap::new(InterruptedWriter {
                inner: ChunkedWriter::new(state.clone(), 2),
                attempts: attempts.clone(),
            });
            wrap.write_all(b"abcdef").await.unwrap();
            wrap.flush().await.unwrap();
            let state = state.lock().unwrap();
            assert_eq!(state.data, b"abcdef");
            assert_eq!(state.writes, 3);
            assert!(state.flushed);
            assert_eq!(attempts.load(Ordering::SeqCst), 6);
        });
    }

    impl AsyncRead for PendingIo {
        async fn read<B: IoBufMut>(&mut self, buf: B) -> (Result<usize, io::Error>, B) {
            let _ = futures_util::future::pending::<()>().await;
            (Ok(0), buf)
        }
    }

    struct ZeroWriter;

    impl AsyncWrite for ZeroWriter {
        async fn write<B: IoBuf>(&mut self, buf: B) -> (Result<usize, io::Error>, B) {
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
        let runtime =
            crate::vibeio::executor::Runtime::new(crate::vibeio::driver::AnyDriver::new_mock());
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
    fn async_wrap_reads_into_uninitialized_storage_and_preserves_prefix() {
        let reads = Arc::new(AtomicUsize::new(0));
        let mut wrap = AsyncWrap::new(CountingReader::new(b"abcdef", reads.clone()));
        let waker = noop_waker();
        let mut cx = Context::from_waker(&waker);

        // Exercise both the newly completed read and the cached remainder.
        for expected in [b"!abc", b"!def"] {
            let mut storage = [std::mem::MaybeUninit::uninit(); 4];
            let mut buf = ReadBuf::uninit(&mut storage);
            buf.put_slice(b"!");
            assert!(matches!(
                Pin::new(&mut wrap).poll_read(&mut cx, &mut buf),
                Poll::Ready(Ok(()))
            ));
            assert_eq!(buf.filled(), expected);
            assert_eq!(buf.initialized().len(), 4);
        }
        assert_eq!(reads.load(Ordering::SeqCst), 1);

        let mut storage = [std::mem::MaybeUninit::uninit(); 4];
        let mut buf = ReadBuf::uninit(&mut storage);
        assert!(matches!(
            Pin::new(&mut wrap).poll_read(&mut cx, &mut buf),
            Poll::Ready(Ok(()))
        ));
        assert!(buf.filled().is_empty());
        assert!(buf.initialized().is_empty());
    }

    #[test]
    fn async_wrap_write_writes_all_and_flushes() {
        let runtime =
            crate::vibeio::executor::Runtime::new(crate::vibeio::driver::AnyDriver::new_mock());
        runtime.block_on(async {
            let state = Arc::new(Mutex::new(WriterState {
                data: Vec::new(),
                writes: 0,
                flushed: false,
            }));
            let writer = ChunkedWriter::new(state.clone(), 2);
            let mut wrap = AsyncWrap::new(writer);

            let payload: Vec<u8> = (0..super::BUFFER_SIZE * 2 + 7)
                .map(|index| (index % 251) as u8)
                .collect();
            wrap.write_all(&payload)
                .await
                .expect("write_all should succeed");
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

    #[test]
    fn async_wrap_reports_write_zero() {
        let runtime =
            crate::vibeio::executor::Runtime::new(crate::vibeio::driver::AnyDriver::new_mock());
        runtime.block_on(async {
            let mut wrap = AsyncWrap::new(ZeroWriter);
            wrap.write_all(b"data").await.unwrap();
            let err = wrap
                .flush()
                .await
                .expect_err("a zero-length write must not be retried forever");
            assert_eq!(err.kind(), io::ErrorKind::WriteZero);
        });
    }

    struct PartialThenError {
        calls: usize,
    }

    impl AsyncWrite for PartialThenError {
        async fn write<B: IoBuf>(&mut self, buf: B) -> (io::Result<usize>, B) {
            self.calls += 1;
            if self.calls == 1 {
                (Ok(1), buf)
            } else {
                (Err(io::ErrorKind::BrokenPipe.into()), buf)
            }
        }
    }

    #[test]
    fn async_wrap_reports_accepted_bytes_before_a_later_drain_error() {
        let mut wrap = AsyncWrap::new(PartialThenError { calls: 0 });
        let mut cx = Context::from_waker(std::task::Waker::noop());
        assert!(matches!(
            Pin::new(&mut wrap).poll_write(&mut cx, b"abc"),
            Poll::Ready(Ok(3))
        ));
        assert!(matches!(Pin::new(&mut wrap).poll_write(&mut cx, b"bc"),
            Poll::Ready(Err(error)) if error.kind() == io::ErrorKind::BrokenPipe));
    }

    #[test]
    fn async_wrap_drain_failure_cannot_be_followed_by_successful_delivery() {
        for kind in [
            io::ErrorKind::BrokenPipe,
            io::ErrorKind::WriteZero,
            io::ErrorKind::InvalidData,
        ] {
            struct FailingWriter(io::ErrorKind);
            impl AsyncWrite for FailingWriter {
                async fn write<B: IoBuf>(&mut self, buf: B) -> (io::Result<usize>, B) {
                    let result = match self.0 {
                        io::ErrorKind::WriteZero => Ok(0),
                        io::ErrorKind::InvalidData => Ok(buf.buf_len() + 1),
                        kind => Err(kind.into()),
                    };
                    (result, buf)
                }
            }
            impl AsyncRead for FailingWriter {
                async fn read<B: IoBufMut>(&mut self, _buf: B) -> (io::Result<usize>, B) {
                    panic!("a read must report the failed drain before reaching the inner reader")
                }
            }
            let mut wrap = AsyncWrap::new(FailingWriter(kind));
            let mut cx = Context::from_waker(std::task::Waker::noop());
            assert!(matches!(
                Pin::new(&mut wrap).poll_write(&mut cx, b"accepted"),
                Poll::Ready(Ok(8))
            ));
            for _ in 0..2 {
                assert!(
                    matches!(Pin::new(&mut wrap).poll_flush(&mut cx), Poll::Ready(Err(error)) if error.kind() == kind)
                );
                assert!(
                    matches!(Pin::new(&mut wrap).poll_shutdown(&mut cx), Poll::Ready(Err(error)) if error.kind() == kind)
                );
                assert!(
                    matches!(Pin::new(&mut wrap).poll_write(&mut cx, b"later"), Poll::Ready(Err(error)) if error.kind() == kind)
                );
                let mut storage = [0; 1];
                let mut read_buf = ReadBuf::new(&mut storage);
                assert!(
                    matches!(Pin::new(&mut wrap).poll_read(&mut cx, &mut read_buf), Poll::Ready(Err(error)) if error.kind() == kind)
                );
                assert!(read_buf.filled().is_empty());
            }
        }
    }

    #[test]
    fn async_wrap_bounds_accepted_buffer_size() {
        let mut wrap = AsyncWrap::new(ZeroWriter);
        let mut cx = Context::from_waker(std::task::Waker::noop());
        assert!(matches!(
            Pin::new(&mut wrap).poll_write(&mut cx, &[0; super::BUFFER_SIZE + 1]),
            Poll::Ready(Ok(super::BUFFER_SIZE))
        ));
    }

    struct GatedWriter {
        enabled: Arc<std::sync::atomic::AtomicBool>,
        state: Arc<Mutex<WriterState>>,
    }

    impl AsyncWrite for GatedWriter {
        async fn write<B: IoBuf>(&mut self, buf: B) -> (io::Result<usize>, B) {
            std::future::poll_fn(|_| {
                // Test polls explicitly after opening the gate; no task sleeps.
                if self.enabled.load(Ordering::SeqCst) {
                    Poll::Ready(())
                } else {
                    Poll::Pending
                }
            })
            .await;
            // SAFETY: each invocation has at least one initialized byte.
            self.state
                .lock()
                .unwrap()
                .data
                .push(unsafe { *buf.as_buf_ptr() });
            (Ok(1), buf)
        }
        async fn flush(&mut self) -> io::Result<()> {
            self.state.lock().unwrap().flushed = true;
            Ok(())
        }
    }

    #[test]
    fn cancelled_pending_write_does_not_consume_replacement_buffer() {
        let enabled = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let state = Arc::new(Mutex::new(WriterState {
            data: Vec::new(),
            writes: 0,
            flushed: false,
        }));
        let mut wrap = AsyncWrap::new(GatedWriter {
            enabled: enabled.clone(),
            state: state.clone(),
        });
        let mut cx = Context::from_waker(std::task::Waker::noop());
        assert!(matches!(
            Pin::new(&mut wrap).poll_write(&mut cx, b"old"),
            Poll::Ready(Ok(3))
        ));
        assert!(Pin::new(&mut wrap).poll_flush(&mut cx).is_pending());
        assert!(
            Pin::new(&mut wrap)
                .poll_write(&mut cx, b"discarded")
                .is_pending()
        );
        // Cancel the pending caller and use a smaller, different buffer.
        enabled.store(true, Ordering::SeqCst);
        assert!(matches!(
            Pin::new(&mut wrap).poll_write(&mut cx, b"z"),
            Poll::Ready(Ok(1))
        ));
        assert_eq!(state.lock().unwrap().data, b"old");
        assert!(matches!(
            Pin::new(&mut wrap).poll_shutdown(&mut cx),
            Poll::Ready(Ok(()))
        ));
        let state = state.lock().unwrap();
        assert_eq!(state.data, b"oldz");
        assert!(state.flushed);
    }
}
