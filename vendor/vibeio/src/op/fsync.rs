use std::io;
use std::task::{Context, Poll};

use crate::driver::AnyDriver;
use crate::driver::CompletionIoResult;
use crate::fd_inner::InnerRawHandle;
use crate::op::Op;

pub struct FsyncOp<'a> {
    handle: &'a InnerRawHandle,
    data_only: bool,
    completion_token: Option<usize>,
}

impl<'a> FsyncOp<'a> {
    #[inline]
    pub fn new(handle: &'a InnerRawHandle, data_only: bool) -> Self {
        Self {
            handle,
            data_only,
            completion_token: None,
        }
    }
}

impl Op for FsyncOp<'_> {
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

        let mut entry_builder = opcode::Fsync::new(types::Fd(self.handle.handle));
        if self.data_only {
            entry_builder = entry_builder.flags(types::FsyncFlags::DATASYNC);
        }
        let entry = entry_builder.build().user_data(user_data);

        Ok(entry)
    }
}

impl Drop for FsyncOp<'_> {
    #[inline]
    fn drop(&mut self) {
        if let Some(completion_token) = self.completion_token {
            if let Some(driver) = crate::current_driver() {
                driver.ignore_completion(completion_token, Box::new(()));
            }
        }
    }
}
