use std::io::{self};
use std::task::{Context, Poll};

use mio::Interest;
#[cfg(windows)]
use windows_sys::Win32::{
    Foundation::{ERROR_IO_PENDING, HANDLE},
    Networking::WinSock::{self as WinSock, SOCKET, WSABUF, WSA_IO_PENDING},
    Storage::FileSystem::WriteFile,
    System::IO::OVERLAPPED,
};

use crate::current_driver;
use crate::driver::AnyDriver;
use crate::driver::CompletionIoResult;
use crate::fd_inner::InnerRawHandle;
#[cfg(windows)]
use crate::fd_inner::RawOsHandle;
#[cfg(unix)]
use crate::io::IoVec;
use crate::io::IoVectoredBuf;
use crate::op::io_util::poll_result_or_wait;
use crate::op::Op;

/// Converts a slice of `IoSlice` to a system iovec buffer.
#[cfg(unix)]
#[inline]
fn iovec_to_system(bufs: &[IoVec]) -> Box<[libc::iovec]> {
    use std::mem::MaybeUninit;

    let mut iovecs_maybeuninit: Box<[MaybeUninit<libc::iovec>]> = Box::new_uninit_slice(bufs.len());
    for (index, s) in bufs.iter().enumerate() {
        let iov = libc::iovec {
            iov_base: s.ptr as *mut libc::c_void,
            iov_len: s.len,
        };
        iovecs_maybeuninit[index].write(iov);
    }
    // SAFETY: The boxed slice would have all values initialized after interating over original array
    unsafe { iovecs_maybeuninit.assume_init() }
}

#[cfg(windows)]
#[inline]
fn socket_write_vectored<B: IoVectoredBuf>(socket: SOCKET, bufs: &B) -> io::Result<usize> {
    use windows_sys::Win32::Networking::WinSock::{self as WinSock, SOCKET_ERROR, WSABUF};

    let iovecs = bufs.as_iovecs();
    let mut wsabufs = Vec::with_capacity(iovecs.len());
    for iovec in iovecs {
        let len = u32::try_from(iovec.len).map_err(|_| {
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
    let send_result = unsafe {
        WinSock::WSASend(
            socket,
            wsabufs.as_mut_ptr(),
            wsabufs.len() as u32,
            &mut bytes,
            0,
            std::ptr::null_mut(),
            None,
        )
    };
    if send_result == SOCKET_ERROR {
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
    completion_wsabufs: Option<Box<[WSABUF]>>,
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
            completion_wsabufs: None,
            #[cfg(windows)]
            completion_staging: None,
            #[cfg(target_os = "linux")]
            completion_system_iovecs: None,
        }
    }

    #[inline]
    pub fn take_bufs(mut self) -> B {
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
            let written = unsafe {
                libc::writev(
                    self.handle.handle,
                    iovecs_system.as_ptr(),
                    iovecs_system.len() as libc::c_int,
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

        match poll_result_or_wait(result, self.handle, cx, driver, Interest::WRITABLE) {
            Poll::Ready(Ok(written)) => Poll::Ready(Ok(written)),
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
        }
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
                self.completion_wsabufs = None;
                self.completion_staging = None;
            }
            return Poll::Ready(Err(io::Error::from_raw_os_error(-result)));
        }

        #[cfg(windows)]
        {
            self.completion_wsabufs = None;
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
                let mut wsabufs = Vec::with_capacity(iovecs.len());
                for iovec in iovecs {
                    let len = u32::try_from(iovec.len).map_err(|_| {
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

                let mut wsabufs = wsabufs.into_boxed_slice();
                let send_result = unsafe {
                    WinSock::WSASend(
                        socket as SOCKET,
                        wsabufs.as_mut_ptr(),
                        wsabufs.len() as u32,
                        std::ptr::null_mut(),
                        0,
                        overlapped,
                        None,
                    )
                };

                if send_result == 0 {
                    self.completion_wsabufs = Some(wsabufs);
                    self.completion_staging = None;
                    return Ok(());
                }

                let err = unsafe { WinSock::WSAGetLastError() };
                if err == WSA_IO_PENDING {
                    self.completion_wsabufs = Some(wsabufs);
                    self.completion_staging = None;
                    Ok(())
                } else {
                    self.completion_wsabufs = None;
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
                let total_len_u32 = u32::try_from(total_len).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "writev total length is too large for Windows file I/O",
                    )
                })?;

                let mut staging = Vec::with_capacity(total_len);
                for iovec in iovecs {
                    let slice = unsafe { std::slice::from_raw_parts(iovec.ptr, iovec.len) };
                    staging.extend_from_slice(slice);
                }

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
                    self.completion_wsabufs = None;
                    self.completion_staging = Some(staging);
                    return Ok(());
                }

                let err = io::Error::last_os_error();
                if err.raw_os_error() == Some(ERROR_IO_PENDING as i32) {
                    self.completion_wsabufs = None;
                    self.completion_staging = Some(staging);
                    Ok(())
                } else {
                    self.completion_wsabufs = None;
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
            iovecs.len() as _,
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
        if let Some(completion_token) = self.completion_token {
            if let Some(driver) = current_driver() {
                #[cfg(target_os = "linux")]
                let bufs = self.completion_system_iovecs.take();
                #[cfg(windows)]
                let bufs = self.completion_wsabufs.take();
                #[cfg(not(any(target_os = "linux", windows)))]
                let bufs = ();

                driver.ignore_completion(completion_token, Box::new((bufs, self.bufs.take())));
            }
        }
    }
}
