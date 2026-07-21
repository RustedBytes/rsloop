use std::io;
use std::task::{Context, Poll};

#[cfg(windows)]
use windows_sys::Win32::{
    Foundation::{ERROR_IO_PENDING, HANDLE},
    Storage::FileSystem::ReadFile,
    System::IO::OVERLAPPED,
};

use crate::driver::AnyDriver;
use crate::driver::CompletionIoResult;
use crate::fd_inner::InnerRawHandle;
#[cfg(windows)]
use crate::fd_inner::RawOsHandle;
use crate::io::IoBufMut;
use crate::op::io_util::CompletionBuffer;
use crate::op::Op;

pub struct ReadAtOp<'a, B: IoBufMut> {
    handle: &'a InnerRawHandle,
    buf: Option<CompletionBuffer<B>>,
    offset: u64,
    completion_token: Option<usize>,
}

impl<'a, B: IoBufMut> ReadAtOp<'a, B> {
    #[inline]
    pub fn new(handle: &'a InnerRawHandle, buf: B, offset: u64) -> Self {
        Self {
            handle,
            buf: Some(CompletionBuffer::new(buf, handle.uses_completion())),
            offset,
            completion_token: None,
        }
    }

    #[inline]
    pub fn take_bufs(mut self) -> B {
        self.buf.take().unwrap().into_inner()
    }
}

impl<B: IoBufMut> Op for ReadAtOp<'_, B> {
    type Output = usize;

    #[cfg(any(unix, windows))]
    #[inline]
    fn poll_completion(
        &mut self,
        cx: &mut Context<'_>,
        driver: &AnyDriver,
    ) -> Poll<io::Result<Self::Output>> {
        let result = if let Some(completion_token) = self.completion_token {
            match driver.get_completion_result(completion_token) {
                Some(result) => {
                    self.completion_token = None;
                    result
                }
                None => {
                    driver.set_completion_waker(completion_token, cx.waker().clone());
                    return Poll::Pending;
                }
            }
        } else {
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
        let read = result as usize;
        let buf = self.buf.as_mut().unwrap().as_mut();
        unsafe { buf.set_buf_init(read) };
        Poll::Ready(Ok(read))
    }

    #[cfg(windows)]
    #[inline]
    fn submit_windows(&mut self, overlapped: *mut OVERLAPPED) -> Result<(), io::Error> {
        let buf = self.buf.as_mut().unwrap().as_mut();
        let RawOsHandle::Handle(handle) = self.handle.handle else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "ReadAtOp expects a file handle, not a socket",
            ));
        };

        let read_len = u32::try_from(buf.buf_capacity()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "read buffer is too large for Windows file I/O",
            )
        })?;

        unsafe {
            (*overlapped).Anonymous.Anonymous.Offset = self.offset as u32;
            (*overlapped).Anonymous.Anonymous.OffsetHigh = (self.offset >> 32) as u32;
        }

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

    #[cfg(target_os = "linux")]
    #[inline]
    fn build_completion_entry(
        &mut self,
        user_data: u64,
    ) -> Result<io_uring::squeue::Entry, io::Error> {
        use io_uring::{opcode, types};

        let buf = self.buf.as_mut().unwrap().as_mut();
        let read_len = u32::try_from(buf.buf_capacity()).map_err(|_| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "read buffer is too large for io_uring",
            )
        })?;

        let entry = opcode::Read::new(
            types::Fd(self.handle.handle),
            buf.as_buf_mut_ptr(),
            read_len,
        )
        .offset(self.offset)
        .build()
        .user_data(user_data);

        Ok(entry)
    }
}

impl<B: IoBufMut> Drop for ReadAtOp<'_, B> {
    #[inline]
    fn drop(&mut self) {
        if let Some(completion_token) = self.completion_token {
            if let Some(driver) = crate::current_driver() {
                if let Some(buf) = self.buf.take() {
                    driver.ignore_completion(completion_token, Box::new(buf.into_stable_box()));
                } else {
                    driver.ignore_completion(completion_token, Box::new(()));
                }
            }
        }
    }
}
