use std::io;
use std::os::fd::{FromRawFd, OwnedFd};
use std::task::{Context, Poll};

use mio::Interest;

use crate::vibeio::driver::AnyDriver;
use crate::vibeio::fd_inner::InnerRawHandle;
use crate::vibeio::op::Op;

#[cfg(test)]
mod tests {
    use super::*;
    use std::future::poll_fn;
    use std::process::{Command, Stdio};

    struct ChildGuard(std::process::Child);
    impl Drop for ChildGuard {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn running_child_stays_pending_until_exit() {
        let runtime = crate::vibeio::executor::Runtime::new(AnyDriver::new_mio().unwrap());
        runtime.block_on(async {
            for completion in [false, true] {
                // The pipe, not a delay, keeps the child alive until explicitly released.
                let mut child = ChildGuard(
                    Command::new("sh")
                        .args(["-c", "read line; exit 7"])
                        .stdin(Stdio::piped())
                        .spawn()
                        .unwrap(),
                );
                let mut op = WaitPidOp::new(child.0.id());
                let driver = crate::vibeio::executor::current_driver().unwrap();
                let mut cx = Context::from_waker(std::task::Waker::noop());
                let first = if completion {
                    op.poll_completion(&mut cx, &driver)
                } else {
                    op.poll_poll(&mut cx, &driver)
                };
                assert!(
                    first.is_pending(),
                    "running child must remain pending: {first:?}"
                );
                // A spurious wake must not complete the operation either.
                let second = if completion {
                    op.poll_completion(&mut cx, &driver)
                } else {
                    op.poll_poll(&mut cx, &driver)
                };
                assert!(
                    second.is_pending(),
                    "spurious wake completed wait: {second:?}"
                );
                drop(child.0.stdin.take());
                let raw = crate::vibeio::time::timeout(
                    crate::vibeio::test_support::WATCHDOG,
                    poll_fn(|cx| {
                        if completion {
                            op.poll_completion(cx, &driver)
                        } else {
                            op.poll_poll(cx, &driver)
                        }
                    }),
                )
                .await
                .unwrap()
                .unwrap();
                use std::os::unix::process::ExitStatusExt;
                assert_eq!(std::process::ExitStatus::from_raw(raw).code(), Some(7));
            }
        });
    }
}

/// State machine for the pidfd-based waitpid operation.
enum WaitPidState {
    /// Initial state: we have the child PID but haven't opened the pidfd yet.
    Init { pid: libc::pid_t },
    /// The pidfd is open and registered with the driver; waiting for readability.
    Polling {
        pid: libc::pid_t,
        // Deregister before closing the descriptor (fields drop in order).
        handle: InnerRawHandle,
        _pidfd: OwnedFd,
    },
    /// Terminal state after the result has been consumed.
    Done,
}

/// An async operation that waits for a child process to exit using Linux's
/// `pidfd_open(2)`.  The pidfd is registered with the I/O driver so the
/// executor is woken only when the child actually terminates — no signals,
/// no busy-polling.
pub struct WaitPidOp {
    state: WaitPidState,
}

impl WaitPidOp {
    /// Create a new `WaitPidOp` for the given child PID.
    #[inline]
    pub fn new(pid: u32) -> Self {
        Self {
            state: WaitPidState::Init {
                pid: pid as libc::pid_t,
            },
        }
    }

    /// Open a pidfd for `pid` via the `pidfd_open` syscall (Linux 5.3+).
    #[inline]
    fn open_pidfd(pid: libc::pid_t) -> io::Result<OwnedFd> {
        // SAFETY: pidfd_open takes integer arguments and returns a new descriptor.
        // The kernel sets CLOEXEC; no read or blocking waitid is performed here.
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0 as libc::c_uint) };
        if fd < 0 {
            Err(io::Error::last_os_error())
        } else {
            // SAFETY: the successful syscall returned a fresh, owned descriptor.
            Ok(unsafe { OwnedFd::from_raw_fd(fd as _) })
        }
    }

    /// Check for exit and reap without blocking, before arming pidfd readiness.
    #[inline]
    fn reap(pid: libc::pid_t) -> io::Result<i32> {
        let mut status: libc::c_int = 0;
        let rc = loop {
            // SAFETY: status is writable; waitpid takes an integer process ID.
            let rc = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
            if rc >= 0 || io::Error::last_os_error().kind() != io::ErrorKind::Interrupted {
                break rc;
            }
        };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        if rc == 0 {
            // Still running: the caller must arm readiness and remain pending.
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "child has not exited yet",
            ));
        }
        Ok(status)
    }
}

impl Op for WaitPidOp {
    type Output = i32;

    fn poll_poll(
        &mut self,
        cx: &mut Context<'_>,
        driver: &AnyDriver,
    ) -> Poll<io::Result<Self::Output>> {
        use std::os::fd::AsRawFd;
        loop {
            match &self.state {
                WaitPidState::Init { pid } => {
                    let pid = *pid;
                    let pidfd = Self::open_pidfd(pid)?;
                    let handle = InnerRawHandle::new_with_mode(
                        pidfd.as_raw_fd(),
                        Interest::READABLE,
                        crate::vibeio::driver::RegistrationMode::Poll,
                    )?;
                    self.state = WaitPidState::Polling {
                        pid,
                        handle,
                        _pidfd: pidfd,
                    };
                }
                WaitPidState::Polling { pid, handle, .. } => {
                    // A pidfd is pollable but not readable: read(pidfd) always
                    // fails with EINVAL. Check waitpid without blocking, then
                    // arm readiness if still alive, including on spurious wakes.
                    match Self::reap(*pid) {
                        Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                            driver.submit_poll(handle, cx.waker().clone(), Interest::READABLE)?;
                            return Poll::Pending;
                        }
                        result => {
                            self.state = WaitPidState::Done;
                            return Poll::Ready(result);
                        }
                    }
                }
                WaitPidState::Done => {
                    return Poll::Ready(Err(io::Error::other("WaitPidOp already completed")));
                }
            }
        }
    }

    fn poll_completion(
        &mut self,
        cx: &mut Context<'_>,
        driver: &AnyDriver,
    ) -> Poll<io::Result<Self::Output>> {
        // pidfd readiness uses a poll-mode registration on every driver.
        self.poll_poll(cx, driver)
    }
}
