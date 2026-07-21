use std::ffi::CString;
use std::io;
use std::mem::MaybeUninit;
use std::task::{Context, Poll};

use crate::current_driver;
use crate::driver::AnyDriver;
use crate::driver::CompletionIoResult;
use crate::op::Op;

pub struct StatxOp {
    dirfd: libc::c_int,
    pathname: CString,
    flags: libc::c_int,
    mask: libc::c_uint,
    statxbuf: Option<Box<MaybeUninit<libc::statx>>>,
    completion_token: Option<usize>,
}

impl StatxOp {
    #[inline]
    pub fn new(
        dirfd: libc::c_int,
        pathname: CString,
        flags: libc::c_int,
        mask: libc::c_uint,
    ) -> Self {
        Self {
            dirfd,
            pathname,
            flags,
            mask,
            statxbuf: None,
            completion_token: None,
        }
    }
}

impl Op for StatxOp {
    type Output = libc::statx;

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
            // SAFETY: kernel fills the statx struct on success.
            let statxbuf = self.statxbuf.take().expect("statxbuf is None");
            let st = unsafe { *statxbuf.assume_init() };
            Poll::Ready(Ok(st))
        }
    }

    #[inline]
    fn build_completion_entry(
        &mut self,
        user_data: u64,
    ) -> Result<io_uring::squeue::Entry, io::Error> {
        use io_uring::{opcode, types};

        let mut statxbuf = if let Some(statxbuf) = self.statxbuf.take() {
            statxbuf
        } else {
            Box::new_uninit()
        };

        let entry = opcode::Statx::new(
            types::Fd(self.dirfd),
            self.pathname.as_ptr(),
            statxbuf.as_mut_ptr() as *mut types::statx,
        )
        .flags(self.flags as _)
        .mask(self.mask)
        .build()
        .user_data(user_data);

        self.statxbuf = Some(statxbuf);

        Ok(entry)
    }
}

impl Drop for StatxOp {
    #[inline]
    fn drop(&mut self) {
        if let Some(completion_token) = self.completion_token {
            if let Some(driver) = current_driver() {
                let statxbuf = self.statxbuf.take();
                driver.ignore_completion(completion_token, Box::new(statxbuf));
            }
        }
    }
}
