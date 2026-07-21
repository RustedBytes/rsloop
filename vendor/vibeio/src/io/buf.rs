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
//! - `String`
//! - `&'static [u8]`
//! - `&'static str`
//! - `[u8; N]` for any size `N`
//! - `Box<[u8]>`
//!
//! `IoBufMut` is implemented for:
//! - `Vec<u8>`
//! - `String`
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
pub trait IoBuf: Send + 'static {
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
pub trait IoBufMut: IoBuf {
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

impl IoBuf for Vec<u8> {
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

impl IoBufMut for Vec<u8> {
    #[inline]
    fn as_buf_mut_ptr(&mut self) -> *mut u8 {
        self.as_mut_ptr()
    }

    #[inline]
    unsafe fn set_buf_init(&mut self, len: usize) {
        self.set_len(len);
    }
}

impl IoBuf for String {
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

impl IoBufMut for String {
    #[inline]
    fn as_buf_mut_ptr(&mut self) -> *mut u8 {
        unsafe { self.as_mut_vec().as_mut_ptr() }
    }

    #[inline]
    unsafe fn set_buf_init(&mut self, len: usize) {
        self.as_mut_vec().set_len(len);
    }
}

impl IoBuf for &'static [u8] {
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

impl IoBuf for &'static str {
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

impl<const N: usize> IoBuf for [u8; N] {
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

impl<const N: usize> IoBufMut for [u8; N] {
    #[inline]
    fn as_buf_mut_ptr(&mut self) -> *mut u8 {
        self.as_mut_ptr()
    }

    unsafe fn set_buf_init(&mut self, _len: usize) {}
}

impl IoBuf for Box<[u8]> {
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

impl IoBufMut for Box<[u8]> {
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
        self.cursor += n;
    }

    /// Consume the wrapper and return the inner buffer.
    #[inline]
    pub(crate) fn into_inner(self) -> I {
        self.buf
    }
}

impl<I: IoBuf> IoBuf for IoBufWithCursor<I> {
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

impl<I: IoBufMut> IoBufMut for IoBufWithCursor<I> {
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
}

impl IoBufTemporaryPoll {
    /// Create a new `IoBufTemporaryPoll` with the given pointer and length.
    #[inline]
    pub(crate) unsafe fn new(ptr: *mut u8, len: usize) -> Self {
        Self { ptr, len }
    }
}

impl IoBuf for IoBufTemporaryPoll {
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
        self.len
    }
}

impl IoBufMut for IoBufTemporaryPoll {
    #[inline]
    fn as_buf_mut_ptr(&mut self) -> *mut u8 {
        self.ptr
    }

    #[inline]
    unsafe fn set_buf_init(&mut self, _len: usize) {}
}

unsafe impl Send for IoBufTemporaryPoll {}

/// A single I/O vector entry.
pub struct IoVec {
    /// Pointer to the data.
    pub ptr: *mut u8,
    /// Length of the data.
    pub len: usize,
}

/// Trait for vectored read buffers.
pub trait IoVectoredBuf: 'static {
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
pub trait IoVectoredBufMut: IoVectoredBuf {
    /// Returns a mutable pointer to an array of `iovec` structures and its length.
    #[inline]
    fn as_iovecs_mut(&mut self) -> Box<[IoVec]> {
        unimplemented!()
    }
}

#[cfg(unix)]
impl IoVectoredBuf for Vec<libc::iovec> {
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
impl IoVectoredBufMut for Vec<libc::iovec> {
    #[inline]
    fn as_iovecs_mut(&mut self) -> Box<[IoVec]> {
        self.as_iovecs()
    }
}

#[cfg(unix)]
impl IoVectoredBuf for Box<[libc::iovec]> {
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
impl IoVectoredBufMut for Box<[libc::iovec]> {
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

impl IoVectoredBuf for IoVectoredBufTemporaryPoll {
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

impl IoVectoredBufMut for IoVectoredBufTemporaryPoll {
    #[inline]
    fn as_iovecs_mut(&mut self) -> Box<[IoVec]> {
        self.as_iovecs()
    }
}

#[inline]
pub(crate) fn iobuf_to_slice(buf: &impl IoBuf) -> &[u8] {
    unsafe { std::slice::from_raw_parts(buf.as_buf_ptr(), buf.buf_len()) }
}

#[inline]
pub(crate) fn iobufmut_to_slice(buf: &mut impl IoBufMut) -> &mut [u8] {
    unsafe { std::slice::from_raw_parts_mut(buf.as_buf_mut_ptr(), buf.buf_len()) }
}
