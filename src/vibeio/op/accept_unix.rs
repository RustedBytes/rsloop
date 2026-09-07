#![deny(unsafe_op_in_unsafe_fn)]
#![warn(clippy::undocumented_unsafe_blocks)]

use std::io;
use std::os::fd::{FromRawFd, OwnedFd, RawFd};
use std::task::{Context, Poll};

use mio::Interest;

use crate::vibeio::driver::{AnyDriver, CompletionIoResult};
use crate::vibeio::fd_inner::InnerRawHandle;
use crate::vibeio::op::Op;
#[cfg(any(not(syscall_accept4), not(target_os = "linux")))]
use crate::vibeio::op::io_util::set_cloexec;

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
    #[cfg(target_os = "linux")]
    fn completion_returns_fd(&self) -> bool {
        true
    }
    type Output = OwnedFd;

    #[inline]
    fn poll_poll(
        &mut self,
        cx: &mut Context<'_>,
        driver: &AnyDriver,
    ) -> Poll<io::Result<Self::Output>> {
        #[cfg(syscall_accept4)]
        // SAFETY: the borrowed listener remains live, null address outputs are
        // permitted, and a successful new fd is wrapped in an owner below.
        let accepted_fd = unsafe {
            libc::accept4(
                self.handle.handle,
                std::ptr::null_mut(),
                std::ptr::null_mut(),
                libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK,
            )
        };
        #[cfg(not(syscall_accept4))]
        // SAFETY: null address outputs are permitted. The handle keeps the
        // listener open and success transfers ownership of a new descriptor.
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
        // SAFETY: accept returned a new descriptor; errors below must close it.
        let owned = unsafe { OwnedFd::from_raw_fd(fd) };
        // On non-Linux Unix, set close-on-exec manually.
        // Linux accept4() above already set it atomically.
        #[cfg(not(syscall_accept4))]
        if let Err(err) = set_cloexec(fd) {
            return Poll::Ready(Err(err));
        }

        Poll::Ready(Ok(owned))
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
            return Poll::Ready(Err(crate::vibeio::op::io_util::completion_error(result)));
        }

        let fd = result as RawFd;
        // SAFETY: the driver transferred this successful accept result.
        let owned = unsafe { OwnedFd::from_raw_fd(fd) };
        // Linux requests CLOEXEC atomically in the accept SQE below.
        #[cfg(not(target_os = "linux"))]
        if let Err(err) = set_cloexec(fd) {
            return Poll::Ready(Err(err));
        }

        Poll::Ready(Ok(owned))
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
        .flags(libc::SOCK_CLOEXEC)
        .build()
        .user_data(user_data);
        Ok(entry)
    }
}

