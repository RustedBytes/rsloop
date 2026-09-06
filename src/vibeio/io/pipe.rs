//! Async pipe utilities.
#![warn(clippy::undocumented_unsafe_blocks)]
//!
//! This module provides async-aware pipe endpoints:
//! - `pipe()`: create a pair of async-aware pipe endpoints.
//! - `Pipe`: a pipe endpoint for async I/O.
//! - `PollPipe`: a variant that uses readiness-based polling.
//!
//! # Examples
//!
//! See the executable "Pipe buffer ownership" example in
//! `tools/vibeio-check/EXAMPLES.md`. Reads return the owned buffer alongside
//! their result; inspect that returned buffer, not an original array copy.

use std::future::poll_fn;
use std::io::{self, IoSlice};
use std::os::fd::OwnedFd;
use std::os::fd::{AsRawFd, IntoRawFd, RawFd};
use std::pin::Pin;
use std::task::{Context, Poll};

use mio::Interest;
use tokio::io::{AsyncRead as TokioAsyncRead, AsyncWrite as TokioAsyncWrite, ReadBuf};

use crate::vibeio::io::{
    AsInnerRawHandle, IoBuf, IoBufMut, IoBufTemporaryPoll, IoVectoredBuf, IoVectoredBufMut,
    IoVectoredBufTemporaryPoll,
};
use crate::vibeio::op::{ReadOp, ReadvOp, WriteOp, WritevOp};
use crate::vibeio::{
    driver::RegistrationMode,
    fd_inner::{InnerRawHandle, set_nonblocking},
    io::{AsyncRead, AsyncWrite},
};

fn pipe_inner() -> std::io::Result<(OwnedFd, OwnedFd)> {
    let (reader, writer) = std::io::pipe()?;
    Ok((reader.into(), writer.into()))
}

#[cfg(test)]
mod setup_tests {
    use super::*;
    use crate::vibeio::{driver::AnyDriver, executor::Runtime};

    #[test]
    fn pending_poll_writes_do_not_retain_borrowed_buffers() {
        Runtime::new(AnyDriver::new_mio().unwrap()).block_on(async {
            let (reader, writer) = pipe().unwrap();
            let mut reader = reader.into_poll().unwrap();
            let mut writer = writer.into_poll().unwrap();
            let mut cx = Context::from_waker(std::task::Waker::noop());
            let mut data = vec![b'a'; 4096];
            let mut filled = 0;
            loop {
                match Pin::new(&mut writer).poll_write(&mut cx, &data) {
                    Poll::Ready(Ok(count)) => {
                        assert!(count > 0);
                        filled += count;
                        assert!(
                            filled < 16 * 1024 * 1024,
                            "pipe failed to apply backpressure"
                        );
                    }
                    Poll::Pending => break,
                    Poll::Ready(Err(error)) => panic!("pipe fill failed: {error}"),
                }
            }
            let vectors = [IoSlice::new(&data), IoSlice::new(&data)];
            assert!(
                Pin::new(&mut writer)
                    .poll_write_vectored(&mut cx, &vectors)
                    .is_pending()
            );
            // Both temporary operations have returned. These borrows may now
            // end without waiting for readiness or a completion packet.
            data.fill(b'x');
            drop(data);
            let mut drained = 0;
            loop {
                let mut storage = [0; 4096];
                let mut buf = ReadBuf::new(&mut storage);
                match Pin::new(&mut reader).poll_read(&mut cx, &mut buf) {
                    Poll::Ready(Ok(())) => {
                        assert!(!buf.filled().is_empty());
                        assert!(buf.filled().iter().all(|byte| *byte == b'a'));
                        drained += buf.filled().len();
                        assert!(drained <= filled);
                    }
                    Poll::Pending => break,
                    Poll::Ready(Err(error)) => panic!("pipe drain failed: {error}"),
                }
            }
            assert_eq!(drained, filled);
            let fresh = [IoSlice::new(b"new"), IoSlice::new(b"data")];
            assert!(matches!(
                Pin::new(&mut writer).poll_write_vectored(&mut cx, &fresh),
                Poll::Ready(Ok(7))
            ));
            let mut storage = [0; 7];
            let mut buf = ReadBuf::new(&mut storage);
            assert!(matches!(
                Pin::new(&mut reader).poll_read(&mut cx, &mut buf),
                Poll::Ready(Ok(()))
            ));
            assert_eq!(buf.filled(), b"newdata");
        });
    }

