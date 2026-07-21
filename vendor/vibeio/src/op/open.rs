use std::ffi::CString;
use std::io;
use std::os::fd::RawFd;
use std::task::{Context, Poll};

use crate::driver::AnyDriver;
use crate::driver::CompletionIoResult;
use crate::op::Op;

pub type OpenRawHandle = RawFd;

pub struct OpenOp {
    path: CString,
    flags: i32,
    mode: libc::mode_t,
    completion_token: Option<usize>,
}

impl OpenOp {
    #[inline]
    pub fn new(path: CString, flags: i32, mode: libc::mode_t) -> Self {
        Self {
            path,
            flags,
            mode,
            completion_token: None,
        }
    }
}

impl Op for OpenOp {
    type Output = OpenRawHandle;

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
            Poll::Ready(Err(io::Error::from_raw_os_error(-result)))
        } else {
            Poll::Ready(Ok(result as RawFd))
        }
    }

    #[inline]
    fn build_completion_entry(
        &mut self,
        user_data: u64,
    ) -> Result<io_uring::squeue::Entry, io::Error> {
        use io_uring::{opcode, types};

        let entry = opcode::OpenAt::new(types::Fd(libc::AT_FDCWD), self.path.as_ptr())
            .flags(self.flags)
            .mode(self.mode)
            .build()
            .user_data(user_data);

        Ok(entry)
    }
}

impl Drop for OpenOp {
    #[inline]
    fn drop(&mut self) {
        if let Some(completion_token) = self.completion_token {
            if let Some(driver) = crate::current_driver() {
                driver.ignore_completion(completion_token, Box::new(()));
            }
        }
    }
}
