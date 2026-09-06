#![warn(clippy::undocumented_unsafe_blocks)]

use std::ffi::CString;
use std::io;
use std::mem::MaybeUninit;
use std::rc::Rc;
use std::task::{Context, Poll};

use crate::vibeio::driver::AnyDriver;
use crate::vibeio::driver::CompletionIoResult;
use crate::vibeio::op::Op;

pub struct StatxOp {
    driver: Rc<AnyDriver>,
    dirfd: libc::c_int,
    pathname: Option<CString>,
    flags: libc::c_int,
    mask: libc::c_uint,
    statxbuf: Option<Box<MaybeUninit<libc::statx>>>,
    completion_token: Option<usize>,
}

impl StatxOp {
    #[inline]
    pub fn new(
        driver: Rc<AnyDriver>,
        dirfd: libc::c_int,
        pathname: CString,
        flags: libc::c_int,
        mask: libc::c_uint,
    ) -> Self {
        Self {
            driver,
            dirfd,
            pathname: Some(pathname),
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
        if !std::ptr::eq(self.driver.as_ref(), driver) {
            return Poll::Ready(Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "operation belongs to a different driver",
            )));
        }
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
            Poll::Ready(Err(crate::vibeio::op::io_util::completion_error(result)))
        } else {
            let statxbuf = self.statxbuf.take().expect("statxbuf is None");
            // SAFETY: the successful statx completion initialized this submitted
            // allocation. The operation retained its stable box until the CQE;
            // errors return above without reading it and the token is cleared.
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
            self.pathname
                .as_ref()
                .expect("operation path missing")
                .as_ptr(),
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
    fn drop(&mut self) {
        if let Some(token) = self.completion_token.take() {
            // Paths and result storage remain owned until the kernel acknowledges
            // completion, even if cancellation runs outside the submitting runtime.
            self.driver.ignore_completion(
                token,
                Box::new((self.pathname.take(), self.statxbuf.take())),
            );
        }
    }
}

#[cfg(test)]
mod cancellation_tests {
    use super::*;

    #[test]
    fn paths_are_retained_by_the_submitting_driver() {
        for entered in [false, true] {
            let owner = Rc::new(AnyDriver::new_mock());
            let mut op = StatxOp::new(
                owner.clone(),
                libc::AT_FDCWD,
                CString::new("from").unwrap(),
                0,
                libc::STATX_ALL,
            );
            let wrong = AnyDriver::new_mock();
            let mut cx = Context::from_waker(std::task::Waker::noop());
            assert!(
                matches!(op.poll_completion(&mut cx, &wrong), Poll::Ready(Err(e)) if e.kind() == io::ErrorKind::InvalidInput)
            );
            assert!(op.completion_token.is_none());
            op.build_completion_entry(41).unwrap();
            let addresses = [op.pathname.as_ref().unwrap().as_ptr()];
            let result_address = op.statxbuf.as_ref().unwrap().as_ptr();
            op.completion_token = Some(41);
            if entered {
                let runtime = crate::vibeio::executor::Runtime::new(AnyDriver::new_mock());
                runtime.block_on(async move { drop(op) });
            } else {
                assert!(crate::vibeio::current_driver().is_none());
                drop(op);
            }
            assert_eq!(
                Rc::strong_count(&owner),
                1,
                "cancelled storage must not form a driver cycle"
            );
            let AnyDriver::Mock(driver) = owner.as_ref() else {
                unreachable!()
            };
            let mut held = driver.ignored.take();
            assert_eq!(held.len(), 1);
            let (token, data) = held.pop().unwrap();
            assert_eq!(token, 41);
            let payload = data
                .downcast::<(Option<CString>, Option<Box<MaybeUninit<libc::statx>>>)>()
                .unwrap();
            assert_eq!(payload.0.as_ref().unwrap().as_ptr(), addresses[0]);
            assert_eq!(payload.0.as_ref().unwrap().to_bytes(), b"from");
            assert_eq!(payload.1.as_ref().unwrap().as_ptr(), result_address);
        }
    }
}
