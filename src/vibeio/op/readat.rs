#![warn(clippy::undocumented_unsafe_blocks)]

use std::io;
use std::task::{Context, Poll};

#[cfg(windows)]
use windows_sys::Win32::{
    Foundation::{ERROR_IO_PENDING, HANDLE},
    Storage::FileSystem::ReadFile,
    System::IO::OVERLAPPED,
};

use crate::vibeio::driver::AnyDriver;
use crate::vibeio::driver::CompletionIoResult;
use crate::vibeio::fd_inner::InnerRawHandle;
#[cfg(windows)]
use crate::vibeio::fd_inner::RawOsHandle;
use crate::vibeio::io::IoBufMut;
use crate::vibeio::op::Op;
use crate::vibeio::op::io_util::CompletionBuffer;

pub struct ReadAtOp<'a, B: IoBufMut> {
    handle: &'a InnerRawHandle,
    buf: Option<CompletionBuffer<B>>,
    // Non-completion platforms use the higher-level filesystem fallback.
    // Keep other operation fields subject to dead-code checking.
    #[cfg_attr(not(any(target_os = "linux", windows)), allow(dead_code))]
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
        assert!(
            self.completion_token.is_none(),
            "cannot reclaim a buffer while I/O is pending"
        );
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
                CompletionIoResult::SubmitErr(err) => {
                    crate::vibeio::op::io_util::read_error_result(err)?
                }
            }
        };
        let result = if result < 0 {
            crate::vibeio::op::io_util::read_error_result(
                crate::vibeio::op::io_util::completion_error(result),
            )?
        } else {
            result
        };
        let read = result as usize;
        let buf = self.buf.as_mut().unwrap().as_mut();
        // SAFETY: successful file-read completion initializes exactly the
        // reported prefix of the submitted writable capacity. Pending storage
        // stays owned by CompletionBuffer; errors return before changing length.
        // Windows EOF is normalized to a zero-byte successful completion above.
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

        let read_len =
            crate::vibeio::op::io_util::completion_len(buf.buf_capacity()).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "read buffer is too large for Windows file I/O",
                )
            })?;

        // SAFETY: the IOCP driver supplies its live, exclusively initialized
        // OVERLAPPED record for this submission. Both offset words are written
        // before ReadFile can retain the record; the driver owns it to completion.
        unsafe {
            (*overlapped).Anonymous.Anonymous.Offset = self.offset as u32;
            (*overlapped).Anonymous.Anonymous.OffsetHigh = (self.offset >> 32) as u32;
        }

        // SAFETY: handle is the borrowed file handle, kept alive by the enclosing
        // file operation. IoBufMut supplies exclusive writable storage for
        // read_len bytes. CompletionBuffer keeps its allocation stable while
        // ReadFile is pending, and Drop transfers it to the owning driver on
        // cancellation. The driver retains OVERLAPPED until acknowledgement.
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
        let read_len =
            crate::vibeio::op::io_util::completion_len(buf.buf_capacity()).map_err(|_| {
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
        .offset(crate::vibeio::op::io_util::positional_offset(self.offset)?)
        .build()
        .user_data(user_data);

        Ok(entry)
    }
}

impl<B: IoBufMut> Drop for ReadAtOp<'_, B> {
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
    fn pending_buffer_is_retained_by_owning_driver() {
        crate::vibeio::op::io_util::cancellation_tests::check_cancellation(
            |handle, buffer, reclaim| {
                let mut op = ReadAtOp::new(handle, buffer, 0);
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
