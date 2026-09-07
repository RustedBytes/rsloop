#![warn(clippy::undocumented_unsafe_blocks)]

use std::io;
use std::task::{Context, Poll};

use mio::Interest;
#[cfg(windows)]
use windows_sys::Win32::{
    Foundation::{ERROR_IO_PENDING, HANDLE},
    Networking::WinSock::{self as WinSock, SOCKET, WSA_IO_PENDING, WSABUF},
    Storage::FileSystem::WriteFile,
    System::IO::OVERLAPPED,
};

use crate::vibeio::driver::AnyDriver;
use crate::vibeio::driver::CompletionIoResult;
use crate::vibeio::fd_inner::InnerRawHandle;
#[cfg(windows)]
use crate::vibeio::fd_inner::RawOsHandle;
use crate::vibeio::io::IoVectoredBuf;
use crate::vibeio::op::Op;
#[cfg(unix)]
use crate::vibeio::op::io_util::iovec_to_system;
use crate::vibeio::op::io_util::{iovec_count, poll_result_or_wait};

#[cfg(windows)]
#[inline]
fn socket_write_vectored<B: IoVectoredBuf>(socket: SOCKET, bufs: &B) -> io::Result<usize> {
    use windows_sys::Win32::Networking::WinSock::{self as WinSock, SOCKET_ERROR, WSABUF};

    let iovecs = bufs.as_iovecs();
    let mut wsabufs = Vec::with_capacity(iovecs.len());
    for iovec in iovecs {
        let len = crate::vibeio::op::io_util::completion_len(iovec.len).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "writev buffer is too large for Windows socket I/O",
            )
        })?;
        wsabufs.push(WSABUF {
            len,
            buf: iovec.ptr as *mut _,
        });
    }

    let mut bytes: u32 = 0;
    // SAFETY: IoVectoredBuf owns initialized, stable payloads. The checked
    // descriptors and bytes output remain valid through this non-overlapped
    // call; neither their addresses nor the payload pointers are retained.
    let send_result = unsafe {
        WinSock::WSASend(
            socket,
            wsabufs.as_mut_ptr(),
            iovec_count(wsabufs.len())?,
            &mut bytes,
            0,
            std::ptr::null_mut(),
            None,
        )
    };
    if send_result == SOCKET_ERROR {
        // SAFETY: reads the calling thread's Winsock error without pointer access.
        return Err(io::Error::from_raw_os_error(unsafe {
            WinSock::WSAGetLastError()
        }));
    }

    Ok(bytes as usize)
}

pub struct WritevOp<'a, B: IoVectoredBuf> {
    handle: &'a InnerRawHandle,
    bufs: Option<B>,
    completion_token: Option<usize>,
    #[cfg(windows)]
    completion_staging: Option<Vec<u8>>,
    #[cfg(target_os = "linux")]
    completion_system_iovecs: Option<Box<[libc::iovec]>>,
}

impl<'a, B: IoVectoredBuf> WritevOp<'a, B> {
    #[inline]
    pub fn new(handle: &'a InnerRawHandle, bufs: B) -> Self {
        Self {
            handle,
            bufs: Some(bufs),
            completion_token: None,
            #[cfg(windows)]
            completion_staging: None,
            #[cfg(target_os = "linux")]
            completion_system_iovecs: None,
        }
    }

    #[inline]
    pub fn take_bufs(mut self) -> B {
        assert!(
            self.completion_token.is_none(),
            "cannot reclaim a buffer while I/O is pending"
        );
        self.bufs.take().unwrap()
    }
}

