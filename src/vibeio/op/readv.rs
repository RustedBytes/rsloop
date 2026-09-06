#![warn(clippy::undocumented_unsafe_blocks)]

use std::io;
use std::task::{Context, Poll};

use mio::Interest;
#[cfg(windows)]
use windows_sys::Win32::{
    Foundation::{ERROR_IO_PENDING, HANDLE},
    Networking::WinSock::{self as WinSock, SOCKET, WSA_IO_PENDING, WSABUF},
    Storage::FileSystem::ReadFile,
    System::IO::OVERLAPPED,
};

use crate::vibeio::driver::AnyDriver;
use crate::vibeio::fd_inner::InnerRawHandle;
#[cfg(windows)]
use crate::vibeio::fd_inner::RawOsHandle;
use crate::vibeio::op::Op;
#[cfg(unix)]
use crate::vibeio::op::io_util::iovec_to_system;
use crate::vibeio::op::io_util::{iovec_count, poll_result_or_wait};
use crate::vibeio::{driver::CompletionIoResult, io::IoVectoredBufMut};

#[cfg(windows)]
#[inline]
fn socket_read_vectored<B: IoVectoredBufMut>(socket: SOCKET, bufs: &mut B) -> io::Result<usize> {
    use windows_sys::Win32::Networking::WinSock::{self as WinSock, SOCKET_ERROR, WSABUF};

    let iovecs = bufs.as_iovecs_mut();
    let mut wsabufs = Vec::with_capacity(iovecs.len());
    for iovec in iovecs {
        let len = u32::try_from(iovec.len).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "readv buffer is too large for Windows socket I/O",
            )
        })?;
        wsabufs.push(WSABUF {
            len,
            buf: iovec.ptr as *mut _,
        });
    }

    let mut bytes: u32 = 0;
    let mut flags: u32 = 0;
    // SAFETY: IoVectoredBufMut owns stable, disjoint writable regions. The
    // checked WSABUF descriptors and output integers live through this
    // non-overlapped call, which does not retain their addresses afterward.
    let recv_result = unsafe {
        WinSock::WSARecv(
            socket,
            wsabufs.as_mut_ptr(),
            iovec_count(wsabufs.len())?,
            &mut bytes,
            &mut flags,
            std::ptr::null_mut(),
            None,
        )
    };
    if recv_result == SOCKET_ERROR {
        // SAFETY: reads the calling thread's Winsock error without pointer access.
        return Err(io::Error::from_raw_os_error(unsafe {
            WinSock::WSAGetLastError()
        }));
    }

    Ok(bytes as usize)
}

pub struct ReadvOp<'a, B: IoVectoredBufMut> {
    handle: &'a InnerRawHandle,
    bufs: Option<B>,
    completion_token: Option<usize>,
    #[cfg(windows)]
    completion_staging: Option<Vec<u8>>,
    #[cfg(target_os = "linux")]
    completion_system_iovecs: Option<Box<[libc::iovec]>>,
}

impl<'a, B: IoVectoredBufMut> ReadvOp<'a, B> {
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

impl<B: IoVectoredBufMut> Op for ReadvOp<'_, B> {
    type Output = usize;

