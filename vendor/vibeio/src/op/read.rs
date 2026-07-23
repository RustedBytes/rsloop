use std::io;
use std::task::{Context, Poll};

use mio::Interest;
#[cfg(windows)]
use windows_sys::Win32::{
    Foundation::{ERROR_HANDLE_EOF, ERROR_IO_PENDING, HANDLE},
    Networking::WinSock::{self, SOCKET, WSABUF, WSA_IO_PENDING},
    Storage::FileSystem::ReadFile,
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
fn socket_read(socket: SOCKET, buf: &mut [u8]) -> io::Result<usize> {
    use windows_sys::Win32::Networking::WinSock::{self as WinSock, SOCKET_ERROR, WSABUF};

    let len = u32::try_from(buf.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "read buffer is too large for Windows socket I/O",
        )
    })?;

    let mut wsabuf = WSABUF {
        len,
        buf: buf.as_mut_ptr().cast(),
    };
    let mut bytes: u32 = 0;
    let mut flags: u32 = 0;

    let recv_result = unsafe {
        WinSock::WSARecv(
            socket,
            &mut wsabuf,
            1,
            &mut bytes,
            &mut flags,
            std::ptr::null_mut(),
            None,
        )
    };
    if recv_result == SOCKET_ERROR {
        return Err(io::Error::from_raw_os_error(unsafe {
            WinSock::WSAGetLastError()
        }));
    }

    Ok(bytes as usize)
}

use crate::io::IoBufMut;

pub struct ReadOp<'a, B: IoBufMut> {
    handle: &'a InnerRawHandle,
    buf: Option<CompletionBuffer<B>>,
    completion_token: Option<usize>,
    #[cfg(windows)]
    socket_buf: Option<Box<WSABUF>>,
}

impl<'a, B: IoBufMut> ReadOp<'a, B> {
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

impl<B: IoBufMut> Op for ReadOp<'_, B> {
    type Output = usize;

    #[cfg(any(unix, windows))]
    #[inline]
    fn poll_poll(
        &mut self,
        cx: &mut Context<'_>,
        driver: &AnyDriver,
    ) -> Poll<io::Result<Self::Output>> {
        let buf = self.buf.as_mut().unwrap().as_mut();

        #[cfg(unix)]
        let result = {
            let read = unsafe {
                libc::read(
                    self.handle.handle,
                    buf.as_buf_mut_ptr().cast::<libc::c_void>(),
                    buf.buf_capacity(),
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
            RawOsHandle::Socket(socket) => {
                let slice = unsafe {
                    std::slice::from_raw_parts_mut(buf.as_buf_mut_ptr(), buf.buf_capacity())
                };
                socket_read(socket as SOCKET, slice)
            }
            RawOsHandle::Handle(_) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "poll-based read currently supports sockets only on Windows",
            )),
        };

        match poll_result_or_wait(result, self.handle, cx, driver, Interest::READABLE) {
            Poll::Ready(Ok(read)) => {
                unsafe { buf.set_buf_init(read) };
                Poll::Ready(Ok(read))
            }
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
            if -result == ERROR_HANDLE_EOF as i32 {
                let buf = self.buf.as_mut().unwrap().as_mut();
                unsafe { buf.set_buf_init(0) };
                return Poll::Ready(Ok(0));
            }
            return Poll::Ready(Err(io::Error::from_raw_os_error(-result)));
        }
        let read = result as usize;
        let buf = self.buf.as_mut().unwrap().as_mut();
        unsafe { buf.set_buf_init(read) };
        Poll::Ready(Ok(read))
    }

    #[cfg(windows)]
    #[inline]
    fn submit_windows(&mut self, overlapped: *mut OVERLAPPED) -> Result<(), io::Error> {
        let buf = self.buf.as_mut().unwrap().as_mut();
        match self.handle.handle {
            RawOsHandle::Socket(socket) => {
                let read_len = u32::try_from(buf.buf_capacity()).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "read buffer is too large for Windows socket I/O",
                    )
                })?;

                let wsabuf = self.socket_buf.get_or_insert_with(|| {
                    Box::new(WSABUF {
                        len: 0,
                        buf: std::ptr::null_mut(),
                    })
                });
                wsabuf.len = read_len;
                wsabuf.buf = buf.as_buf_mut_ptr().cast();

                let mut flags: u32 = 0;
                let recv_result = unsafe {
                    WinSock::WSARecv(
                        socket as SOCKET,
                        wsabuf.as_mut() as *mut WSABUF,
                        1,
                        std::ptr::null_mut(),
                        &mut flags,
                        overlapped,
                        None,
                    )
                };

                if recv_result == 0 {
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
                let read_len = u32::try_from(buf.buf_capacity()).map_err(|_| {
                    io::Error::new(
                        io::ErrorKind::InvalidInput,
                        "read buffer is too large for Windows file I/O",
                    )
                })?;

                let read_result = unsafe {
                    ReadFile(
                        handle as HANDLE,
                        buf.as_buf_mut_ptr().cast(),
                        read_len,
                        std::ptr::null_mut(),
                        overlapped,
                    )
                };

                if read_result != 0 {
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

        let buf = self.buf.as_mut().unwrap().as_mut();
        let entry = opcode::Read::new(
            types::Fd(self.handle.handle),
            buf.as_buf_mut_ptr(),
            (buf.buf_capacity()) as _,
        )
        .build()
        .user_data(user_data);

        Ok(entry)
    }
}

impl<B: IoBufMut> Drop for ReadOp<'_, B> {
    #[inline]
    fn drop(&mut self) {
        if let Some(completion_token) = self.completion_token {
            if let Some(driver) = crate::current_driver() {
                #[cfg(windows)]
                let completion_state = self.socket_buf.take();
                #[cfg(not(windows))]
                let completion_state = ();

                let ignored_data = Box::new((
                    completion_state,
                    self.buf.take().map(CompletionBuffer::into_stable_box),
                ));
                #[cfg(windows)]
                driver.cancel_completion(completion_token, self.handle.handle, ignored_data);
                #[cfg(not(windows))]
                driver.ignore_completion(completion_token, ignored_data);
            }
        }
    }
}