impl<B: IoVectoredBuf> Op for WritevOp<'_, B> {
    type Output = usize;

    #[cfg(any(unix, windows))]
    #[inline]
    fn poll_poll(
        &mut self,
        cx: &mut Context<'_>,
        driver: &AnyDriver,
    ) -> Poll<io::Result<Self::Output>> {
        let bufs = self.bufs.as_ref().unwrap();

        #[cfg(unix)]
        let result = {
            let iovecs = bufs.as_iovecs();
            let iovecs_system = iovec_to_system(&iovecs);
            // SAFETY: every descriptor refers to initialized memory owned by
            // bufs, and both payloads and descriptor array outlive this call.
            let written = unsafe {
                libc::writev(
                    self.handle.handle,
                    iovecs_system.as_ptr(),
                    iovec_count(iovecs_system.len())?,
                )
            };
            if written == -1 {
                Err(io::Error::last_os_error())
            } else {
                Ok(written as usize)
            }
        };

        #[cfg(windows)]
        let result = match self.handle.handle {
            RawOsHandle::Socket(socket) => socket_write_vectored(socket as SOCKET, bufs),
            RawOsHandle::Handle(_) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "poll-based writev currently supports sockets only on Windows",
            )),
        };

        poll_result_or_wait(result, self.handle, cx, driver, Interest::WRITABLE)
    }

    #[cfg(any(unix, windows))]
    #[inline]
    fn poll_completion(
        &mut self,
        cx: &mut Context<'_>,
        driver: &AnyDriver,
    ) -> Poll<io::Result<Self::Output>> {
        let result = if let Some(completion_token) = self.completion_token {
            // Get the completion result
            match driver.get_completion_result(completion_token) {
                Some(result) => {
                    self.completion_token = None;
                    result
                }
                None => {
                    // The completion is not ready yet
                    driver.set_completion_waker(completion_token, cx.waker().clone());
                    return Poll::Pending;
                }
            }
        } else {
            // Submit the op
            match driver.submit_completion(self, cx.waker().clone()) {
                CompletionIoResult::Ok(result) => result,
                CompletionIoResult::Retry(token) => {
                    self.completion_token = Some(token);
                    return Poll::Pending;
                }
                CompletionIoResult::SubmitErr(err) => return Poll::Ready(Err(err)),
            }
        };
        if result < 0 {
            #[cfg(windows)]
            {
                self.completion_staging = None;
            }
            return Poll::Ready(Err(crate::vibeio::op::io_util::completion_error(result)));
        }

        #[cfg(windows)]
        {
            self.completion_staging = None;
        }

        Poll::Ready(Ok(result as usize))
    }

    #[cfg(windows)]
    #[inline]
    fn submit_windows(&mut self, overlapped: *mut OVERLAPPED) -> Result<(), io::Error> {
        let bufs = self.bufs.as_ref().unwrap();
        match self.handle.handle {
            RawOsHandle::Socket(socket) => {
                let iovecs = bufs.as_iovecs();
                crate::vibeio::op::io_util::completion_vectored_len(
                    iovecs.iter().map(|iov| iov.len),
                )?;
                let mut wsabufs = Vec::with_capacity(iovecs.len());
                for iovec in iovecs {
                    let len =
                        crate::vibeio::op::io_util::completion_len(iovec.len).map_err(|_| {
                            io::Error::new(
                                io::ErrorKind::InvalidInput,
                                "writev buffer is too large for Windows socket I/O",
                            )
                        })?;
                    wsabufs.push(WSABUF {
                        len,
                        buf: iovec.ptr as *mut _,
                    });
                }

                // SAFETY: Winsock captures the WSABUF descriptors before return.
                // Their Vec need only survive this call; payloads remain owned
                // through completion/cancellation, and the driver retains the
                // OVERLAPPED until acknowledgement.
                // https://learn.microsoft.com/en-us/windows/win32/api/winsock2/nf-winsock2-wsasend
                let send_result = unsafe {
                    WinSock::WSASend(
                        socket as SOCKET,
                        wsabufs.as_mut_ptr(),
                        iovec_count(wsabufs.len())?,
                        std::ptr::null_mut(),
                        0,
                        overlapped,
                        None,
                    )
                };

                if send_result == 0 {
                    self.completion_staging = None;
                    return Ok(());
                }

                // SAFETY: reads thread-local Winsock error after failed submission.
                let err = unsafe { WinSock::WSAGetLastError() };
                if err == WSA_IO_PENDING {
                    self.completion_staging = None;
                    Ok(())
                } else {
                    self.completion_staging = None;
                    Err(io::Error::from_raw_os_error(err))
                }
            }
            RawOsHandle::Handle(handle) => {
                let iovecs = bufs.as_iovecs();
                let total_len = (iovecs.iter()).try_fold(0usize, |acc, iovec| {
                    acc.checked_add(iovec.len).ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidInput, "writev buffer length overflow")
                    })
                })?;
                let total_len_u32 =
                    crate::vibeio::op::io_util::completion_len(total_len).map_err(|_| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "writev total length is too large for Windows file I/O",
                        )
                    })?;

                let mut staging = Vec::with_capacity(total_len);
                for iovec in iovecs {
                    if iovec.len == 0 {
                        continue;
                    }
                    // SAFETY: IoVectoredBuf guarantees initialized, stable
                    // readable regions throughout this borrow. Copy into a
                    // separate allocation before submitting the file write.
                    let slice = unsafe { std::slice::from_raw_parts(iovec.ptr, iovec.len) };
                    staging.extend_from_slice(slice);
                }

                // SAFETY: staging owns the initialized concatenated bytes and
                // remains retained on successful/pending submission. The driver
                // keeps OVERLAPPED alive through completion or cancellation.
                let write_result = unsafe {
                    WriteFile(
                        handle as HANDLE,
                        staging.as_ptr().cast(),
                        total_len_u32,
                        std::ptr::null_mut(),
                        overlapped,
                    )
                };

                if write_result != 0 {
                    self.completion_staging = Some(staging);
                    return Ok(());
                }

                let err = io::Error::last_os_error();
                if err.raw_os_error() == Some(ERROR_IO_PENDING as i32) {
                    self.completion_staging = Some(staging);
                    Ok(())
                } else {
                    self.completion_staging = None;
                    Err(err)
                }
            }
        }
    }

    #[cfg(target_os = "linux")]
    #[inline]
    fn build_completion_entry(
        &mut self,
        user_data: u64,
    ) -> Result<io_uring::squeue::Entry, io::Error> {
        use io_uring::{opcode, types};

        let bufs = self.bufs.as_ref().unwrap();

        // Build a temporary iovec array for the syscall.
        let iovecs = if let Some(iovecs) = self.completion_system_iovecs.take() {
            iovecs
        } else {
            iovec_to_system(&bufs.as_iovecs())
        };

        let entry = opcode::Writev::new(
            types::Fd(self.handle.handle),
            iovecs.as_ptr(),
            iovec_count(iovecs.len())?,
        )
        .build()
        .user_data(user_data);

        // Store the iovec array for the completion, because it needs to be kept alive until the
        // completion is ready.
        self.completion_system_iovecs = Some(iovecs);

        Ok(entry)
    }
}

