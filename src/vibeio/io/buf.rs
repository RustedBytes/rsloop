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
//! See the executable "Buffer length and capacity" and "Pipe buffer ownership" examples in
//! `tools/vibeio-check/EXAMPLES.md`. Read errors must be handled separately from
//! EOF; the returned buffer contains the data initialized by the operation.

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(clippy::undocumented_unsafe_blocks)]

use std::io::{IoSlice, IoSliceMut};

/// Trait for read-only buffers.
///
/// This trait is implemented by types that can be used as buffers for
/// reading data in async I/O operations.
/// # Safety
///
/// Implementors must keep the returned pointer valid and stable for reads of
/// `buf_len()` bytes while the buffer value stays at a fixed address. Callers
/// must not move the value while a submitted pointer is outstanding; completion
/// operations enforce this by retaining the same boxed buffer allocation across
/// cancellation. Moving a Vec or Box may preserve its pointer, but inline arrays
/// require this caller-side address stability. The
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

// SAFETY: Vec owns its allocation; len describes initialized bytes and never
// exceeds capacity. Operations do not resize it while its pointer is in use.
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

// SAFETY: &mut Vec grants exclusive access to its allocation, including spare
// capacity. set_buf_init exposes only the prefix initialized by its caller.
unsafe impl IoBufMut for Vec<u8> {
    #[inline]
    fn as_buf_mut_ptr(&mut self) -> *mut u8 {
        self.as_mut_ptr()
    }

    #[inline]
    unsafe fn set_buf_init(&mut self, len: usize) {
        // SAFETY: the IoBufMut caller guarantees initialization and capacity.
        unsafe { self.set_len(len) };
    }
}

// SAFETY: String owns initialized UTF-8 bytes. Only read access to its len-byte
// prefix is exposed, so neither UTF-8 validity nor spare capacity is modified.
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

// SAFETY: the initialized slice is immutable and outlives every operation.
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

// SAFETY: the initialized UTF-8 bytes are immutable and have static storage.
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

// SAFETY: all N bytes are initialized. The operation keeps the array stationary
// while using its pointer (completion operations box their buffer storage).
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

// SAFETY: the exclusive array borrow covers all N writable initialized bytes.
// Partial writes do not make any of the remaining array elements uninitialized.
unsafe impl<const N: usize> IoBufMut for [u8; N] {
    #[inline]
    fn as_buf_mut_ptr(&mut self) -> *mut u8 {
        self.as_mut_ptr()
    }

    unsafe fn set_buf_init(&mut self, _len: usize) {}
}

// SAFETY: the box owns len initialized bytes at a stable allocation address.
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

// SAFETY: the box exclusively owns its initialized slice; partial writes leave
// the other bytes valid and do not change the allocation size.
unsafe impl IoBufMut for Box<[u8]> {
    #[inline]
    fn as_buf_mut_ptr(&mut self) -> *mut u8 {
        self.as_mut_ptr()
    }

    unsafe fn set_buf_init(&mut self, _len: usize) {}
}