impl Drop for AcceptUnixOp<'_> {
    #[inline]
    fn drop(&mut self) {
        if let Some(completion_token) = self.completion_token {
            self.handle
                .cancel_completion(completion_token, Box::new(()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(target_os = "linux")]
    use std::os::fd::AsRawFd;
    #[cfg(target_os = "linux")]
    use std::os::linux::net::SocketAddrExt;
    #[cfg(target_os = "linux")]
    use std::os::unix::net::{SocketAddr, UnixListener, UnixStream};
    use std::rc::Rc;
    #[cfg(target_os = "linux")]
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[cfg(target_os = "linux")]
    #[test]
    fn discarding_poll_accept_result_closes_the_connection() {
        let name = format!("vibeio-accept-owned-{}", std::process::id());
        let address = SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
        let listener = UnixListener::bind_addr(&address).unwrap();
        listener.set_nonblocking(true).unwrap();
        let driver = Rc::new(AnyDriver::new_mio().unwrap());
        let handle = InnerRawHandle::new_with_driver_and_mode(
            &driver,
            listener.as_raw_fd(),
            Interest::READABLE,
            crate::vibeio::driver::RegistrationMode::Poll,
        )
        .unwrap();
        let mut op = AcceptUnixOp::new(&handle);
        let mut cx = Context::from_waker(std::task::Waker::noop());
        assert!(matches!(op.poll_poll(&mut cx, &driver), Poll::Pending));
        let mut peer = UnixStream::connect_addr(&address).unwrap();
        peer.set_nonblocking(true).unwrap();
        let result = crate::vibeio::test_support::poll_io(
            || op.poll_poll(&mut cx, &driver),
            || driver.wait(Some(std::time::Duration::from_millis(100))),
        )
        .expect("Unix accept should complete after client connects");
        drop(result);
        crate::vibeio::test_support::assert_eof(&mut peer);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn discarding_completion_accept_results_closes_tcp_and_unix_connections() {
        use crate::vibeio::driver::RegistrationMode;
        use crate::vibeio::op::AcceptOp;
        use std::time::{Duration, Instant};

        fn complete<O: Op>(op: &mut O, driver: &AnyDriver) -> O::Output {
            let deadline = Instant::now() + crate::vibeio::test_support::WATCHDOG;
            let mut cx = Context::from_waker(std::task::Waker::noop());
            loop {
                if let Poll::Ready(result) = op.poll_completion(&mut cx, driver) {
                    return result.unwrap();
                }
                assert!(Instant::now() < deadline, "accept completion timed out");
                driver.wait(Some(Duration::from_millis(10)));
            }
        }

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
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let handle = InnerRawHandle::new_with_driver_and_mode(
            &driver,
            listener.as_raw_fd(),
            Interest::READABLE,
            RegistrationMode::Completion,
        )
        .unwrap();
        let mut peer = std::net::TcpStream::connect(listener.local_addr().unwrap()).unwrap();
        peer.set_read_timeout(Some(crate::vibeio::test_support::WATCHDOG))
            .unwrap();
        let mut op = AcceptOp::new(&handle);
        let accepted = complete(&mut op, &driver);
        assert_eq!(accepted.1, peer.local_addr().unwrap());
        drop(accepted);
        crate::vibeio::test_support::assert_eof(&mut peer);
        drop(op);
        drop(handle);

        let name = format!("vibeio-accept-owned-completion-{}", std::process::id());
        let address = SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
        let listener = UnixListener::bind_addr(&address).unwrap();
        let handle = InnerRawHandle::new_with_driver_and_mode(
            &driver,
            listener.as_raw_fd(),
            Interest::READABLE,
            RegistrationMode::Completion,
        )
        .unwrap();
        let mut peer = UnixStream::connect_addr(&address).unwrap();
        peer.set_nonblocking(true).unwrap();
        let mut op = AcceptUnixOp::new(&handle);
        drop(complete(&mut op, &driver));
        crate::vibeio::test_support::assert_eof(&mut peer);
    }

    #[test]
    fn cancelled_accept_uses_the_owning_driver() {
        for entered in [false, true] {
            let owner = Rc::new(AnyDriver::new_mock());
            let handle = InnerRawHandle::for_mock_completion(owner.clone());
            let cancel = move || {
                let mut op = AcceptUnixOp::new(&handle);
                op.completion_token = Some(41);
                drop(op);
            };
            if entered {
                let runtime = crate::vibeio::executor::Runtime::new(AnyDriver::new_mock());
                runtime.block_on(async move { cancel() });
            } else {
                cancel();
            }
            let AnyDriver::Mock(driver) = owner.as_ref() else {
                unreachable!()
            };
            let held = driver.ignored.take();
            assert_eq!(held.len(), 1);
            assert_eq!(held[0].0, 41);
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn completion_accept_creates_close_on_exec_descriptor() {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let name = format!(
            "vibeio-accept-flags-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        );
        let address = SocketAddr::from_abstract_name(name.as_bytes()).unwrap();
        let listener = UnixListener::bind_addr(&address).unwrap();
        listener.set_nonblocking(true).unwrap();
        let _peer = UnixStream::connect_addr(&address).unwrap();
        let mut handle = InnerRawHandle::for_mock_completion(Rc::new(AnyDriver::new_mock()));
        handle.handle = listener.as_raw_fd();
        let mut op = AcceptUnixOp::new(&handle);
        let entry = op.build_completion_entry(17).unwrap();
        let mut ring = io_uring::IoUring::new(2).unwrap();
        // SAFETY: the listener remains open until completion and accept uses no
        // userspace address outputs. The returned descriptor is claimed below.
        unsafe { ring.submission().push(&entry).unwrap() };
        ring.submit_and_wait(1).unwrap();
        let completion = ring.completion().next().unwrap();
        assert_eq!(completion.user_data(), 17);
        assert!(
            completion.result() >= 0,
            "accept failed: {}",
            completion.result()
        );
        // SAFETY: the successful accept CQE transfers a new owned descriptor.
        let accepted = unsafe { OwnedFd::from_raw_fd(completion.result()) };
        // Inspect the kernel result without running poll_completion's finishing
        // code: setting CLOEXEC there would hide the inheritance window.
        // SAFETY: accepted owns a live fd; F_GETFD has no pointer arguments.
        let flags = unsafe { libc::fcntl(accepted.as_raw_fd(), libc::F_GETFD) };
        assert_ne!(flags, -1);
        assert_ne!(flags & libc::FD_CLOEXEC, 0);
        // SAFETY: accepted still owns the fd; F_GETFL only queries integer flags.
        let status = unsafe { libc::fcntl(accepted.as_raw_fd(), libc::F_GETFL) };
        assert_ne!(status, -1);
        assert_eq!(
            status & libc::O_NONBLOCK,
            0,
            "completion mode remains blocking"
        );
    }
}
