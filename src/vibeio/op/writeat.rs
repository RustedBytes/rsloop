#![warn(clippy::undocumented_unsafe_blocks)]

use std::io;
use std::task::{Context, Poll};

#[cfg(windows)]
use windows_sys::Win32::{
    Foundation::{ERROR_IO_PENDING, HANDLE},
    Storage::FileSystem::WriteFile,
    System::IO::OVERLAPPED,
};

use crate::vibeio::driver::AnyDriver;
use crate::vibeio::driver::CompletionIoResult;
use crate::vibeio::fd_inner::InnerRawHandle;
#[cfg(windows)]
use crate::vibeio::fd_inner::RawOsHandle;
use crate::vibeio::io::IoBuf;
use crate::vibeio::op::Op;
use crate::vibeio::op::io_util::CompletionBuffer;

#[cfg(any(windows, test))]
pub(crate) fn validate_windows_write_offset(offset: u64) -> io::Result<()> {
    // WriteFile interprets both offset words set to all-one bits as append.
    // A positional operation must not silently select that separate behavior.
    if offset == u64::MAX {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "positional offset is the Windows append sentinel",
        ));
    }
    Ok(())
}

#[cfg_attr(not(any(target_os = "linux", windows)), allow(dead_code))]
pub struct WriteAtOp<'a, B: IoBuf> {
    handle: &'a InnerRawHandle,
    buf: Option<CompletionBuffer<B>>,
    offset: u64,
    completion_token: Option<usize>,
}

impl<'a, B: IoBuf> WriteAtOp<'a, B> {
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
        assert!(
            self.completion_token.is_none(),
            "cannot reclaim a buffer while I/O is pending"
        );
        self.buf.take().unwrap().into_inner()
    }
}

impl<B: IoBuf> Op for WriteAtOp<'_, B> {
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
            return Poll::Ready(Err(crate::vibeio::op::io_util::completion_error(result)));
        }
        let written = result as usize;
        Poll::Ready(Ok(written))
    }

    #[cfg(windows)]
    #[inline]
    fn submit_windows(&mut self, overlapped: *mut OVERLAPPED) -> Result<(), io::Error> {
        validate_windows_write_offset(self.offset)?;
        let RawOsHandle::Handle(handle) = self.handle.handle else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "WriteAtOp expects a file handle, not a socket",
            ));
        };

        let buf = self.buf.as_ref().unwrap().as_ref();
        let write_len =
            crate::vibeio::op::io_util::completion_len(buf.buf_len()).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "write buffer is too large for Windows file I/O",
                )
            })?;

        // SAFETY: the driver provides exclusive writable OVERLAPPED storage
        // before submission. Splitting the offset preserves both 32-bit words.
        unsafe {
            (*overlapped).Anonymous.Anonymous.Offset = self.offset as u32;
            (*overlapped).Anonymous.Anonymous.OffsetHigh = (self.offset >> 32) as u32;
        }

        // SAFETY: the initialized buffer is stable and retained through I/O;
        // the driver retains OVERLAPPED, including cancellation acknowledgement.
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

    #[cfg(target_os = "linux")]
    #[inline]
    fn build_completion_entry(
        &mut self,
        user_data: u64,
    ) -> Result<io_uring::squeue::Entry, io::Error> {
        use io_uring::{opcode, types};

        let buf = self.buf.as_ref().unwrap().as_ref();
        let write_len =
            crate::vibeio::op::io_util::completion_len(buf.buf_len()).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "write buffer is too large for io_uring",
                )
            })?;

        let entry = opcode::Write::new(types::Fd(self.handle.handle), buf.as_buf_ptr(), write_len)
            .offset(crate::vibeio::op::io_util::positional_offset(self.offset)?)
            .build()
            .user_data(user_data);

        Ok(entry)
    }
}

impl<B: IoBuf> Drop for WriteAtOp<'_, B> {
    #[inline]
    fn drop(&mut self) {
        if let Some(token) = self.completion_token.take() {
            let completion_state = ();
            // The owning driver, not the currently entered runtime, must retain
            // every kernel-visible allocation until completion is acknowledged.
            self.handle.cancel_completion(
                token,
                Box::new((
                    completion_state,
                    self.buf.take().map(CompletionBuffer::into_stable_box),
                )),
            );
        }
    }
}

#[cfg(test)]
mod cancellation_tests {
    use super::*;

    #[test]
    fn windows_append_sentinel_is_not_a_positional_offset() {
        for offset in [
            0,
            1,
            u32::MAX as u64,
            1 << 32,
            i64::MAX as u64,
            u64::MAX - 1,
        ] {
            validate_windows_write_offset(offset).unwrap();
        }
        assert_eq!(
            validate_windows_write_offset(u64::MAX).unwrap_err().kind(),
            io::ErrorKind::InvalidInput
        );
    }

    #[test]
    fn pending_buffer_is_retained_by_owning_driver() {
        crate::vibeio::op::io_util::cancellation_tests::check_cancellation(
            |handle, buffer, reclaim| {
                let mut op = WriteAtOp::new(handle, buffer, 0);
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
