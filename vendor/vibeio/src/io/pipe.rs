//! Async pipe utilities.
//!
//! This module provides async-aware pipe endpoints:
//! - `pipe()`: create a pair of async-aware pipe endpoints.
//! - `Pipe`: a pipe endpoint for async I/O.
//! - `PollPipe`: a variant that uses readiness-based polling.
//!
//! # Examples
//!
//! ```ignore
//! use vibeio::io::{AsyncRead, AsyncWrite, pipe};
//!
//! async fn pipe_example() {
//!     let (mut reader, mut writer) = pipe().unwrap();
//!
//!     writer.write(b"hello").await.0.unwrap();
//!     let mut buf = [0u8; 5];
//!     reader.read(buf).await.0.unwrap();
//!     assert_eq!(&buf, b"hello");
//! }
//! ```

use std::future::poll_fn;
use std::io::{self, IoSlice};
use std::mem::{ManuallyDrop, MaybeUninit};
use std::os::fd::OwnedFd;
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, RawFd};
use std::pin::Pin;
use std::task::{Context, Poll};

use mio::Interest;
use tokio::io::{AsyncRead as TokioAsyncRead, AsyncWrite as TokioAsyncWrite, ReadBuf};

use crate::io::{
    AsInnerRawHandle, IoBuf, IoBufMut, IoBufTemporaryPoll, IoVectoredBuf, IoVectoredBufMut,
    IoVectoredBufTemporaryPoll,
};
use crate::op::{ReadOp, ReadvOp, WriteOp, WritevOp};
use crate::{
    driver::RegistrationMode,
    fd_inner::InnerRawHandle,
    io::{AsyncRead, AsyncWrite},
};

fn pipe_inner() -> std::io::Result<(OwnedFd, OwnedFd)> {
    let mut fds: [RawFd; 2] = [-1; 2];
    let result = unsafe { libc::pipe(fds.as_mut_ptr()) };
    if result == -1 {
        return Err(std::io::Error::last_os_error());
    }
    Ok((unsafe { OwnedFd::from_raw_fd(fds[0]) }, unsafe {
        OwnedFd::from_raw_fd(fds[1])
    }))
}

/// Create a new async-aware pipe.
///
/// Returns a tuple of `(reader, writer)` pipe endpoints.
pub fn pipe() -> std::io::Result<(Pipe, Pipe)> {
    let (read, write) = pipe_inner()?;
    Ok((
        Pipe::from_std_with_mode(read, RegistrationMode::Completion)?,
        Pipe::from_std_with_mode(write, RegistrationMode::Completion)?,
    ))
}

/// A pipe endpoint that can use either completion or readiness-based I/O.
pub struct Pipe {
    inner: OwnedFd,
    handle: ManuallyDrop<InnerRawHandle>,
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
        let handle = ManuallyDrop::new(InnerRawHandle::new_with_mode(
            inner.as_raw_fd(),
            Interest::READABLE | Interest::WRITABLE,
            mode,
        )?);
        let flags = unsafe { libc::fcntl(inner.as_raw_fd(), libc::F_GETFL) };
        if flags != -1 {
            let mut new_flags = flags | libc::O_NONBLOCK;
            if handle.uses_completion() {
                new_flags &= !libc::O_NONBLOCK;
            }
            unsafe { libc::fcntl(inner.as_raw_fd(), libc::F_SETFL, new_flags) };
        }
        Ok(Self { inner, handle })
    }

    /// Convert this `Pipe` to a `PollPipe` for readiness-based operations.
    #[inline]
    pub fn into_poll(self) -> Result<PollPipe, io::Error> {
        let mut stream = self;
        stream.handle.rebind_mode(RegistrationMode::Poll)?;
        let flags = unsafe { libc::fcntl(stream.inner.as_raw_fd(), libc::F_GETFL) };
        if flags != -1 {
            let mut new_flags = flags | libc::O_NONBLOCK;
            if stream.handle.uses_completion() {
                new_flags &= !libc::O_NONBLOCK;
            }
            unsafe { libc::fcntl(stream.inner.as_raw_fd(), libc::F_SETFL, new_flags) };
        }
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
        let flags = unsafe { libc::fcntl(stream.inner.as_raw_fd(), libc::F_GETFL) };
        if flags != -1 {
            let mut new_flags = flags | libc::O_NONBLOCK;
            if stream.handle.uses_completion() {
                new_flags &= !libc::O_NONBLOCK;
            }
            unsafe { libc::fcntl(stream.inner.as_raw_fd(), libc::F_SETFL, new_flags) };
        }
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
        let mut this = ManuallyDrop::new(self);

        // Safety: `this` will not be dropped, so we must drop the registration handle manually.
        // We then move out the inner std stream and transfer its fd ownership to the caller.
        unsafe {
            ManuallyDrop::drop(&mut this.handle);
            std::ptr::read(&this.inner).into_raw_fd()
        }
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
        // Equivalent to .assume_init_mut() in Rust 1.93.0+
        let unfilled = unsafe { &mut *(buf.unfilled_mut() as *mut [MaybeUninit<u8>] as *mut [u8]) };
        let buf_temp = unsafe { IoBufTemporaryPoll::new(unfilled.as_mut_ptr(), unfilled.len()) };
        let mut op = ReadOp::new(&this.stream.handle, buf_temp);
        match this.stream.handle.poll_op_poll(cx, &mut op) {
            Poll::Ready(Ok(read)) => {
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

impl Drop for Pipe {
    #[inline]
    fn drop(&mut self) {
        // Safety: The struct is dropped after the handle is dropped.
        unsafe {
            ManuallyDrop::drop(&mut self.handle);
        }
    }
}