    #[test]
    fn raw_pipe_conversion_releases_registration_and_transfers_live_endpoints() {
        use std::io::{Read, Write};
        use std::os::fd::FromRawFd;
        let mut driver = AnyDriver::new_mock();
        let AnyDriver::Mock(mock) = &mut driver else {
            unreachable!()
        };
        mock.registrations = Some(Default::default());
        mock.registrations
            .as_ref()
            .unwrap()
            .results
            .borrow_mut()
            .extend([Ok(mio::Token(0)), Ok(mio::Token(1))]);
        Runtime::new(driver).block_on(async {
            let (reader, writer) = pipe().unwrap();
            let read_fd = reader.as_raw_fd();
            let write_fd = writer.as_raw_fd();
            let reader = reader.into_raw_fd();
            // SAFETY: into_raw_fd transferred sole ownership of a live fd.
            let reader = unsafe { OwnedFd::from_raw_fd(reader) };
            let writer = writer.into_raw_fd();
            // SAFETY: into_raw_fd transferred sole ownership of a live fd.
            let writer = unsafe { OwnedFd::from_raw_fd(writer) };
            assert_eq!(reader.as_raw_fd(), read_fd);
            assert_eq!(writer.as_raw_fd(), write_fd);
            let mut reader = std::fs::File::from(reader);
            let mut writer = std::fs::File::from(writer);
            writer.write_all(b"pipe").unwrap();
            drop(writer);
            let mut data = Vec::new();
            reader.read_to_end(&mut data).unwrap();
            assert_eq!(data, b"pipe");
            drop(reader);
            let driver = crate::vibeio::executor::current_driver().unwrap();
            let AnyDriver::Mock(mock) = driver.as_ref() else {
                unreachable!()
            };
            assert_eq!(
                *mock.registrations.as_ref().unwrap().deregistered.borrow(),
                [mio::Token(0), mio::Token(1)]
            );
        });
    }

    #[test]
    fn pipe_endpoints_are_close_on_exec() {
        let (reader, writer) = pipe_inner().unwrap();
        for fd in [reader.as_raw_fd(), writer.as_raw_fd()] {
            // SAFETY: F_GETFD queries the live owned endpoint without pointers.
            let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
            assert_ne!(flags, -1);
            assert_ne!(flags & libc::FD_CLOEXEC, 0);
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn pipe_endpoints_are_not_inherited_across_exec() {
        const CHILD_FDS: &str = "VIBEIO_PIPE_EXEC_TEST_FDS";
        if let Ok(endpoints) = std::env::var(CHILD_FDS) {
            for endpoint in endpoints.split(',') {
                let (fd, original_target) = endpoint.split_once('=').unwrap();
                let target = std::fs::read_link(format!("/proc/self/fd/{fd}"));
                // The test runner may reuse the numeric descriptor, but it must
                // not retain this pipe (which is still alive in the parent).
                match target {
                    Ok(target) => assert_ne!(target, std::path::Path::new(original_target)),
                    Err(error) => assert_eq!(error.kind(), io::ErrorKind::NotFound),
                }
            }
            return;
        }
        let (reader, writer) = pipe_inner().unwrap();
        let endpoints = [reader.as_raw_fd(), writer.as_raw_fd()]
            .map(|fd| {
                let target = std::fs::read_link(format!("/proc/self/fd/{fd}")).unwrap();
                format!("{fd}={}", target.display())
            })
            .join(",");
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "vibeio::io::pipe::setup_tests::pipe_endpoints_are_not_inherited_across_exec",
            ])
            .env(CHILD_FDS, endpoints)
            .status()
            .unwrap();
        assert!(status.success());
    }

