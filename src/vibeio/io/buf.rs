//! Buffer traits for async I/O operations.
//!
//! This module provides traits for working with buffers in async I/O:
//! - `IoBuf` and `IoBufMut`: traits for read/write buffers.
//! - `IoVectoredBuf` and `IoVectoredBufMut`: traits for vectored I/O buffers.
//!
//! # Buffer types
//!
//! `IoBuf` is implemented for:
//! - `Vec<u8>`
//! - `String` (read-only)
//! - `&'static [u8]`
//! - `&'static str`
//! - `[u8; N]` for any size `N`
//! - `Box<[u8]>`
//!
//! `IoBufMut` is implemented for:
//! - `Vec<u8>`
//! - `[u8; N]` for any size `N`
//! - `Box<[u8]>`
//!
//! # Examples
//!
//! ```ignore
//! use vibeio::io::{AsyncRead, IoBufMut};
//!
//! async fn read_something<R: AsyncRead>(reader: &mut R) {
//!     let mut buf = vec![0u8; 1024];
//!     let (result, buf) = reader.read(buf).await;
//!     let bytes_read = result.unwrap_or(0);
//!     println!("Read {} bytes", bytes_read);
//! }
//! ```

use std::io::{IoSlice, IoSliceMut};

/// Trait for read-only buffers.
///
/// This trait is implemented by types that can be used as buffers for
/// reading data in async I/O operations.
/// # Safety
///
/// Implementors must keep the returned pointer valid and stable for reads of
/// `buf_len()` bytes while the buffer is owned by an I/O operation. The
/// reported length must not exceed `buf_capacity()`.
pub unsafe trait IoBuf: Send + 'static {
    /// Returns a raw pointer to the inner buffer.
    fn as_buf_ptr(&self) -> *const u8;

    /// Returns the length of the initialized part of the buffer.
    fn buf_len(&self) -> usize;

    /// Returns the capacity of the buffer.
    fn buf_capacity(&self) -> usize;
}

/// Trait for mutable buffers.
///
/// This trait extends `IoBuf` with mutable operations needed for writing.
/// # Safety
///
/// In addition to the [`IoBuf`] requirements, the mutable pointer must remain
/// valid and stable for writes of `buf_capacity()` bytes. `set_buf_init` must
/// only expose bytes initialized by the completed operation.
pub unsafe trait IoBufMut: IoBuf {
    /// Returns a raw mutable pointer to the inner buffer.
    fn as_buf_mut_ptr(&mut self) -> *mut u8;

    /// Updates the length of the initialized part of the buffer.
    ///
    /// # Safety
    ///
    /// The caller must ensure that the given `len` does not exceed the capacity
    /// of the buffer, and that the elements up to `len` have been initialized.
    unsafe fn set_buf_init(&mut self, len: usize);
}

unsafe impl IoBuf for Vec<u8> {
    #[inline]
    fn as_buf_ptr(&self) -> *const u8 {
        self.as_ptr()
    }

    #[inline]
    fn buf_len(&self) -> usize {
        self.len()
    }

    #[inline]
    fn buf_capacity(&self) -> usize {
        self.capacity()
    }
}

unsafe impl IoBufMut for Vec<u8> {
    #[inline]
    fn as_buf_mut_ptr(&mut self) -> *mut u8 {
        self.as_mut_ptr()
    }

    #[inline]
    unsafe fn set_buf_init(&mut self, len: usize) {
        self.set_len(len);
    }
}

unsafe impl IoBuf for String {
    #[inline]
    fn as_buf_ptr(&self) -> *const u8 {
        self.as_ptr()
    }

    #[inline]
    fn buf_len(&self) -> usize {
        self.len()
    }

    #[inline]
    fn buf_capacity(&self) -> usize {
        self.capacity()
    }
}

