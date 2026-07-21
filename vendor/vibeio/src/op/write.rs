use std::io;
use std::task::{Context, Poll};

use mio::Interest;
#[cfg(windows)]
use windows_sys::Win32::{
    Foundation::{ERROR_IO_PENDING, HANDLE},
    Networking::WinSock::{self, SOCKET, WSABUF, WSA_IO_PENDING},
    Storage::FileSystem::WriteFile,
    System::IO::OVERLAPPED,
};

use crate::driver::AnyDriver;
use crate::driver::CompletionIoResult;
use crate::fd_inner::InnerRawHandle;
#[cfg(windows)]
use crate::fd_inner::RawOsHandle;
use crate::op::io_util::{poll_result_or_wait, CompletionBuffer};
use crate::op::Op;

#[cfg(windows)]
#[inline]
fn socket_write(socket: SOCKET, buf: &[u8]) -> io::Result<usize> {
    use windows_sys::Win32::Networking::WinSock::{self as WinSock, SOCKET_ERROR, WSABUF};

    let len = u32::try_from(buf.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "write buffer is too large for Windows socket I/O",
        )
    })?;

    let mut wsabuf = WSABUF {
        len,
        buf: buf.as_ptr().cast_mut().cast(),
    };
    let mut bytes: u32 = 0;

    let send_result = unsafe {
        WinSock::WSASend(
            socket,
            &mut wsabuf,
            1,
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

use crate::io::IoBuf;

pub struct WriteOp<'a, B: IoBuf> {
    handle: &'a InnerRawHandle,
    buf: Option<CompletionBuffer<B>>,
    completion_token: Option<usize>,
    #[cfg(windows)]
    socket_buf: Option<Box<WSABUF>>,
}

impl<'a, B: IoBuf> WriteOp<'a, B> {
    #[inline]
    pub fn new(handle: &'a InnerRawHandle, buf: B) -> Self {
        Self {
            handle,
            buf: Some(CompletionBuffer::new(buf, handle.uses_completion())),
            completion_token: None,
            #[cfg(windows)]
            socket_buf: None,
        }
    }

    #[inline]
    pub fn take_bufs(mut self) -> B {
        self.buf.take().unwrap().into_inner()
    }
}

impl<B: IoBuf> Op for WriteOp<'_, B> {
    type Output = usize;

    #[cfg(any(unix, windows))]
    #[inline]
    fn poll_poll(
        &mut self,
        cx: &mut Context<'_>,
        driver: &AnyDriver,
    ) -> Poll<io::Result<Self::Output>> {
        let buf = self.buf.as_ref().unwrap().as_ref();

        #[cfg(unix)]
        let result = {
            let written = unsafe {
                libc::write(
                    self.handle.handle,
                    buf.as_buf_ptr().cast::<libc::c_void>(),
                    buf.buf_len(),
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
            RawOsHandle::Socket(socket) => {
                let slice = unsafe { std::slice::from_raw_parts(buf.as_buf_ptr(), buf.buf_len()) };
                socket_write(socket as SOCKET, slice)
            }
            RawOsHandle::Handle(_) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "poll-based write currently supports sockets only on Windows",
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
            return Poll::Ready(Err(io::Error::from_raw_os_error(-result)));
        }
        Poll::Ready(Ok(result as usize))
    }

    #[cfg(windows)]
    #[inline]
    fn submit_windows(&mut self, overlapped: *mut OVERLAPPED) -> Result<(), io::Error> {
        let buf = self.buf.as_ref().unwrap().as_ref();
        match self.handle.handle {
            RawOsHandle::Socket(socket) => {
                let write_len = u32::try_from(buf.buf_len()).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "write buffer is too large for Windows socket I/O",
                    )
                })?;

                let wsabuf = self.socket_buf.get_or_insert_with(|| {
                    Box::new(WSABUF {
                        len: 0,
                        buf: std::ptr::null_mut(),
                    })
                });
                wsabuf.len = write_len;
                wsabuf.buf = buf.as_buf_ptr() as *mut _;

                let send_result = unsafe {
                    WinSock::WSASend(
                        socket as SOCKET,
                        wsabuf.as_mut() as *mut WSABUF,
                        1,
                        std::ptr::null_mut(),
                        0,
                        overlapped,
                        None,
                    )
                };

                if send_result == 0 {
                    return Ok(());
                }

                let err = unsafe { WinSock::WSAGetLastError() };
                if err == WSA_IO_PENDING {
                    Ok(())
                } else {
                    Err(io::Error::from_raw_os_error(err))
                }
            }
            RawOsHandle::Handle(handle) => {
                let write_len = u32::try_from(buf.buf_len()).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "write buffer is too large for Windows file I/O",
                    )
                })?;

                let write_result = unsafe {
                    WriteFile(
                        handle as HANDLE,
                        buf.as_buf_ptr().cast(),
                        write_len,
                        std::ptr::null_mut(),
                        overlapped,
                    )
                };

                if write_result != 0 {
                    return Ok(());
                }

                let err = io::Error::last_os_error();
                if err.raw_os_error() == Some(ERROR_IO_PENDING as i32) {
                    Ok(())
                } else {
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

        let buf = self.buf.as_ref().unwrap().as_ref();
        let entry = opcode::Write::new(
            types::Fd(self.handle.handle),
            buf.as_buf_ptr(),
            buf.buf_len() as _,
        )
        .build()
        .user_data(user_data);

        Ok(entry)
    }
}

impl<B: IoBuf> Drop for WriteOp<'_, B> {
    #[inline]
    fn drop(&mut self) {
        if let Some(completion_token) = self.completion_token {
            if let Some(driver) = crate::current_driver() {
                #[cfg(windows)]
                let completion_state = self.socket_buf.take();
                #[cfg(not(windows))]
                let completion_state = ();

                driver.ignore_completion(
                    completion_token,
                    Box::new((
                        completion_state,
                        self.buf.take().map(CompletionBuffer::into_stable_box),
                    )),
                );
            }
        }
    }
}