    #[test]
    fn pipe_roundtrip_and_mode_conversion_on_readiness_driver() {
        Runtime::new(AnyDriver::new_mio().unwrap()).block_on(async {
            let (reader, mut writer) = pipe().unwrap();
            let mut reader = reader.into_poll().unwrap().into_completion().unwrap();
            // Completion requests fall back to poll mode on this driver.
            assert!(!reader.handle.uses_completion());
            for fd in [reader.as_raw_fd(), writer.as_raw_fd()] {
                // SAFETY: both pipe endpoints are live; F_GETFL has no pointers.
                let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
                assert_ne!(flags, -1);
                assert_ne!(flags & libc::O_NONBLOCK, 0);
            }
            assert_eq!(writer.write(b"abc".to_vec()).await.0.unwrap(), 3);
            let (result, buffer) = reader.read(vec![0; 3]).await;
            assert_eq!(result.unwrap(), 3);
            assert_eq!(buffer, b"abc");
        });
    }
}

/// Create a new async-aware pipe.
///
/// Returns a tuple of `(reader, writer)` pipe endpoints.
/// Both endpoints are close-on-exec to prevent unintended child inheritance.
pub fn pipe() -> std::io::Result<(Pipe, Pipe)> {
    let (read, write) = pipe_inner()?;
    Ok((
        Pipe::from_std_with_mode(read, RegistrationMode::Completion)?,
        Pipe::from_std_with_mode(write, RegistrationMode::Completion)?,
    ))
}

/// A pipe endpoint that can use either completion or readiness-based I/O.
pub struct Pipe {
    // Deregister before closing the descriptor (fields drop in declaration order).
    handle: InnerRawHandle,
    inner: OwnedFd,
}

/// A poll-only variant that always uses readiness-based operations.
pub struct PollPipe {
    stream: Pipe,
}

impl Pipe {
    /// Create a `Pipe` from a standard library `OwnedFd` with the given registration mode.
    #[inline]
    pub(crate) fn from_std_with_mode(
        inner: OwnedFd,
        mode: RegistrationMode,
    ) -> Result<Self, io::Error> {
        #[cfg(unix)]
        let handle = InnerRawHandle::new_with_mode(
            inner.as_raw_fd(),
            Interest::READABLE | Interest::WRITABLE,
            mode,
        )?;
        set_nonblocking(inner.as_raw_fd(), !handle.uses_completion())?;
        Ok(Self { inner, handle })
    }

    /// Convert this `Pipe` to a `PollPipe` for readiness-based operations.
    #[inline]
    pub fn into_poll(self) -> Result<PollPipe, io::Error> {
        let mut stream = self;
        stream.handle.rebind_mode(RegistrationMode::Poll)?;
        set_nonblocking(stream.inner.as_raw_fd(), !stream.handle.uses_completion())?;
        Ok(PollPipe { stream })
    }
}

impl PollPipe {
    /// Convert this `PollPipe` back to an adaptive `Pipe`.
    #[inline]
    pub fn into_adaptive(self) -> Pipe {
        self.stream
    }

    /// Convert this `PollPipe` to a completion-based `Pipe`.
    #[inline]
    pub fn into_completion(self) -> Result<Pipe, io::Error> {
        let mut stream = self.stream;
        stream.handle.rebind_mode(RegistrationMode::Completion)?;
        set_nonblocking(stream.inner.as_raw_fd(), !stream.handle.uses_completion())?;
        Ok(stream)
    }
}

impl AsRawFd for Pipe {
    #[inline]
    fn as_raw_fd(&self) -> RawFd {
        self.inner.as_raw_fd()
    }
}

impl AsRawFd for PollPipe {
    #[inline]
    fn as_raw_fd(&self) -> RawFd {
        self.stream.inner.as_raw_fd()
    }
}

impl IntoRawFd for Pipe {
    #[inline]
    fn into_raw_fd(self) -> RawFd {
        let Self { handle, inner } = self;
        drop(handle);
        inner.into_raw_fd()
    }
}

impl IntoRawFd for PollPipe {
    #[inline]
    fn into_raw_fd(self) -> RawFd {
        self.stream.into_raw_fd()
    }
}

impl<'a> AsInnerRawHandle<'a> for Pipe {
    #[inline]
    fn as_inner_raw_handle(&'a self) -> &'a InnerRawHandle {
        &self.handle
    }
}

impl<'a> AsInnerRawHandle<'a> for PollPipe {
    #[inline]
    fn as_inner_raw_handle(&'a self) -> &'a InnerRawHandle {
        self.stream.as_inner_raw_handle()
    }
}

impl AsyncRead for Pipe {
    #[inline]
    async fn read<B: IoBufMut>(&mut self, buf: B) -> (Result<usize, io::Error>, B) {
        let handle = &self.handle;
        let mut op = ReadOp::new(handle, buf);
        let result = poll_fn(|cx| handle.poll_op(cx, &mut op)).await;
        (result, op.take_bufs())
    }

