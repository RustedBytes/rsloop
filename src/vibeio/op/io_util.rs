use std::io;
use std::task::{Context, Poll};

use mio::Interest;

use crate::vibeio::driver::AnyDriver;
use crate::vibeio::fd_inner::InnerRawHandle;

pub(super) use crate::vibeio::driver::completion_error;

#[cfg(test)]
mod completion_error_tests {
    use super::*;

    #[test]
    fn decoding_preserves_error_codes_and_rejects_unrepresentable_results() {
        for code in [1, 5, 38, 995, i32::MAX] {
            assert_eq!(completion_error(-code).raw_os_error(), Some(code));
        }
        for result in [i32::MIN, 0, 1, i32::MAX] {
            let error = completion_error(result);
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
            assert_eq!(error.raw_os_error(), None);
        }
    }
}

/// Normalize EOF reported either during submission or by a completed read.
pub(super) fn read_error_result(error: io::Error) -> io::Result<i32> {
    // Overlapped ReadFile can report EOF immediately or through its completion.
    // https://learn.microsoft.com/en-us/windows/win32/fileio/testing-for-the-end-of-a-file
    #[cfg(windows)]
    if error.raw_os_error() == Some(windows_sys::Win32::Foundation::ERROR_HANDLE_EOF as i32) {
        return Ok(0);
    }
    Err(error)
}

#[cfg(all(target_os = "linux", feature = "fs"))]
pub(super) fn positional_offset(offset: u64) -> io::Result<u64> {
    // Linux file offsets are signed. In particular, io_uring treats all-one
    // bits as a request to use AND advance the shared cursor, not a position.
    i64::try_from(offset).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "file offset exceeds signed 64-bit range",
        )
    })?;
    Ok(offset)
}

pub(super) fn iovec_count<T: TryFrom<usize>>(count: usize) -> io::Result<T> {
    T::try_from(count).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "too many I/O vectors for native count field",
        )
    })
}

#[cfg(unix)]
pub(super) fn iovec_to_system(bufs: &[crate::vibeio::io::IoVec]) -> Box<[libc::iovec]> {
    bufs.iter()
        .map(|buf| libc::iovec {
            iov_base: buf.ptr.cast(),
            iov_len: buf.len,
        })
        .collect()
}

#[cfg(any(target_os = "linux", windows, test))]
#[inline]
pub(super) fn completion_len(capacity: usize) -> io::Result<u32> {
    // CompletionIoResult stores successful counts in a signed i32, with
    // negative values reserved for errors. Reject before submitting any I/O.
    i32::try_from(capacity).map(|len| len as u32).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "buffer exceeds completion length limit",
        )
    })
}

#[cfg(any(windows, test))]
pub(super) fn completion_vectored_len(mut lengths: impl Iterator<Item = usize>) -> io::Result<u32> {
    let total = lengths
        .try_fold(0usize, |total, len| total.checked_add(len))
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "vectored length overflow"))?;
    completion_len(total)
}

#[cfg(unix)]
pub(super) fn set_cloexec(fd: std::os::fd::RawFd) -> Result<(), io::Error> {
    // SAFETY: F_GETFD has no pointer arguments and only queries descriptor flags.
    let fdflags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if fdflags == -1 {
        return Err(io::Error::last_os_error());
    }
    if fdflags & libc::FD_CLOEXEC == 0 {
        // SAFETY: F_SETFD consumes an integer flag set, preserving existing flags.
        let result = unsafe { libc::fcntl(fd, libc::F_SETFD, fdflags | libc::FD_CLOEXEC) };
        if result == -1 {
            return Err(io::Error::last_os_error());
        }
    }

    Ok(())
}

pub(crate) enum CompletionBuffer<B> {
    Inline(B),
    Boxed(Box<B>),
}

#[cfg(test)]
mod storage_tests {
    use super::*;