unsafe impl IoBuf for &'static [u8] {
    #[inline]
    fn as_buf_ptr(&self) -> *const u8 {
        self.as_ptr()
    }

    #[inline]
    fn buf_len(&self) -> usize {
        self.len()
    }

    #[inline]
    fn buf_capacity(&self) -> usize {
        self.len()
    }
}

unsafe impl IoBuf for &'static str {
    #[inline]
    fn as_buf_ptr(&self) -> *const u8 {
        self.as_bytes().as_ptr()
    }

    #[inline]
    fn buf_len(&self) -> usize {
        self.len()
    }

    #[inline]
    fn buf_capacity(&self) -> usize {
        self.len()
    }
}

unsafe impl<const N: usize> IoBuf for [u8; N] {
    #[inline]
    fn as_buf_ptr(&self) -> *const u8 {
        self.as_ptr()
    }

    #[inline]
    fn buf_len(&self) -> usize {
        N
    }

    #[inline]
    fn buf_capacity(&self) -> usize {
        N
    }
}

unsafe impl<const N: usize> IoBufMut for [u8; N] {
    #[inline]
    fn as_buf_mut_ptr(&mut self) -> *mut u8 {
        self.as_mut_ptr()
    }

    unsafe fn set_buf_init(&mut self, _len: usize) {}
}

unsafe impl IoBuf for Box<[u8]> {
    #[inline]
    fn as_buf_ptr(&self) -> *const u8 {
        self.as_ptr()
    }

    #[inline]
    fn buf_len(&self) -> usize {
        self.len()
    }

    #[inline]
    fn buf_capacity(&self) -> usize {
        self.len()
    }
}

unsafe impl IoBufMut for Box<[u8]> {
    #[inline]
    fn as_buf_mut_ptr(&mut self) -> *mut u8 {
        self.as_mut_ptr()
    }

    unsafe fn set_buf_init(&mut self, _len: usize) {}
}

/// A buffer wrapper with a cursor for tracking progress.
pub(crate) struct IoBufWithCursor<I: IoBuf> {
    pub(crate) buf: I,
    pub(crate) cursor: usize,
}

impl<I: IoBuf> IoBufWithCursor<I> {
    /// Create a new `IoBufWithCursor` with the given buffer.
    #[inline]
    pub(crate) fn new(buf: I) -> Self {
        IoBufWithCursor { buf, cursor: 0 }
    }

    /// Advance the cursor by `n` bytes.
    #[inline]
    pub(crate) fn advance(&mut self, n: usize) {
        assert!(
            n <= self.buf_len(),
            "cannot advance an I/O buffer cursor past the initialized data"
        );
        self.cursor += n;
    }

    /// Consume the wrapper and return the inner buffer.
    #[inline]
    pub(crate) fn into_inner(self) -> I {
        self.buf
    }
}

unsafe impl<I: IoBuf> IoBuf for IoBufWithCursor<I> {
    #[inline]
    fn as_buf_ptr(&self) -> *const u8 {
        unsafe { self.buf.as_buf_ptr().add(self.cursor) }
    }

    #[inline]
    fn buf_len(&self) -> usize {
        self.buf.buf_len() - self.cursor
    }

    #[inline]
    fn buf_capacity(&self) -> usize {
        self.buf.buf_capacity() - self.cursor
    }
}

unsafe impl<I: IoBufMut> IoBufMut for IoBufWithCursor<I> {
    #[inline]
    fn as_buf_mut_ptr(&mut self) -> *mut u8 {
        unsafe { self.buf.as_buf_mut_ptr().add(self.cursor) }
    }

    unsafe fn set_buf_init(&mut self, len: usize) {
        self.buf.set_buf_init(self.cursor + len);
    }
}

/// A temporary buffer for polling operations.
pub(crate) struct IoBufTemporaryPoll {
    ptr: *mut u8,
    len: usize,
    capacity: usize,
}

