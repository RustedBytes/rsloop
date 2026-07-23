use std::io;
use std::os::fd::RawFd;
use std::task::{Context, Poll};

use mio::Interest;

use crate::current_driver;
use crate::driver::{AnyDriver, CompletionIoResult};
use crate::fd_inner::InnerRawHandle;
use crate::op::Op;

#[inline]
fn set_cloexec(fd: RawFd) -> Result<(), io::Error> {
    // set FD_CLOEXEC on file descriptor flags
    let fdflags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if fdflags == -1 {
        return Err(io::Error::last_os_error());
    }
    if fdflags & libc::FD_CLOEXEC == 0 {
        let result = unsafe { libc::fcntl(fd, libc::F_SETFD, fdflags | libc::FD_CLOEXEC) };
        if result == -1 {
            return Err(io::Error::last_os_error());
        }
    }

    Ok(())
}

pub struct AcceptUnixOp<'a> {
    handle: &'a InnerRawHandle,
    completion_token: Option<usize>,
}

impl<'a> AcceptUnixOp<'a> {
    #[inline]
    pub fn new(handle: &'a InnerRawHandle) -> Self {
        Self {
            handle,
            completion_token: None,
        }
    }
}

impl Op for AcceptUnixOp<'_> {
    type Output = RawFd;

    #[inline]
    fn poll_poll(
        &mut self,
        cx: &mut Context<'_>,
        driver: &AnyDriver,
    ) -> Poll<io::Result<Self::Output>> {
        #[cfg(syscall_accept4)]
        let accepted_fd = unsafe {
            libc::accept4(
                self.handle.handle,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            )
        };
        #[cfg(not(syscall_accept4))]
        let accepted_fd = unsafe {
            libc::accept(
                self.handle.handle,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
            )
        };
        if accepted_fd == -1 {
            let error = io::Error::last_os_error();
            if error.kind() == io::ErrorKind::WouldBlock {
                if let Err(err) =
                    driver.submit_poll(self.handle, cx.waker().clone(), Interest::READABLE)
                {
                    return Poll::Ready(Err(err));
                }
                return Poll::Pending;
            }
            return Poll::Ready(Err(error));
        }

        let fd = accepted_fd as RawFd;
        // On non-Linux Unix, set close-on-exec manually.
        // Linux accept4() above already set it atomically.
        #[cfg(not(syscall_accept4))]
        if let Err(err) = set_cloexec(fd) {
            return Poll::Ready(Err(err));
        }

        Poll::Ready(Ok(fd))
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
            return Poll::Ready(Err(io::Error::from_raw_os_error(-result)));
        }

        let fd = result as RawFd;
        if let Err(err) = set_cloexec(fd) {
            return Poll::Ready(Err(err));
        }

        Poll::Ready(Ok(fd))
    }

    #[cfg(target_os = "linux")]
    #[inline]
    fn build_completion_entry(
        &mut self,
        user_data: u64,
    ) -> Result<io_uring::squeue::Entry, io::Error> {
        use io_uring::{opcode, types};

        let entry = opcode::Accept::new(
            types::Fd(self.handle.handle),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        )
        .build()
        .user_data(user_data);
        Ok(entry)
    }
}

impl Drop for AcceptUnixOp<'_> {
    #[inline]
    fn drop(&mut self) {
        if let Some(completion_token) = self.completion_token {
            if let Some(driver) = current_driver() {
                driver.ignore_completion(completion_token, Box::new(()));
            }
        }
    }
}
