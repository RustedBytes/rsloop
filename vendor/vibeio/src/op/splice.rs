use std::io;
use std::os::fd::{AsRawFd, BorrowedFd};
use std::task::{Context, Poll};

use mio::Interest;

use crate::driver::AnyDriver;
use crate::driver::CompletionIoResult;
use crate::fd_inner::InnerRawHandle;
use crate::op::io_util::poll_result_or_wait;
use crate::op::Op;

pub struct SpliceOp<'a> {
    fd_in: BorrowedFd<'a>,
    fd_out: &'a InnerRawHandle,
    len: usize,
    completion_token: Option<usize>,
}

impl<'a> SpliceOp<'a> {
    #[inline]
    pub fn new(fd_in: BorrowedFd<'a>, fd_out: &'a InnerRawHandle, len: usize) -> Self {
        Self {
            fd_in,
            fd_out,
            len,
            completion_token: None,
        }
    }
}

impl Op for SpliceOp<'_> {
    type Output = usize;

    #[inline]
    fn poll_poll(
        &mut self,
        cx: &mut Context<'_>,
        driver: &AnyDriver,
    ) -> Poll<io::Result<Self::Output>> {
        let result = {
            let returned = unsafe {
                libc::splice(
                    self.fd_in.as_raw_fd(),
                    std::ptr::null_mut(),
                    self.fd_out.handle,
                    std::ptr::null_mut(),
                    self.len,
                    libc::SPLICE_F_NONBLOCK,
                )
            };
            if returned == -1 {
                Err(io::Error::last_os_error())
            } else {
                Ok(returned as usize)
            }
        };

        poll_result_or_wait(result, self.fd_out, cx, driver, Interest::WRITABLE)
    }

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
            Poll::Ready(Ok(result as usize))
        }
    }

    #[inline]
    fn build_completion_entry(
        &mut self,
        user_data: u64,
    ) -> Result<io_uring::squeue::Entry, io::Error> {
        use io_uring::{opcode, types};

        let entry = opcode::Splice::new(
            types::Fd(self.fd_in.as_raw_fd()),
            -1,
            types::Fd(self.fd_out.handle),
            -1,
            self.len as u32,
        )
        .build()
        .user_data(user_data);

        Ok(entry)
    }
}

impl Drop for SpliceOp<'_> {
    #[inline]
    fn drop(&mut self) {
        if let Some(completion_token) = self.completion_token {
            if let Some(driver) = crate::current_driver() {
                driver.ignore_completion(completion_token, Box::new(()));
            }
        }
    }
}