impl IoBufTemporaryPoll {
    /// Create a new `IoBufTemporaryPoll` with the given pointer and length.
    ///
    /// # Safety
    ///
    /// `ptr` must remain valid for `len` initialized bytes throughout the poll.
    /// Read operations additionally require exclusive writable access. The
    /// wrapper must not escape the backing borrow, be sent to another thread,
    /// or be submitted to an operation that retains the pointer after polling.
    #[inline]
    pub(crate) unsafe fn new(ptr: *mut u8, len: usize) -> Self {
        Self {
            ptr,
            len,
            capacity: len,
        }
    }

    /// Wrap writable storage without claiming that any bytes are initialized.
    ///
    /// # Safety
    ///
    /// `ptr` must be non-null, aligned, and exclusively writable for `capacity`
    /// bytes. The same lifetime and poll-only restrictions as `new` apply.
    #[inline]
    pub(crate) unsafe fn new_uninit(ptr: *mut u8, capacity: usize) -> Self {
        Self {
            ptr,
            len: 0,
            capacity,
        }
    }
}

// SAFETY: constructors distinguish initialized length from writable capacity;
// their caller keeps the allocation stable for the entire synchronous poll.
unsafe impl IoBuf for IoBufTemporaryPoll {
    #[inline]
    fn as_buf_ptr(&self) -> *const u8 {
        self.ptr as *const u8
    }

    #[inline]
    fn buf_len(&self) -> usize {
        self.len
    }

    #[inline]
    fn buf_capacity(&self) -> usize {
        self.capacity
    }
}

// SAFETY: mutable use requires exclusive storage under the constructor contract.
// Only the initialized prefix supplied by a successful read is exposed.
unsafe impl IoBufMut for IoBufTemporaryPoll {
    #[inline]
    fn as_buf_mut_ptr(&mut self) -> *mut u8 {
        self.ptr
    }

    #[inline]
    unsafe fn set_buf_init(&mut self, len: usize) {
        self.len = len;
    }
}

// SAFETY: construction is unsafe and requires the wrapper to stay within the
// backing borrow on the polling thread. Send is required by IoBuf, but callers
// must not use it to transfer this non-owning wrapper or retain it asynchronously.
unsafe impl Send for IoBufTemporaryPoll {}

/// A single I/O vector entry.
pub struct IoVec {
    /// Pointer to the data.
    pub ptr: *mut u8,
    /// Length of the data.
    pub len: usize,
}

/// Trait for vectored read buffers.
/// # Safety
///
/// Every returned vector must describe memory that remains valid and stable
/// for reads for as long as the I/O operation owns this value.
pub unsafe trait IoVectoredBuf: 'static {
    /// Returns a pointer to an array of `iovec` structures and its length.
    #[inline]
    fn as_iovecs(&self) -> Box<[IoVec]> {
        unimplemented!()
    }

    /// Returns `true` if the vectored buffer is empty.
    #[inline]
    fn is_empty(&self) -> bool {
        self.as_iovecs().is_empty()
    }
}

/// Trait for vectored write buffers.
/// # Safety
///
/// Every returned vector must describe memory that remains valid and stable
/// for writes for as long as the I/O operation owns this value.
pub unsafe trait IoVectoredBufMut: IoVectoredBuf {
    /// Returns a mutable pointer to an array of `iovec` structures and its length.
    #[inline]
    fn as_iovecs_mut(&mut self) -> Box<[IoVec]> {
        unimplemented!()
    }
}

#[cfg(unix)]
unsafe impl IoVectoredBuf for Vec<libc::iovec> {
    #[inline]
    fn as_iovecs(&self) -> Box<[IoVec]> {
        let mut iovecs = Box::new_uninit_slice(self.len());
        for (index, iovec) in self.iter().enumerate() {
            iovecs[index].write(IoVec {
                ptr: iovec.iov_base as *mut u8,
                len: iovec.iov_len,
            });
        }

        unsafe { iovecs.assume_init() }
    }

