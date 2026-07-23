use std::io;
use std::os::fd::RawFd;
use std::task::{Context, Poll};

use mio::Interest;

use crate::driver::AnyDriver;
use crate::fd_inner::InnerRawHandle;
use crate::op::Op;

/// Wraps a raw pidfd and closes it on drop.
struct OwnedPidFd(RawFd);

impl Drop for OwnedPidFd {
    #[inline]
    fn drop(&mut self) {
        if self.0 >= 0 {
            unsafe {
                libc::close(self.0);
            }
        }
    }
}

/// State machine for the pidfd-based waitpid operation.
enum WaitPidState {
    /// Initial state: we have the child PID but haven't opened the pidfd yet.
    Init { pid: libc::pid_t },
    /// The pidfd is open and registered with the driver; waiting for readability.
    Polling {
        pid: libc::pid_t,
        pidfd: OwnedPidFd,
        handle: InnerRawHandle,
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
    fn open_pidfd(pid: libc::pid_t) -> io::Result<RawFd> {
        let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0 as libc::c_uint) };
        if fd < 0 {
            Err(io::Error::last_os_error())
        } else {
            let raw = fd as RawFd;
            // Set non-blocking + close-on-exec atomically via a single fcntl
            // call chain. O_NONBLOCK is required so that read() on the pidfd
            // returns EAGAIN instead of blocking when the child hasn't exited
            // yet, and FD_CLOEXEC prevents leaking across exec.
            let flags = unsafe { libc::fcntl(raw, libc::F_GETFL) };
            if flags != -1 {
                unsafe {
                    libc::fcntl(raw, libc::F_SETFL, flags | libc::O_NONBLOCK);
                }
            }
            let fd_flags = unsafe { libc::fcntl(raw, libc::F_GETFD) };
            if fd_flags != -1 {
                unsafe {
                    libc::fcntl(raw, libc::F_SETFD, fd_flags | libc::FD_CLOEXEC);
                }
            }
            Ok(raw)
        }
    }

    /// Reap the child with `waitpid` after the pidfd signalled readability.
    #[inline]
    fn reap(pid: libc::pid_t) -> io::Result<i32> {
        let mut status: libc::c_int = 0;
        let rc = unsafe { libc::waitpid(pid, &mut status, libc::WNOHANG) };
        if rc < 0 {
            return Err(io::Error::last_os_error());
        }
        if rc == 0 {
            // Shouldn't happen after pidfd is readable, but be defensive.
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "child has not exited yet",
            ));
        }
        Ok(status)
    }
}

impl Op for WaitPidOp {
    type Output = i32; // raw waitpid status

    #[inline]
    fn poll_poll(
        &mut self,
        cx: &mut Context<'_>,
        driver: &AnyDriver,
    ) -> Poll<io::Result<Self::Output>> {
        loop {
            match &self.state {
                WaitPidState::Init { pid } => {
                    let pid = *pid;
                    let raw_fd = Self::open_pidfd(pid)?;
                    let pidfd = OwnedPidFd(raw_fd);
                    let handle = InnerRawHandle::new_with_mode(
                        raw_fd,
                        Interest::READABLE,
                        crate::driver::RegistrationMode::Poll,
                    )?;
                    self.state = WaitPidState::Polling { pid, pidfd, handle };
                    // Fall through to the Polling arm.
                }
                WaitPidState::Polling {
                    pid,
                    pidfd: _,
                    handle,
                } => {
                    // Try a non-blocking read on the pidfd. If it's readable the
                    // child has exited.
                    let mut buf = [0u8; 8];
                    let n = unsafe {
                        libc::read(
                            handle.handle,
                            buf.as_mut_ptr().cast::<libc::c_void>(),
                            buf.len(),
                        )
                    };
                    if n < 0 {
                        let err = io::Error::last_os_error();
                        if err.kind() == io::ErrorKind::WouldBlock {
                            // Not ready yet — register for readability and pend.
                            if let Err(submit_err) =
                                driver.submit_poll(handle, cx.waker().clone(), Interest::READABLE)
                            {
                                return Poll::Ready(Err(submit_err));
                            }
                            return Poll::Pending;
                        }
                        // EBADF or similar — try reaping anyway
                    }

                    let pid = *pid;
                    let status = Self::reap(pid);
                    self.state = WaitPidState::Done;
                    return Poll::Ready(status);
                }
                WaitPidState::Done => {
                    return Poll::Ready(Err(io::Error::other("WaitPidOp already completed")));
                }
            }
        }
    }

    #[inline]
    fn poll_completion(
        &mut self,
        cx: &mut Context<'_>,
        driver: &AnyDriver,
    ) -> Poll<io::Result<Self::Output>> {
        // We need to take ownership to transition states. Use a temporary Done.
        match std::mem::replace(&mut self.state, WaitPidState::Done) {
            WaitPidState::Init { pid } => {
                let raw_fd = Self::open_pidfd(pid)?;
                let pidfd = OwnedPidFd(raw_fd);
                // For io_uring we register in Poll mode so we can use PollAdd
                // to wait for readability on the pidfd.
                let handle = InnerRawHandle::new_with_mode(
                    raw_fd,
                    Interest::READABLE,
                    crate::driver::RegistrationMode::Poll,
                )?;
                // Submit the poll via the driver (which uses io_uring PollAdd
                // on uring, or mio on poll-based).
                if let Err(submit_err) =
                    driver.submit_poll(&handle, cx.waker().clone(), Interest::READABLE)
                {
                    self.state = WaitPidState::Polling { pid, pidfd, handle };
                    return Poll::Ready(Err(submit_err));
                }
                self.state = WaitPidState::Polling { pid, pidfd, handle };
                Poll::Pending
            }
            WaitPidState::Polling { pid, pidfd, handle } => {
                // Check if the pidfd is readable (child exited).
                let mut buf = [0u8; 8];
                let n = unsafe {
                    libc::read(
                        handle.handle,
                        buf.as_mut_ptr().cast::<libc::c_void>(),
                        buf.len(),
                    )
                };
                if n < 0 {
                    let err = io::Error::last_os_error();
                    if err.kind() == io::ErrorKind::WouldBlock {
                        // Re-arm poll.
                        if let Err(submit_err) =
                            driver.submit_poll(&handle, cx.waker().clone(), Interest::READABLE)
                        {
                            self.state = WaitPidState::Polling { pid, pidfd, handle };
                            return Poll::Ready(Err(submit_err));
                        }
                        self.state = WaitPidState::Polling { pid, pidfd, handle };
                        return Poll::Pending;
                    }
                    // Other error — try reaping anyway.
                }

                let status = Self::reap(pid);
                drop(handle);
                drop(pidfd);
                self.state = WaitPidState::Done;
                Poll::Ready(status)
            }
            WaitPidState::Done => Poll::Ready(Err(io::Error::other("WaitPidOp already completed"))),
        }
    }
}
