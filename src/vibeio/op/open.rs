#![warn(clippy::undocumented_unsafe_blocks)]

use std::ffi::CString;
use std::io;
use std::os::fd::{FromRawFd, OwnedFd};
use std::rc::Rc;
use std::task::{Context, Poll};

use crate::vibeio::driver::AnyDriver;
use crate::vibeio::driver::CompletionIoResult;
use crate::vibeio::op::Op;

pub struct OpenOp {
    driver: Rc<AnyDriver>,
    path: Option<CString>,
    flags: i32,
    mode: libc::mode_t,
    completion_token: Option<usize>,
}

impl OpenOp {
    #[inline]
    pub fn new(driver: Rc<AnyDriver>, path: CString, flags: i32, mode: libc::mode_t) -> Self {
        Self {
            driver,
            path: Some(path),
            flags,
            mode,
            completion_token: None,
        }
    }
}

impl Op for OpenOp {
    type Output = OwnedFd;

    fn completion_returns_fd(&self) -> bool {
        true
    }

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
            // SAFETY: A successful OpenAt completion returns a fresh descriptor.
            // Taking the completion removes it from the driver's pending state;
            // clearing our token above prevents cancellation from closing it.
            // Ownership now transfers to the result, including if it is discarded.
            Poll::Ready(Ok(unsafe { OwnedFd::from_raw_fd(result) }))
        }
    }

    #[inline]
    fn build_completion_entry(
        &mut self,
        user_data: u64,
    ) -> Result<io_uring::squeue::Entry, io::Error> {
        use io_uring::{opcode, types};

        let entry = opcode::OpenAt::new(
            types::Fd(libc::AT_FDCWD),
            self.path.as_ref().expect("operation path missing").as_ptr(),
        )
        .flags(self.flags)
        .mode(self.mode)
        .build()
        .user_data(user_data);

        Ok(entry)
    }
}

impl Drop for OpenOp {
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
    fn dropping_successful_open_result_closes_the_descriptor() {
        use std::io::Read;
        use std::os::fd::AsRawFd;
        use std::time::{Duration, Instant};

        let driver = match AnyDriver::new_uring_custom(io_uring::IoUring::builder()) {
            Ok(driver) => Rc::new(driver),
            Err(error)
                if matches!(
                    error.raw_os_error(),
                    Some(libc::EPERM | libc::ENOSYS | libc::EOPNOTSUPP)
                ) =>
            {
                eprintln!("io_uring unavailable: {error}");
                return;
            }
            Err(error) => panic!("io_uring initialization failed: {error}"),
        };
        let (mut reader, writer) = std::io::pipe().unwrap();
        crate::vibeio::fd_inner::set_nonblocking(reader.as_raw_fd(), true).unwrap();
        let mut op = OpenOp::new(
            driver.clone(),
            CString::new(format!("/proc/self/fd/{}", writer.as_raw_fd())).unwrap(),
            libc::O_WRONLY | libc::O_CLOEXEC,
            0,
        );
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut cx = Context::from_waker(std::task::Waker::noop());
        let opened = loop {
            if let Poll::Ready(result) = op.poll_completion(&mut cx, &driver) {
                break result.unwrap();
            }
            assert!(Instant::now() < deadline, "open completion timed out");
            driver.wait(Some(Duration::from_millis(10)));
        };
        drop(writer);
        drop(op);
        let mut byte = [0];
        assert_eq!(
            reader.read(&mut byte).unwrap_err().kind(),
            io::ErrorKind::WouldBlock,
            "the returned descriptor must keep the pipe writer alive"
        );
        drop(opened);
        assert_eq!(reader.read(&mut byte).unwrap(), 0, "last writer was leaked");
    }

    #[test]
    fn paths_are_retained_by_the_submitting_driver() {
        for entered in [false, true] {
            let owner = Rc::new(AnyDriver::new_mock());
            let mut op = OpenOp::new(
                owner.clone(),
                CString::new("from").unwrap(),
                libc::O_RDONLY,
                0,
            );
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