    #[inline]
    fn is_empty(&self) -> bool {
        self.is_empty()
    }
}

#[cfg(unix)]
unsafe impl IoVectoredBufMut for Vec<libc::iovec> {
    #[inline]
    fn as_iovecs_mut(&mut self) -> Box<[IoVec]> {
        self.as_iovecs()
    }
}

#[cfg(unix)]
unsafe impl IoVectoredBuf for Box<[libc::iovec]> {
    #[inline]
    fn as_iovecs(&self) -> Box<[IoVec]> {
        let mut iovecs = Box::new_uninit_slice(self.len());
        for (index, iovec) in self.iter().enumerate() {
            iovecs[index].write(IoVec {
                ptr: iovec.iov_base as *mut u8,
                len: iovec.iov_len,
            });
        }

        unsafe { iovecs.assume_init() }
    }
}

#[cfg(unix)]
unsafe impl IoVectoredBufMut for Box<[libc::iovec]> {
    #[inline]
    fn as_iovecs_mut(&mut self) -> Box<[IoVec]> {
        self.as_iovecs()
    }
}

/// A temporary vectored buffer for polling operations.
pub(crate) struct IoVectoredBufTemporaryPoll {
    pub(crate) iovecs: Vec<(*mut u8, usize)>,
}

impl IoVectoredBufTemporaryPoll {
    /// Create a new `IoVectoredBufTemporaryPoll` from immutable slices.
    #[inline]
    pub(crate) unsafe fn new(iovecs: &[IoSlice<'_>]) -> Self {
        let iovecs = iovecs
            .iter()
            .map(|iovec| (iovec.as_ptr() as *mut u8, iovec.len()))
            .collect();
        Self { iovecs }
    }

    /// Create a new `IoVectoredBufTemporaryPoll` from mutable slices.
    #[allow(dead_code)]
    #[inline]
    pub(crate) unsafe fn new_mut(iovecs: &mut [IoSliceMut<'_>]) -> Self {
        let iovecs = iovecs
            .iter_mut()
            .map(|iovec| (iovec.as_mut_ptr(), iovec.len()))
            .collect();
        Self { iovecs }
    }
}

unsafe impl IoVectoredBuf for IoVectoredBufTemporaryPoll {
    #[inline]
    fn as_iovecs(&self) -> Box<[IoVec]> {
        let mut iovecs = Box::new_uninit_slice(self.iovecs.len());
        for (index, iovec) in self.iovecs.iter().enumerate() {
            iovecs[index].write(IoVec {
                ptr: iovec.0,
                len: iovec.1,
            });
        }

        unsafe { iovecs.assume_init() }
    }

    #[inline]
    fn is_empty(&self) -> bool {
        self.iovecs.is_empty()
    }
}

unsafe impl IoVectoredBufMut for IoVectoredBufTemporaryPoll {
    #[inline]
    fn as_iovecs_mut(&mut self) -> Box<[IoVec]> {
        self.as_iovecs()
    }
}

#[cfg(any(feature = "fs", feature = "process", feature = "stdio"))]
#[inline]
pub(crate) fn iobuf_to_slice(buf: &impl IoBuf) -> &[u8] {
    unsafe { std::slice::from_raw_parts(buf.as_buf_ptr(), buf.buf_len()) }
}

#[cfg(any(feature = "fs", feature = "process", feature = "stdio"))]
#[inline]
pub(crate) fn iobufmut_to_slice(buf: &mut impl IoBufMut) -> &mut [u8] {
    unsafe { std::slice::from_raw_parts_mut(buf.as_buf_mut_ptr(), buf.buf_len()) }
}

#[cfg(test)]
mod tests {
    use super::{IoBuf, IoBufMut, IoBufTemporaryPoll};

    #[test]
    fn temporary_poll_buffer_tracks_initialized_prefix() {
        let mut storage = [std::mem::MaybeUninit::<u8>::uninit(); 8];
        // SAFETY: storage stays exclusively borrowed on this thread until buf
        // is no longer used. No asynchronous operation receives the pointer.
        let mut buf =
            unsafe { IoBufTemporaryPoll::new_uninit(storage.as_mut_ptr().cast(), storage.len()) };
        assert_eq!(buf.buf_len(), 0);
        assert_eq!(buf.buf_capacity(), 8);
        // SAFETY: write and expose only the first three bytes of this allocation.
        unsafe {
            std::ptr::copy_nonoverlapping(b"abc".as_ptr(), buf.as_buf_mut_ptr(), 3);
            buf.set_buf_init(3);
            assert_eq!(
                std::slice::from_raw_parts(buf.as_buf_ptr(), buf.buf_len()),
                b"abc"
            );
        }
        assert_eq!(buf.buf_capacity(), 8);
    }

    async fn check_poll_read_buffer(
        mut stream: impl tokio::io::AsyncRead + Unpin,
        mut peer: impl std::io::Write,
    ) {
        use std::{
            mem::MaybeUninit,
            pin::Pin,
            task::{Context, Poll},
        };
        use tokio::io::ReadBuf;

        let mut storage = [MaybeUninit::uninit(); 8];
        let mut buf = ReadBuf::uninit(&mut storage);
        buf.put_slice(b"!");
        let mut cx = Context::from_waker(std::task::Waker::noop());
        assert!(
            Pin::new(&mut stream)
                .poll_read(&mut cx, &mut buf)
                .is_pending()
        );
        assert_eq!(buf.filled(), b"!");
        assert_eq!(buf.initialized().len(), 1);

        peer.write_all(b"abc").unwrap();
        // The exact number returned by each read is allowed to vary.
        while buf.filled().len() < 4 {
            let before = buf.filled().len();
            std::future::poll_fn(|cx| Pin::new(&mut stream).poll_read(cx, &mut buf))
                .await
                .unwrap();
            assert!(buf.filled().len() > before);
        }
        assert_eq!(buf.filled(), b"!abc");
        assert_eq!(buf.initialized().len(), 4);
        drop(peer);
        std::future::poll_fn(|cx| Pin::new(&mut stream).poll_read(cx, &mut buf))
            .await
            .unwrap();
        assert_eq!(buf.filled(), b"!abc");
        assert_eq!(buf.initialized().len(), 4);

        let mut empty = [];
        let mut empty = ReadBuf::uninit(&mut empty);
        assert!(matches!(
            Pin::new(&mut stream).poll_read(&mut cx, &mut empty),
            Poll::Ready(Ok(()))
        ));
    }

    #[test]
    fn tcp_poll_read_handles_uninitialized_storage_pending_and_eof() {
        #[cfg(unix)]
        let driver = crate::vibeio::driver::AnyDriver::new_mio().unwrap();
        #[cfg(windows)]
        let driver = crate::vibeio::driver::AnyDriver::new_iocp().unwrap();
        let runtime = crate::vibeio::executor::Runtime::new(driver);
        runtime.block_on(async {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let peer = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
            let (stream, _) = listener.accept().unwrap();
            let stream = crate::vibeio::net::PollTcpStream::from_std(stream).unwrap();
            check_poll_read_buffer(stream, peer).await;
        });
    }

    #[cfg(unix)]
    #[test]
    fn unix_poll_read_handles_uninitialized_storage_pending_and_eof() {
        let runtime = crate::vibeio::executor::Runtime::new(
            crate::vibeio::driver::AnyDriver::new_mio().unwrap(),
        );
        runtime.block_on(async {
            let (stream, peer) = std::os::unix::net::UnixStream::pair().unwrap();
            let stream = crate::vibeio::net::PollUnixStream::from_std(stream).unwrap();
            check_poll_read_buffer(stream, peer).await;
        });
    }
}