impl<B: IoVectoredBuf> Drop for WritevOp<'_, B> {
    #[inline]
    fn drop(&mut self) {
        if let Some(token) = self.completion_token.take() {
            #[cfg(target_os = "linux")]
            let completion_state = self.completion_system_iovecs.take();
            #[cfg(windows)]
            let completion_state = self.completion_staging.take();
            #[cfg(not(any(target_os = "linux", windows)))]
            let completion_state = ();
            // The owning driver, not the currently entered runtime, must retain
            // every kernel-visible allocation until completion is acknowledged.
            self.handle
                .cancel_completion(token, Box::new((completion_state, self.bufs.take())));
        }
    }
}

#[cfg(test)]
mod cancellation_tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn cancelled_write_retains_descriptor_array_and_payload_addresses() {
        let owner = std::rc::Rc::new(AnyDriver::new_mock());
        let handle = InnerRawHandle::for_mock_completion(owner.clone());
        let buffers = vec![
            Box::<[u8]>::from([]),
            Box::<[u8]>::from(b"payload".as_slice()),
        ];
        let payload = buffers[1].as_ptr();
        let mut op = WritevOp::new(&handle, buffers);
        op.build_completion_entry(41).unwrap();
        let descriptors = op.completion_system_iovecs.as_ref().unwrap().as_ptr();
        op.completion_token = Some(41);
        drop(op);
        let AnyDriver::Mock(driver) = owner.as_ref() else {
            unreachable!()
        };
        let held = driver.ignored.take();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].0, 41);
        let (iovecs, buffers) = held[0]
            .1
            .downcast_ref::<(Option<Box<[libc::iovec]>>, Option<Vec<Box<[u8]>>>)>()
            .unwrap();
        let iovecs = iovecs.as_ref().unwrap();
        assert_eq!(iovecs.as_ptr(), descriptors);
        assert_eq!(iovecs.len(), 2);
        assert_eq!(iovecs[0].iov_len, 0);
        assert_eq!(iovecs[1].iov_base.cast_const().cast::<u8>(), payload);
        assert_eq!(iovecs[1].iov_len, 7);
        assert_eq!(buffers.as_ref().unwrap()[1].as_ptr(), payload);
        assert_eq!(&*buffers.as_ref().unwrap()[1], b"payload");
        drop(held); // Model acknowledgement; no actual kernel request was made.
    }

    #[cfg(windows)]
    #[test]
    fn cancelled_file_write_retains_staging_allocation() {
        let owner = std::rc::Rc::new(AnyDriver::new_mock());
        let handle = InnerRawHandle::for_mock_completion(owner.clone());
        let mut op = WritevOp::new(&handle, vec![b"payload".to_vec().into_boxed_slice()]);
        let staging = b"payload".to_vec();
        let ptr = staging.as_ptr();
        op.completion_staging = Some(staging);
        op.completion_token = Some(41);
        drop(op);
        let AnyDriver::Mock(driver) = owner.as_ref() else {
            unreachable!()
        };
        let held = driver.ignored.take();
        assert_eq!(held.len(), 1);
        assert_eq!(held[0].0, 41);
        let (staging, buffers) = held[0]
            .1
            .downcast_ref::<(Option<Vec<u8>>, Option<Vec<Box<[u8]>>>)>()
            .unwrap();
        let staging = staging.as_ref().unwrap();
        assert_eq!(staging.as_ptr(), ptr);
        assert_eq!(staging, b"payload");
        assert_eq!(&*buffers.as_ref().unwrap()[0], b"payload");
        drop(held); // Model acknowledgement releasing all retained storage.
    }

    #[test]
    fn pending_buffer_is_retained_by_owning_driver() {
        crate::vibeio::op::io_util::cancellation_tests::check_cancellation(
            |handle, buffer, reclaim| {
                let mut op = WritevOp::new(handle, buffer);
                op.completion_token = Some(41);
                if reclaim {
                    let result =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| op.take_bufs()));
                    assert!(result.is_err(), "pending storage must not be reclaimed");
                } else {
                    drop(op);
                }
            },
        );
    }
}