    #[cfg(any(unix, windows))]
    #[inline]
    fn poll_poll(
        &mut self,
        cx: &mut Context<'_>,
        driver: &AnyDriver,
    ) -> Poll<io::Result<Self::Output>> {
        let bufs = self.bufs.as_mut().unwrap();
        #[cfg(unix)]
        let result = {
            let mut iovecs = iovec_to_system(&bufs.as_iovecs_mut());
            // SAFETY: the owned buffer provides disjoint writable regions;
            // descriptors remain live through this synchronous call and their
            // count is checked before conversion to the native integer type.
            let read = unsafe {
                libc::readv(
                    self.handle.handle,
                    iovecs.as_mut_ptr(),
                    iovec_count(iovecs.len())?,
                )
            };
            if read == -1 {
                Err(io::Error::last_os_error())
            } else {
                Ok(read as usize)
            }
        };

        #[cfg(windows)]
        let result = match self.handle.handle {
            RawOsHandle::Socket(socket) => socket_read_vectored(socket as SOCKET, bufs),
            RawOsHandle::Handle(_) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "poll-based readv currently supports sockets only on Windows",
            )),
        };

        poll_result_or_wait(result, self.handle, cx, driver, Interest::READABLE)
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
                CompletionIoResult::SubmitErr(err) => {
                    crate::vibeio::op::io_util::read_error_result(err)?
                }
            }
        };
        let result = if result < 0 {
            #[cfg(windows)]
            {
                self.completion_staging = None;
            }
            crate::vibeio::op::io_util::read_error_result(io::Error::from_raw_os_error(-result))?
        } else {
            result
        };

        #[cfg(windows)]
        {
            let bufs = self.bufs.as_mut().unwrap();
            if let Some(staging) = self.completion_staging.take() {
                let mut src_offset = 0usize;
                let mut remaining = result as usize;
                let iovecs = bufs.as_iovecs_mut();
                for dst in iovecs {
                    if remaining == 0 {
                        break;
                    }
                    let chunk = remaining.min(dst.len);
                    if chunk == 0 {
                        continue;
                    }

                    let src = &staging[src_offset..src_offset + chunk];
                    // SAFETY: IoVectoredBufMut supplies writable capacity for
                    // each destination. The separately allocated staging buffer
                    // cannot overlap it. Do not form a u8 slice over spare capacity.
                    unsafe {
                        std::ptr::copy_nonoverlapping(src.as_ptr(), dst.ptr, chunk);
                    }
                    src_offset += chunk;
                    remaining -= chunk;
                }
            }
        }

        Poll::Ready(Ok(result as usize))
    }

    #[cfg(windows)]
    #[inline]
    fn submit_windows(&mut self, overlapped: *mut OVERLAPPED) -> Result<(), io::Error> {
        let bufs = self.bufs.as_mut().unwrap();
        match self.handle.handle {
            RawOsHandle::Socket(socket) => {
                let iovecs = bufs.as_iovecs_mut();
                let mut wsabufs = Vec::with_capacity(iovecs.len());
                for iovec in iovecs {
                    let len = u32::try_from(iovec.len).map_err(|_| {
                        io::Error::new(
                            io::ErrorKind::InvalidInput,
                            "readv buffer is too large for Windows socket I/O",
                        )
                    })?;
                    wsabufs.push(WSABUF {
                        len,
                        buf: iovec.ptr as *mut _,
                    });
                }

                let mut flags = 0u32;
                // SAFETY: Winsock captures the WSABUF array during submission;
                // flags is only an immediate output. Payloads and OVERLAPPED
                // remain owned through completion/cancellation acknowledgement.
                // https://learn.microsoft.com/en-us/windows/win32/api/winsock2/nf-winsock2-wsarecv
                let recv_result = unsafe {
                    WinSock::WSARecv(
                        socket as SOCKET,
                        wsabufs.as_mut_ptr(),
                        iovec_count(wsabufs.len())?,
                        std::ptr::null_mut(),
                        &mut flags,
                        overlapped,
                        None,
                    )
                };

                if recv_result == 0 {
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
                let iovecs = bufs.as_iovecs_mut();
                let total_len = (iovecs.iter()).try_fold(0usize, |acc, iovec| {
                    acc.checked_add(iovec.len).ok_or_else(|| {
                        io::Error::new(io::ErrorKind::InvalidInput, "readv buffer length overflow")
                    })
                })?;
                let total_len_u32 = u32::try_from(total_len).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "readv total length is too large for Windows file I/O",
                    )
                })?;

                let mut staging = vec![0u8; total_len];
                // SAFETY: staging owns total_len writable bytes and is retained
                // below on success or pending submission. The driver retains
                // OVERLAPPED; cancellation retains staging until acknowledgement.
                let read_result = unsafe {
                    ReadFile(
                        handle as HANDLE,
                        staging.as_mut_ptr().cast(),
                        total_len_u32,
                        std::ptr::null_mut(),
                        overlapped,
                    )
                };

                if read_result != 0 {
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

        let bufs = self.bufs.as_mut().unwrap();

        // Build a temporary iovec array for the syscall.
        let mut iovecs = if let Some(iovecs) = self.completion_system_iovecs.take() {
            iovecs
        } else {
            iovec_to_system(&bufs.as_iovecs_mut())
        };

        let entry = opcode::Readv::new(
            types::Fd(self.handle.handle),
            iovecs.as_mut_ptr(),
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

impl<B: IoVectoredBufMut> Drop for ReadvOp<'_, B> {
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

    #[test]
    fn pending_buffer_is_retained_by_owning_driver() {
        crate::vibeio::op::io_util::cancellation_tests::check_cancellation(
            |handle, buffer, reclaim| {
                let mut op = ReadvOp::new(handle, buffer);
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
