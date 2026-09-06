use std::ffi::CString;
use std::io;
use std::rc::Rc;
use std::task::{Context, Poll};

use crate::vibeio::driver::AnyDriver;
use crate::vibeio::driver::CompletionIoResult;
use crate::vibeio::op::Op;

pub struct UnlinkOp {
    driver: Rc<AnyDriver>,
    path: Option<CString>,
    completion_token: Option<usize>,
    is_dir: bool,
}

impl UnlinkOp {
    #[inline]
    pub fn new(driver: Rc<AnyDriver>, path: CString, is_dir: bool) -> Self {
        Self {
            driver,
            path: Some(path),
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
        let entry = opcode::UnlinkAt::new(
            types::Fd(libc::AT_FDCWD),
            self.path.as_ref().expect("operation path missing").as_ptr(),
        )
        .flags(if self.is_dir { libc::AT_REMOVEDIR } else { 0 })
        .build()
        .user_data(user_data);

        Ok(entry)
    }
}

impl Drop for UnlinkOp {
    fn drop(&mut self) {
        if let Some(token) = self.completion_token.take() {
            // Paths and result storage remain owned until the kernel acknowledges
            // completion, even if cancellation runs outside the submitting runtime.
            self.driver
                .ignore_completion(token, Box::new((self.path.take(),)));
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
            let mut op = UnlinkOp::new(owner.clone(), CString::new("from").unwrap(), false);
            let wrong = AnyDriver::new_mock();
            let mut cx = Context::from_waker(std::task::Waker::noop());
            assert!(
                matches!(op.poll_completion(&mut cx, &wrong), Poll::Ready(Err(e)) if e.kind() == io::ErrorKind::InvalidInput)
            );
            assert!(op.completion_token.is_none());
            op.build_completion_entry(41).unwrap();
            let addresses = [op.path.as_ref().unwrap().as_ptr()];
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
            let payload = data.downcast::<(Option<CString>,)>().unwrap();
            assert_eq!(payload.0.as_ref().unwrap().as_ptr(), addresses[0]);
            assert_eq!(payload.0.as_ref().unwrap().to_bytes(), b"from");
        }
    }
}