/// A buffer wrapper with a cursor for tracking progress.
pub(crate) struct IoBufWithCursor<I: IoBuf> {
    buf: I,
    cursor: usize,
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

// SAFETY: private fields and checked advance keep cursor within initialized
// data. The underlying IoBuf retains ownership and supplies stable storage.
unsafe impl<I: IoBuf> IoBuf for IoBufWithCursor<I> {
    #[inline]
    fn as_buf_ptr(&self) -> *const u8 {
        // SAFETY: cursor is at most the initialized length, hence in bounds or
        // one-past-the-end of the allocation supplied by IoBuf.
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

// SAFETY: the suffix is exclusively borrowed from the underlying IoBufMut;
// its writable capacity is reduced by the checked cursor offset.
unsafe impl<I: IoBufMut> IoBufMut for IoBufWithCursor<I> {
    #[inline]
    fn as_buf_mut_ptr(&mut self) -> *mut u8 {
        // SAFETY: cursor never exceeds the underlying initialized length.
        unsafe { self.buf.as_buf_mut_ptr().add(self.cursor) }
    }

    unsafe fn set_buf_init(&mut self, len: usize) {
        // SAFETY: caller initialized len suffix bytes within suffix capacity;
        // the prefix before cursor was already initialized.
        unsafe { self.buf.set_buf_init(self.cursor + len) };
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

/// Owned readable buffers for vectored output operations.
/// # Safety
///
/// Every returned vector must describe initialized memory that remains valid
/// and stable for reads for as long as the I/O operation owns this value,
/// including across moves of the value and asynchronous cancellation.
pub unsafe trait IoVectoredBuf: 'static {
    /// Returns owned descriptors for this value's readable memory regions.
    fn as_iovecs(&self) -> Box<[IoVec]>;

    /// Returns `true` if there are no vector descriptors.
    ///
    /// This does not test the total initialized byte count. Input operations
    /// also use this check, and an implementation may expose zero readable
    /// bytes while its writable descriptors provide spare capacity. Collections
    /// containing empty segments are therefore not necessarily empty here.
    #[inline]
    fn is_empty(&self) -> bool {
        self.as_iovecs().is_empty()
    }
}

/// Owned writable buffers for vectored input operations.
///
/// A successful read returns the total number of bytes initialized across the
/// descriptors in order, skipping empty regions. Unlike `IoBufMut`, this trait
/// has no initialization-length setter: reads do not resize individual buffers
/// or update custom initialization metadata. Implementations exposing spare
/// capacity must use the returned byte count before exposing those bytes safely.
/// # Safety
///
/// Every returned vector must describe exclusively writable memory that remains
/// valid and stable for as long as the I/O operation owns this value. Writable
/// regions must not overlap one another or any live references to their bytes.
pub unsafe trait IoVectoredBufMut: IoVectoredBuf {
    /// Returns owned descriptors for this value's writable memory regions.
    fn as_iovecs_mut(&mut self) -> Box<[IoVec]>;
}

// SAFETY: each box owns initialized bytes at an address stable across moves of
// the vector. Unlike a collection of libc::iovec, it owns the pointed-to storage.
unsafe impl IoVectoredBuf for Vec<Box<[u8]>> {
    #[inline]
    fn as_iovecs(&self) -> Box<[IoVec]> {
        self.iter()
            .map(|buf| IoVec {
                ptr: buf.as_ptr().cast_mut(),
                len: buf.len(),
            })
            .collect()
    }

    #[inline]
    fn is_empty(&self) -> bool {
        self.is_empty()
    }
}

// SAFETY: distinct owned boxes cannot overlap; &mut self grants exclusive access
// to every initialized buffer for the duration of the operation.
unsafe impl IoVectoredBufMut for Vec<Box<[u8]>> {
    #[inline]
    fn as_iovecs_mut(&mut self) -> Box<[IoVec]> {
        self.iter_mut()
            .map(|buf| IoVec {
                ptr: buf.as_mut_ptr(),
                len: buf.len(),
            })
            .collect()
    }
}

/// A temporary vectored buffer for polling operations.
pub(crate) struct IoVectoredBufTemporaryPoll {
    iovecs: Vec<(*mut u8, usize)>,
}

impl IoVectoredBufTemporaryPoll {
    /// Create a new `IoVectoredBufTemporaryPoll` from immutable slices.
    ///
    /// # Safety
    ///
    /// All backing borrows must outlive this wrapper and any use of its pointers.
    /// Use only for synchronous write polling, never completion submission or
    /// mutable I/O. No pointer may be retained after the poll returns.
    #[inline]
    pub(crate) unsafe fn new(iovecs: &[IoSlice<'_>]) -> Self {
        let iovecs = iovecs
            .iter()
            .map(|iovec| (iovec.as_ptr() as *mut u8, iovec.len()))
            .collect();
        Self { iovecs }
    }

    /// Create a new `IoVectoredBufTemporaryPoll` from mutable slices.
    ///
    /// # Safety
    ///
    /// All backing exclusive borrows must outlive this wrapper and any pointer
    /// use. Use only for synchronous polling, never completion submission.
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

// SAFETY: the unsafe constructors require stable, initialized borrowed storage
// and forbid retaining pointers beyond the synchronous polling operation.
unsafe impl IoVectoredBuf for IoVectoredBufTemporaryPoll {
    #[inline]
    fn as_iovecs(&self) -> Box<[IoVec]> {
        self.iovecs
            .iter()
            .map(|&(ptr, len)| IoVec { ptr, len })
            .collect()
    }

    #[inline]
    fn is_empty(&self) -> bool {
        self.iovecs.is_empty()
    }
}

// SAFETY: mutable I/O is permitted only for new_mut, whose contract preserves
// the non-overlapping exclusive IoSliceMut borrows throughout the poll.
unsafe impl IoVectoredBufMut for IoVectoredBufTemporaryPoll {
    #[inline]
    fn as_iovecs_mut(&mut self) -> Box<[IoVec]> {
        self.as_iovecs()
    }
}

#[cfg(any(test, feature = "fs", feature = "process", feature = "stdio"))]
#[inline]
pub(crate) fn iobuf_to_slice(buf: &impl IoBuf) -> &[u8] {
    // SAFETY: IoBuf supplies a stable pointer to buf_len initialized bytes for
    // this borrow; the returned slice cannot outlive the buffer reference.
    unsafe { std::slice::from_raw_parts(buf.as_buf_ptr(), buf.buf_len()) }
}

#[cfg(any(test, feature = "fs", feature = "process", feature = "stdio"))]
#[inline]
pub(crate) fn read_into_buf(
    buf: &mut impl IoBufMut,
    read: impl FnOnce(&mut [u8]) -> std::io::Result<usize>,
) -> std::io::Result<usize> {
    let capacity = buf.buf_capacity();
    if capacity == 0 {
        return Ok(0);
    }
    let initialized = buf.buf_len();
    let ptr = buf.as_buf_mut_ptr();
    // SAFETY: IoBufMut provides exclusive writable capacity and an initialized
    // prefix. Initialize only spare bytes before exposing a safe Rust slice:
    // even a safe Read implementation may inspect its destination contents.
    let slice = unsafe {
        ptr.add(initialized).write_bytes(0, capacity - initialized);
        std::slice::from_raw_parts_mut(ptr, capacity)
    };
    let count = read(slice)?;
    if count > capacity {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "reader reported more bytes than the supplied buffer capacity",
        ));
    }
    // SAFETY: the complete capacity was initialized before invoking read, and
    // the returned count was checked. Errors leave the original length intact.
    unsafe { buf.set_buf_init(count) };
    Ok(count)
}

#[cfg(test)]
mod tests {
    use super::{IoBuf, IoBufMut, IoBufTemporaryPoll, IoVectoredBuf, IoVectoredBufMut};

    #[test]
    fn vectored_emptiness_preserves_writable_spare_capacity() {
        struct Spare(Vec<u8>);
        // SAFETY: the Vec owns stable memory. The readable descriptor exposes
        // only initialized bytes, and moving this wrapper does not move them.
        unsafe impl IoVectoredBuf for Spare {
            fn as_iovecs(&self) -> Box<[super::IoVec]> {
                vec![super::IoVec {
                    ptr: self.0.as_ptr().cast_mut(),
                    len: self.0.len(),
                }]
                .into_boxed_slice()
            }
        }
        // SAFETY: the Vec exclusively owns its full writable capacity; its one
        // descriptor cannot overlap another. No initialized slice is formed.
        unsafe impl IoVectoredBufMut for Spare {
            fn as_iovecs_mut(&mut self) -> Box<[super::IoVec]> {
                vec![super::IoVec {
                    ptr: self.0.as_mut_ptr(),
                    len: self.0.capacity(),
                }]
                .into_boxed_slice()
            }
        }
        let mut spare = Spare(Vec::with_capacity(8));
        assert_eq!(spare.as_iovecs()[0].len, 0);
        assert!(
            !spare.is_empty(),
            "input must not skip uninitialized writable capacity"
        );
        assert!(spare.as_iovecs_mut()[0].len >= 8);
        let empty_segments: Vec<Box<[u8]>> = vec![Box::new([]), Box::new([])];
        assert!(!IoVectoredBuf::is_empty(&empty_segments));
        assert!(IoVectoredBuf::is_empty(&Vec::<Box<[u8]>>::new()));
        #[cfg(unix)]
        {
            use crate::vibeio::io::AsyncRead;
            let (socket, mut peer) = std::os::unix::net::UnixStream::pair().unwrap();
            std::io::Write::write_all(&mut peer, b"abc").unwrap();
            let runtime = crate::vibeio::RuntimeBuilder::new()
                .driver(crate::vibeio::DriverKind::Mio)
                .build()
                .unwrap();
            runtime.block_on(async move {
                let mut socket = crate::vibeio::net::UnixStream::from_std(socket).unwrap();
                let (result, buffer) = socket.read_vectored(spare).await;
                let count = result.unwrap();
                assert_eq!(count, 3);
                // SAFETY: the successful read initialized the returned-count
                // prefix inside the Vec's owned writable capacity.
                let received = unsafe { std::slice::from_raw_parts(buffer.0.as_ptr(), count) };
                assert_eq!(received, b"abc");
            });
        }
    }

    #[cfg(any(feature = "fs", feature = "process", feature = "stdio"))]
    #[test]
    fn blocking_read_initializes_spare_capacity_and_tracks_result_length() {
        let mut buf = Vec::with_capacity(8);
        buf.extend_from_slice(b"ab");
        let capacity = buf.capacity();
        let count = super::read_into_buf(&mut buf, |slice| {
            assert_eq!(slice.len(), capacity);
            assert_eq!(&slice[..2], b"ab");
            assert!(slice[2..].iter().all(|byte| *byte == 0));
            slice[..3].copy_from_slice(b"xyz");
            Ok(3)
        })
        .unwrap();
        assert_eq!(count, 3);
        assert_eq!(buf, b"xyz");
        super::read_into_buf(&mut buf, |_| Ok(0)).unwrap();
        assert!(buf.is_empty());
    }

    #[cfg(any(feature = "fs", feature = "process", feature = "stdio"))]
    #[test]
    fn blocking_read_errors_do_not_expose_unreported_bytes() {
        let mut buf = Vec::with_capacity(8);
        buf.extend_from_slice(b"ab");
        assert!(
            super::read_into_buf(&mut buf, |_| Err(std::io::ErrorKind::Interrupted.into()))
                .is_err()
        );
        assert_eq!(buf, b"ab");
        let capacity = buf.capacity();
        let error = super::read_into_buf(&mut buf, |_| Ok(capacity + 1)).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert_eq!(buf, b"ab");
        let mut empty = Vec::<u8>::new();
        assert_eq!(
            super::read_into_buf(&mut empty, |_| panic!("zero capacity must not read")).unwrap(),
            0
        );
    }

    #[cfg(unix)]
    #[test]
    fn raw_iovec_collections_do_not_implement_owned_buffer_traits() {
        // Inference becomes ambiguous (a compile error) if the unsafe blanket
        // implementation for unowned raw pointer collections is reintroduced.
        trait AmbiguousIfVectored<A> {
            fn check() {}
        }
        impl<T: ?Sized> AmbiguousIfVectored<()> for T {}
        struct ImplementsVectored;
        impl<T: IoVectoredBuf> AmbiguousIfVectored<ImplementsVectored> for T {}
        let _ = <Vec<libc::iovec> as AmbiguousIfVectored<_>>::check;
        let _ = <Box<[libc::iovec]> as AmbiguousIfVectored<_>>::check;
    }

    #[test]
    fn owned_vectors_keep_storage_stable_across_moves() {
        let buffers = vec![
            Box::<[u8]>::from(&b"abc"[..]),
            Box::<[u8]>::from(&b"def"[..]),
        ];
        let before = buffers.as_iovecs();
        let mut moved = Box::new(buffers);
        let after = moved.as_iovecs_mut();
        for (before, after) in before.iter().zip(after.iter()) {
            assert_eq!(before.ptr, after.ptr);
            assert_eq!(before.len, after.len);
        }
        assert_ne!(after[0].ptr, after[1].ptr);
        let empty: Vec<Box<[u8]>> = Vec::new();
        assert!(IoVectoredBuf::is_empty(&empty));
    }

    #[cfg(unix)]
    #[test]
    fn owned_vectors_round_trip_through_unix_stream() {
        use crate::vibeio::io::{AsyncRead, AsyncWrite};
        use std::io::{Read, Write};
        let runtime = crate::vibeio::executor::Runtime::new(
            crate::vibeio::driver::AnyDriver::new_mio().unwrap(),
        );
        runtime.block_on(async {
            let (stream, mut peer) = std::os::unix::net::UnixStream::pair().unwrap();
            let mut stream = crate::vibeio::net::UnixStream::from_std(stream).unwrap();
            peer.write_all(b"abcdef").unwrap();
            let buffers = vec![vec![0; 2].into_boxed_slice(), vec![0; 4].into_boxed_slice()];
            let (result, buffers) = stream.read_vectored(buffers).await;
            let read = result.unwrap();
            assert!(read > 0 && read <= 6);
            assert_eq!(&buffers.concat()[..read], &b"abcdef"[..read]);
            let (result, returned) = stream.write_vectored(buffers).await;
            let written = result.unwrap();
            assert!(written > 0 && written <= 6);
            let mut received = vec![0; written];
            peer.read_exact(&mut received).unwrap();
            assert_eq!(received, returned.concat()[..written]);
        });
    }

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