    #[cfg(all(target_os = "linux", feature = "fs"))]
    #[test]
    fn positional_entries_reject_negative_offsets_and_current_position_sentinel() {
        use crate::vibeio::op::{Op, ReadAtOp, WriteAtOp};
        let handle = InnerRawHandle::for_mock_completion(std::rc::Rc::new(AnyDriver::new_mock()));
        for offset in [i64::MAX as u64 + 1, u64::MAX - 1, u64::MAX] {
            let mut read = ReadAtOp::new(&handle, vec![7u8; 4], offset);
            assert_eq!(
                read.build_completion_entry(1).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
            assert_eq!(read.take_bufs(), vec![7; 4]);
            let mut write = WriteAtOp::new(&handle, vec![7u8; 4], offset);
            assert_eq!(
                write.build_completion_entry(2).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
            assert_eq!(write.take_bufs(), vec![7; 4]);
        }
        for offset in [0, 1, i64::MAX as u64] {
            assert!(
                ReadAtOp::new(&handle, vec![0u8; 1], offset)
                    .build_completion_entry(1)
                    .is_ok()
            );
            assert!(
                WriteAtOp::new(&handle, vec![0u8; 1], offset)
                    .build_completion_entry(2)
                    .is_ok()
            );
        }
    }

    #[test]
    fn read_errors_preserve_non_eof_failures() {
        let error = read_error_result(io::Error::new(io::ErrorKind::PermissionDenied, "denied"))
            .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(error.to_string(), "denied");
        let error = read_error_result(io::Error::from_raw_os_error(6)).unwrap_err();
        assert_eq!(error.raw_os_error(), Some(6));
        #[cfg(windows)]
        assert_eq!(
            read_error_result(io::Error::from_raw_os_error(38)).unwrap(),
            0
        );
        #[cfg(not(windows))]
        assert_eq!(
            read_error_result(io::Error::from_raw_os_error(38))
                .unwrap_err()
                .raw_os_error(),
            Some(38)
        );
    }

    #[cfg(all(windows, feature = "fs"))]
    #[test]
    fn windows_overlapped_file_reads_return_zero_at_eof() {
        use crate::vibeio::fd_inner::RawOsHandle;
        use crate::vibeio::op::{ReadAtOp, ReadOp, ReadvOp};
        use std::os::windows::{fs::OpenOptionsExt, io::AsRawHandle};
        use windows_sys::Win32::Storage::FileSystem::{
            FILE_FLAG_DELETE_ON_CLOSE, FILE_FLAG_OVERLAPPED,
        };

        let path = std::env::temp_dir().join(format!(
            "vibeio-eof-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .custom_flags(FILE_FLAG_OVERLAPPED | FILE_FLAG_DELETE_ON_CLOSE)
            .open(path)
            .unwrap();
        let runtime = crate::vibeio::RuntimeBuilder::new()
            .driver(crate::vibeio::DriverKind::Iocp)
            .enable_timer(true)
            .build()
            .unwrap();
        runtime.block_on(async move {
            let handle = InnerRawHandle::new(
                RawOsHandle::Handle(file.as_raw_handle()),
                Interest::READABLE,
            )
            .unwrap();
            let mut scalar = ReadOp::new(&handle, vec![7u8; 4]);
            assert_eq!(
                crate::vibeio::time::timeout(
                    crate::vibeio::test_support::WATCHDOG,
                    std::future::poll_fn(|cx| handle.poll_op(cx, &mut scalar))
                )
                .await
                .unwrap()
                .unwrap(),
                0
            );
            assert!(scalar.take_bufs().is_empty());
            for offset in [0, 10] {
                let mut positional = ReadAtOp::new(&handle, vec![7u8; 4], offset);
                assert_eq!(
                    crate::vibeio::time::timeout(
                        crate::vibeio::test_support::WATCHDOG,
                        std::future::poll_fn(|cx| handle.poll_op(cx, &mut positional))
                    )
                    .await
                    .unwrap()
                    .unwrap(),
                    0
                );
                assert!(positional.take_bufs().is_empty());
            }
            let buffers = vec![
                vec![7u8; 2].into_boxed_slice(),
                vec![9u8; 2].into_boxed_slice(),
            ];
            let mut vectored = ReadvOp::new(&handle, buffers.clone());
            assert_eq!(
                crate::vibeio::time::timeout(
                    crate::vibeio::test_support::WATCHDOG,
                    std::future::poll_fn(|cx| handle.poll_op(cx, &mut vectored))
                )
                .await
                .unwrap()
                .unwrap(),
                0
            );
            assert_eq!(vectored.take_bufs(), buffers);
            let buffers = vec![
                vec![].into_boxed_slice(),
                b"ab".to_vec().into_boxed_slice(),
                vec![].into_boxed_slice(),
                b"cd".to_vec().into_boxed_slice(),
            ];
            let mut write = crate::vibeio::op::WritevOp::new(&handle, buffers.clone());
            assert_eq!(
                crate::vibeio::time::timeout(
                    crate::vibeio::test_support::WATCHDOG,
                    std::future::poll_fn(|cx| handle.poll_op(cx, &mut write))
                )
                .await
                .unwrap()
                .unwrap(),
                4
            );
            assert_eq!(write.take_bufs(), buffers);
            let mut read = ReadAtOp::new(&handle, Vec::<u8>::with_capacity(4), 0);
            assert_eq!(
                crate::vibeio::time::timeout(
                    crate::vibeio::test_support::WATCHDOG,
                    std::future::poll_fn(|cx| handle.poll_op(cx, &mut read))
                )
                .await
                .unwrap()
                .unwrap(),
                4
            );
            assert_eq!(read.take_bufs(), b"abcd");
            let mut invalid = crate::vibeio::op::WriteAtOp::new(&handle, b"BAD".to_vec(), u64::MAX);
            let result = std::future::poll_fn(|cx| handle.poll_op(cx, &mut invalid)).await;
            assert_eq!(result.unwrap_err().kind(), io::ErrorKind::InvalidInput);
            assert_eq!(invalid.take_bufs(), b"BAD");
            assert_eq!(
                file.metadata().unwrap().len(),
                4,
                "invalid offset must not append"
            );
        });
    }

    #[cfg(windows)]
    #[test]
    fn pending_windows_receives_outlive_submission_metadata() {
        use crate::vibeio::fd_inner::RawOsHandle;
        use crate::vibeio::op::{Op, ReadOp, ReadvOp, RecvOp};
        use std::io::Write;
        use std::os::windows::io::AsRawSocket;
        use std::sync::mpsc;

        async fn receive<O: Op<Output = usize>>(
            handle: &InnerRawHandle,
            op: &mut O,
            signal: &mpsc::Sender<()>,
        ) {
            let mut signalled = false;
            let read = std::future::poll_fn(|cx| {
                let result = handle.poll_op(cx, op);
                // Release the sender only after submission has returned Pending,
                // when the stack-local WSABUF and flags no longer exist.
                if result.is_pending() && !signalled {
                    signal.send(()).unwrap();
                    signalled = true;
                }
                result
            });
            assert_eq!(
                crate::vibeio::time::timeout(crate::vibeio::test_support::WATCHDOG, read)
                    .await
                    .unwrap()
                    .unwrap(),
                1
            );
            assert!(signalled);
        }

        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let mut peer = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        let (socket, _) = listener.accept().unwrap();
        let (signal, ready) = mpsc::channel();
        let writer = std::thread::spawn(move || {
            for _ in 0..3 {
                ready
                    .recv_timeout(crate::vibeio::test_support::WATCHDOG)
                    .unwrap();
                peer.write_all(b"x").unwrap();
            }
        });
        let runtime = crate::vibeio::executor::Runtime::new(AnyDriver::new_iocp().unwrap());
        runtime.block_on(async move {
            let handle = InnerRawHandle::new(
                RawOsHandle::Socket(socket.as_raw_socket()),
                Interest::READABLE,
            )
            .unwrap();
            let mut read = ReadOp::new(&handle, Vec::<u8>::with_capacity(8));
            receive(&handle, &mut read, &signal).await;
            assert_eq!(read.take_bufs(), b"x");
            let mut recv = RecvOp::new(&handle, Vec::<u8>::with_capacity(8));
            receive(&handle, &mut recv, &signal).await;
            assert_eq!(recv.take_bufs(), b"x");
            let mut readv = ReadvOp::new(
                &handle,
                vec![vec![].into_boxed_slice(), vec![0u8].into_boxed_slice()],
            );
            receive(&handle, &mut readv, &signal).await;
            assert_eq!(&*readv.take_bufs()[1], b"x");
        });
        writer.join().unwrap();
    }

    #[test]
    fn native_iovec_counts_do_not_wrap() {
        for count in [0, 1, 1024, i32::MAX as usize] {
            assert_eq!(iovec_count::<i32>(count).unwrap() as usize, count);
            assert_eq!(iovec_count::<u32>(count).unwrap() as usize, count);
        }
        assert_eq!(
            iovec_count::<i32>(i32::MAX as usize + 1)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
        assert_eq!(iovec_count::<u32>(u32::MAX as usize).unwrap(), u32::MAX);
        #[cfg(target_pointer_width = "64")]
        assert_eq!(
            iovec_count::<u32>(u32::MAX as usize + 1)
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn completion_entries_transfer_expected_bytes() {
        use crate::vibeio::op::{Op, ReadOp, ReadvOp, RecvOp, SendOp, WriteOp, WritevOp};
        use std::io::{Read, Write};
        use std::os::fd::AsRawFd;
        use std::os::unix::net::UnixStream;
        use std::rc::Rc;

        fn complete(entry: io_uring::squeue::Entry) -> usize {
            // Keep the ring local so it is closed before the caller's operation
            // and payload can be dropped, including if submission fails.
            let mut ring = io_uring::IoUring::new(2).unwrap();
            // SAFETY: each caller below retains its operation and socket through
            // this call; no buffer or metadata moves before CQE acknowledgement.
            unsafe { ring.submission().push(&entry).unwrap() };
            ring.submit_and_wait(1).unwrap();
            let result = ring.completion().next().unwrap().result();
            assert!(result >= 0, "completion failed: {result}");
            result as usize
        }

        let (socket, mut peer) = UnixStream::pair().unwrap();
        peer.set_read_timeout(Some(crate::vibeio::test_support::WATCHDOG))
            .unwrap();
        let mut handle = InnerRawHandle::for_mock_completion(Rc::new(AnyDriver::new_mock()));
        handle.handle = socket.as_raw_fd();

        peer.write_all(b"read").unwrap();
        let mut read = ReadOp::new(&handle, vec![0u8; 8]);
        assert_eq!(complete(read.build_completion_entry(1).unwrap()), 4);
        assert_eq!(&read.take_bufs()[..4], b"read");

        peer.write_all(b"recv").unwrap();
        let mut recv = RecvOp::new(&handle, vec![0u8; 8]);
        assert_eq!(complete(recv.build_completion_entry(2).unwrap()), 4);
        assert_eq!(&recv.take_bufs()[..4], b"recv");

        let mut send = SendOp::new(&handle, b"send".to_vec());
        assert_eq!(complete(send.build_completion_entry(3).unwrap()), 4);
        assert_eq!(send.take_bufs(), b"send");
        let mut output = [0; 4];
        peer.read_exact(&mut output).unwrap();
        assert_eq!(&output, b"send");

        let mut write = WriteOp::new(&handle, b"write".to_vec());
        assert_eq!(complete(write.build_completion_entry(4).unwrap()), 5);
        assert_eq!(write.take_bufs(), b"write");
        let mut output = [0; 5];
        peer.read_exact(&mut output).unwrap();
        assert_eq!(&output, b"write");

        peer.write_all(b"abc").unwrap();
        let mut read = ReadvOp::new(
            &handle,
            vec![
                vec![9u8; 2].into_boxed_slice(),
                vec![9u8; 2].into_boxed_slice(),
            ],
        );
        assert_eq!(complete(read.build_completion_entry(5).unwrap()), 3);
        let buffers = read.take_bufs();
        assert_eq!(&*buffers[0], b"ab");
        assert_eq!(&*buffers[1], &[b'c', 9]);

        let buffers = vec![
            vec![].into_boxed_slice(),
            b"ab".to_vec().into_boxed_slice(),
            b"cd".to_vec().into_boxed_slice(),
        ];
        let mut write = WritevOp::new(&handle, buffers);
        assert_eq!(complete(write.build_completion_entry(6).unwrap()), 4);
        assert_eq!(write.take_bufs().len(), 3);
        let mut output = [0; 4];
        peer.read_exact(&mut output).unwrap();
        assert_eq!(&output, b"abcd");
    }

    #[test]
    fn completion_lengths_do_not_wrap() {
        for capacity in [0, 1, 4096, i32::MAX as usize] {
            assert_eq!(completion_len(capacity).unwrap() as usize, capacity);
        }
        for capacity in [i32::MAX as usize + 1, u32::MAX as usize, usize::MAX] {
            assert_eq!(
                completion_len(capacity).unwrap_err().kind(),
                io::ErrorKind::InvalidInput
            );
        }
    }

    #[test]
    fn vectored_completion_lengths_fit_the_signed_result() {
        let limit = i32::MAX as usize;
        assert_eq!(
            completion_vectored_len([0, limit - 1, 1].into_iter()).unwrap(),
            limit as u32
        );
        assert_eq!(completion_vectored_len([].into_iter()).unwrap(), 0);
        for lengths in [[limit, 1], [usize::MAX, 1]] {
            assert_eq!(
                completion_vectored_len(lengths.into_iter())
                    .unwrap_err()
                    .kind(),
                io::ErrorKind::InvalidInput
            );
        }
    }
    use crate::vibeio::io::{IoBuf, IoBufMut};

    #[test]
    fn inline_array_keeps_address_through_completion_storage_moves() {
        let mut storage = CompletionBuffer::new([7u8; 32], true);
        let pointer = storage.as_mut().as_buf_mut_ptr();
        let moved = Box::new(storage);
        assert_eq!(moved.as_ref().as_ref().as_buf_ptr(), pointer);
        let retained = (*moved).into_stable_box();
        assert_eq!(retained.as_ref().as_buf_ptr(), pointer);
        assert_eq!(retained.as_ref(), &[7; 32]);
        // Model the type-erased driver payload and eventual acknowledgement.
        let payload: Box<dyn std::any::Any> = Box::new(Some(retained));
        let acknowledged = payload
            .downcast::<Option<Box<[u8; 32]>>>()
            .unwrap()
            .unwrap();
        assert_eq!(acknowledged.as_ref().as_buf_ptr(), pointer);
        assert_eq!(*acknowledged, [7; 32]);
    }

    #[test]
    fn poll_storage_returns_inline_array_without_stability_requirement() {
        let storage = CompletionBuffer::new([5u8; 8], false);
        assert!(matches!(storage, CompletionBuffer::Inline(_)));
        assert_eq!(storage.into_inner(), [5; 8]);
    }
}

impl<B> CompletionBuffer<B> {
    #[inline]
    pub(crate) fn new(buf: B, stable: bool) -> Self {
        if stable {
            Self::Boxed(Box::new(buf))
        } else {
            Self::Inline(buf)
        }
    }

    #[inline]
    pub(crate) fn as_ref(&self) -> &B {
        match self {
            Self::Inline(buf) => buf,
            Self::Boxed(buf) => buf.as_ref(),
        }
    }

    #[inline]
    pub(crate) fn as_mut(&mut self) -> &mut B {
        match self {
            Self::Inline(buf) => buf,
            Self::Boxed(buf) => buf.as_mut(),
        }
    }

    #[inline]
    pub(crate) fn into_inner(self) -> B {
        match self {
            Self::Inline(buf) => buf,
            Self::Boxed(buf) => *buf,
        }
    }

    #[inline]
    pub(crate) fn into_stable_box(self) -> Box<B> {
        match self {
            Self::Inline(buf) => Box::new(buf),
            Self::Boxed(buf) => buf,
        }
    }
}

#[inline]
pub(crate) fn poll_result_or_wait(
    result: io::Result<usize>,
    handle: &InnerRawHandle,
    cx: &mut Context<'_>,
    driver: &AnyDriver,
    interest: Interest,
) -> Poll<io::Result<usize>> {
    match result {
        Ok(value) => Poll::Ready(Ok(value)),
        Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
            if let Err(submit_err) = driver.submit_poll(handle, cx.waker().clone(), interest) {
                Poll::Ready(Err(submit_err))
            } else {
                Poll::Pending
            }
        }
        Err(err) => Poll::Ready(Err(err)),
    }
}

#[cfg(test)]
pub(crate) mod cancellation_tests {
    use super::*;
    use crate::vibeio::io::{IoBuf, IoBufMut, IoVec, IoVectoredBuf, IoVectoredBufMut};
    use std::{rc::Rc, sync::Arc};

    pub(crate) struct TrackedBuffer {
        bytes: Box<[u8]>,
        _lifetime: Arc<()>,
    }

    impl TrackedBuffer {
        pub(crate) fn new(lifetime: Arc<()>) -> Self {
            Self {
                bytes: vec![0; 8].into_boxed_slice(),
                _lifetime: lifetime,
            }
        }
    }

    // SAFETY: the box owns eight initialized bytes at a stable address.
    unsafe impl IoBuf for TrackedBuffer {
        fn as_buf_ptr(&self) -> *const u8 {
            self.bytes.as_ptr()
        }
        fn buf_len(&self) -> usize {
            self.bytes.len()
        }
        fn buf_capacity(&self) -> usize {
            self.bytes.len()
        }
    }
    // SAFETY: all bytes are initialized and exclusively owned by this buffer.
    unsafe impl IoBufMut for TrackedBuffer {
        fn as_buf_mut_ptr(&mut self) -> *mut u8 {
            self.bytes.as_mut_ptr()
        }
        unsafe fn set_buf_init(&mut self, _: usize) {}
    }
    // SAFETY: the sole vector points into the buffer's owned, stable allocation.
    unsafe impl IoVectoredBuf for TrackedBuffer {
        fn as_iovecs(&self) -> Box<[IoVec]> {
            vec![IoVec {
                ptr: self.bytes.as_ptr().cast_mut(),
                len: self.bytes.len(),
            }]
            .into_boxed_slice()
        }
    }
    // SAFETY: the sole writable region is exclusively borrowed and cannot overlap.
    unsafe impl IoVectoredBufMut for TrackedBuffer {
        fn as_iovecs_mut(&mut self) -> Box<[IoVec]> {
            vec![IoVec {
                ptr: self.bytes.as_mut_ptr(),
                len: self.bytes.len(),
            }]
            .into_boxed_slice()
        }
    }

    pub(crate) fn check_cancellation(
        cancel: impl Fn(&InnerRawHandle, TrackedBuffer, bool) + Copy + 'static,
    ) {
        for entered in [false, true] {
            for reclaim in [false, true] {
                let owner = Rc::new(AnyDriver::new_mock());
                let handle = InnerRawHandle::for_mock_completion(owner.clone());
                let lifetime = Arc::new(());
                let weak = Arc::downgrade(&lifetime);
                let buffer = TrackedBuffer::new(lifetime);
                if entered {
                    // A different runtime must not receive the owner's token.
                    let runtime = crate::vibeio::executor::Runtime::new(AnyDriver::new_mock());
                    runtime.block_on(async move { cancel(&handle, buffer, reclaim) });
                } else {
                    assert!(crate::vibeio::current_driver().is_none());
                    cancel(&handle, buffer, reclaim);
                }
                let AnyDriver::Mock(driver) = owner.as_ref() else {
                    unreachable!()
                };
                let held = driver.ignored.take();
                assert_eq!(held.len(), 1, "cancellation missed the owning driver");
                assert_eq!(held[0].0, 41);
                assert!(
                    weak.upgrade().is_some(),
                    "buffer released before completion"
                );
                drop(held); // model completion acknowledgement
                assert!(
                    weak.upgrade().is_none(),
                    "buffer retained after acknowledgement"
                );
            }
        }
    }
}
