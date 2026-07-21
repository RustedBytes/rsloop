use std::ffi::CString;
use std::io;
use std::task::{Context, Poll};

use crate::driver::AnyDriver;
use crate::driver::CompletionIoResult;
use crate::op::Op;

pub struct UnlinkOp {
    path: CString,
    completion_token: Option<usize>,
    is_dir: bool,
}

impl UnlinkOp {
    #[inline]
    pub fn new(path: CString, is_dir: bool) -> Self {
        Self {
            path,
            completion_token: None,
            is_dir,
        }
    }
}

impl Op for UnlinkOp {
    type Output = ();

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
            Poll::Ready(Ok(()))
        }
    }

    #[inline]
    fn build_completion_entry(
        &mut self,
        user_data: u64,
    ) -> Result<io_uring::squeue::Entry, io::Error> {
        use io_uring::{opcode, types};

        // AT_Unlink flag is passed as the flags parameter to unlinkat
        let entry = opcode::UnlinkAt::new(types::Fd(libc::AT_FDCWD), self.path.as_ptr())
            .flags(if self.is_dir { libc::AT_REMOVEDIR } else { 0 })
            .build()
            .user_data(user_data);

        Ok(entry)
    }
}

impl Drop for UnlinkOp {
    #[inline]
    fn drop(&mut self) {
        if let Some(completion_token) = self.completion_token {
            if let Some(driver) = crate::current_driver() {
                driver.ignore_completion(completion_token, Box::new(()));
            }
        }
    }
}