    #[inline]
    async fn read_vectored<B: IoVectoredBufMut>(
        &mut self,
        bufs: B,
    ) -> (Result<usize, io::Error>, B) {
        if bufs.is_empty() {
            return (Ok(0), bufs);
        }
        let handle = &self.handle;
        let mut op = ReadvOp::new(handle, bufs);
        let result = poll_fn(|cx| handle.poll_op(cx, &mut op)).await;
        (result, op.take_bufs())
    }
}

impl TokioAsyncRead for PollPipe {
    #[inline]
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<Result<(), io::Error>> {
        if buf.remaining() == 0 {
            return Poll::Ready(Ok(()));
        }

        let this = self.get_mut();
        // SAFETY: only a raw pointer is passed to the synchronous read below;
        // no initialized-byte reference is formed and no pointer is retained.
        let unfilled = unsafe { buf.unfilled_mut() };
        // SAFETY: ReadBuf exclusively owns this writable region for this poll.
        let buf_temp =
            unsafe { IoBufTemporaryPoll::new_uninit(unfilled.as_mut_ptr().cast(), unfilled.len()) };
        let mut op = ReadOp::new(&this.stream.handle, buf_temp);
        match this.stream.handle.poll_op_poll(cx, &mut op) {
            Poll::Ready(Ok(read)) => {
                // SAFETY: the successful read initialized exactly this prefix.
                unsafe {
                    buf.assume_init(read);
                }
                buf.advance(read);
                Poll::Ready(Ok(()))
            }
            Poll::Ready(Err(err)) => Poll::Ready(Err(err)),
            Poll::Pending => Poll::Pending,
        }
    }
}

impl AsyncWrite for Pipe {
    #[inline]
    async fn write<B: IoBuf>(&mut self, buf: B) -> (Result<usize, io::Error>, B) {
        let handle = &self.handle;
        let mut op = WriteOp::new(handle, buf);
        let result = poll_fn(|cx| handle.poll_op(cx, &mut op)).await;
        (result, op.take_bufs())
    }

    #[inline]
    async fn flush(&mut self) -> Result<(), io::Error> {
        Ok(())
    }

    #[inline]
    async fn write_vectored<B: IoVectoredBuf>(&mut self, bufs: B) -> (Result<usize, io::Error>, B) {
        if bufs.is_empty() {
            return (Ok(0), bufs);
        }
        let handle = &self.handle;
        let mut op = WritevOp::new(handle, bufs);
        let result = poll_fn(|cx| handle.poll_op(cx, &mut op)).await;
        (result, op.take_bufs())
    }
}

impl TokioAsyncWrite for PollPipe {
    #[inline]
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<Result<usize, io::Error>> {
        let this = self.get_mut();
        // SAFETY: buf remains initialized and borrowed throughout this call.
        // WriteOp only reads it; poll_op_poll rejects completion mode, and the
        // local operation is dropped before this borrow can end, even on Pending.
        let buf = unsafe { IoBufTemporaryPoll::new(buf.as_ptr() as *mut u8, buf.len()) };
        let mut op = WriteOp::new(&this.stream.handle, buf);
        this.stream.handle.poll_op_poll(cx, &mut op)
    }

    #[inline]
    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        bufs: &[IoSlice<'_>],
    ) -> Poll<Result<usize, io::Error>> {
        if bufs.is_empty() {
            return Poll::Ready(Ok(0));
        }
        let this = self.get_mut();
        // SAFETY: IoSlice data stays borrowed and initialized for this poll.
        // Only descriptor metadata is copied; WritevOp does synchronous poll I/O
        // through poll_op_poll and cannot retain these buffers after returning.
        let bufs = unsafe { IoVectoredBufTemporaryPoll::new(bufs) };
        let mut op = WritevOp::new(&this.stream.handle, bufs);
        this.stream.handle.poll_op_poll(cx, &mut op)
    }

    #[inline]
    fn is_write_vectored(&self) -> bool {
        true
    }

    #[inline]
    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }

    #[inline]
    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<Result<(), io::Error>> {
        Poll::Ready(Ok(()))
    }
}
