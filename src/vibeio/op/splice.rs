use std::io;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::task::{Context, Poll};

use mio::Interest;

use crate::vibeio::driver::CompletionIoResult;
use crate::vibeio::driver::{AnyDriver, RegistrationMode};
use crate::vibeio::fd_inner::InnerRawHandle;
use crate::vibeio::op::Op;
use crate::vibeio::op::io_util::poll_result_or_wait;

pub struct SpliceOp<'a> {
    fd_in: RawFd,
    fd_out: &'a InnerRawHandle,
    len: usize,
    completion_token: Option<usize>,
    source_registration: Option<SourceRegistration>,
    completion_fds: Option<[OwnedFd; 2]>,
}

// AsRawFd does not promise the validity required by BorrowedFd::borrow_raw.
// Ask the kernel to validate and duplicate the descriptor instead.
fn duplicate_fd(fd: RawFd) -> io::Result<OwnedFd> {
    loop {
        // SAFETY: F_DUPFD_CLOEXEC takes an integer minimum descriptor number.
        let duplicated = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 0) };
        if duplicated >= 0 {
            // SAFETY: successful fcntl returned a fresh, owned descriptor.
            return Ok(unsafe { OwnedFd::from_raw_fd(duplicated) });
        }
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
}

struct SourceRegistration {
    // Deregister before closing the duplicate. A separate descriptor allows
    // observing a source that is already registered by another runtime handle.
    handle: InnerRawHandle,
    _fd: OwnedFd,
}

impl<'a> SpliceOp<'a> {
    #[inline]
    pub fn new(fd_in: RawFd, fd_out: &'a InnerRawHandle, len: usize) -> Self {
        Self {
            fd_in,
            fd_out,
            // The completion ABI has a 32-bit length. A larger request must
            // make a short transfer, never wrap to zero and masquerade as EOF.
            len: len.min(u32::MAX as usize),
            completion_token: None,
            source_registration: None,
            completion_fds: None,
        }
    }

    fn source_ready(&self) -> io::Result<bool> {
        let mut descriptor = libc::pollfd {
            fd: self.fd_in,
            events: libc::POLLIN,
            revents: 0,
        };
        loop {
            // SAFETY: one initialized pollfd is exclusively borrowed for this
            // synchronous call. A zero timeout never waits for source data.
            let result = unsafe { libc::poll(&mut descriptor, 1, 0) };
            if result >= 0 {
                // EOF/error readiness also permits the splice syscall to
                // report its result. Regular files are always poll-ready.
                return Ok(result != 0);
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }

    fn source_handle(&mut self) -> io::Result<&InnerRawHandle> {
        if self.source_registration.is_none() {
            let fd = duplicate_fd(self.fd_in)?;
            let handle = InnerRawHandle::new_with_driver_and_mode(
                &self.fd_out.driver_owner(),
                fd.as_raw_fd(),
                Interest::READABLE,
                RegistrationMode::Poll,
            )?;
            self.source_registration = Some(SourceRegistration { handle, _fd: fd });
        }
        Ok(&self.source_registration.as_ref().unwrap().handle)
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
                    self.fd_in,
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

        if result
            .as_ref()
            .is_err_and(|err| err.kind() == io::ErrorKind::WouldBlock)
        {
            match self.source_ready() {
                Ok(false) => {
                    let handle = match self.source_handle() {
                        Ok(handle) => handle,
                        Err(error) => return Poll::Ready(Err(error)),
                    };
                    return poll_result_or_wait(result, handle, cx, driver, Interest::READABLE);
                }
                Err(error) => return Poll::Ready(Err(error)),
                Ok(true) => {}
            }
        }
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

        if self.completion_fds.is_none() {
            // The SQE may not reach the kernel until after the future is
            // cancelled and its callers close their descriptors. Keep the
            // exact descriptor numbers used by the SQE alive through its CQE.
            self.completion_fds =
                Some([duplicate_fd(self.fd_in)?, duplicate_fd(self.fd_out.handle)?]);
        }
        let [source, destination] = self.completion_fds.as_ref().unwrap();
        let entry = opcode::Splice::new(
            types::Fd(source.as_raw_fd()),
            -1,
            types::Fd(destination.as_raw_fd()),
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
            self.fd_out
                .cancel_completion(completion_token, Box::new(self.completion_fds.take()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vibeio::driver::RegistrationMode;
    use std::io::{Read, Write};
    use std::rc::Rc;
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    use std::task::{Wake, Waker};
    use std::time::Duration;

    struct WakeCount(AtomicUsize);

    #[test]
    fn queued_splice_survives_cancellation_and_closing_original_descriptors() {
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
        let (input, mut producer) = std::io::pipe().unwrap();
        let (mut consumer, destination) = std::io::pipe().unwrap();
        crate::vibeio::fd_inner::set_nonblocking(consumer.as_raw_fd(), true).unwrap();
        producer.write_all(b"x").unwrap();
        let handle = InnerRawHandle::new_with_driver_and_mode(
            &driver,
            destination.as_raw_fd(),
            Interest::WRITABLE,
            RegistrationMode::Completion,
        )
        .unwrap();
        let mut op = SpliceOp::new(input.as_raw_fd(), &handle, 1);
        assert!(
            op.poll_completion(&mut Context::from_waker(Waker::noop()), &driver)
                .is_pending()
        );
        // No flush has happened yet: the SQE still contains descriptor numbers
        // in userspace, and cancellation must retain exactly those descriptors.
        drop(op);
        drop(handle);
        drop(input);
        drop(destination);
        let deadline = std::time::Instant::now() + Duration::from_secs(2);
        let mut received = Vec::new();
        loop {
            driver.wait(Some(Duration::from_millis(10)));
            let mut byte = [0];
            match consumer.read(&mut byte) {
                Ok(0) => break, // CQE cleanup closed the last destination owner.
                Ok(1) => received.push(byte[0]),
                Ok(_) => unreachable!(),
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => panic!("reading splice result failed: {error}"),
            }
            assert!(
                std::time::Instant::now() < deadline,
                "splice failed to finish and release its descriptors"
            );
        }
        assert_eq!(&received, b"x");
    }

    #[test]
    fn cancelled_completion_retains_both_descriptors_on_its_owner() {
        let driver = Rc::new(AnyDriver::new_mock());
        let (input, mut producer) = std::io::pipe().unwrap();
        let (mut consumer, destination) = std::io::pipe().unwrap();
        let mut handle = InnerRawHandle::for_mock_completion(driver.clone());
        handle.handle = destination.as_raw_fd();
        let mut op = SpliceOp::new(input.as_raw_fd(), &handle, 1);
        op.build_completion_entry(7).unwrap();
        let descriptors = op
            .completion_fds
            .as_ref()
            .unwrap()
            .each_ref()
            .map(AsRawFd::as_raw_fd);
        assert_ne!(descriptors[0], input.as_raw_fd());
        assert_ne!(descriptors[1], destination.as_raw_fd());
        op.build_completion_entry(7).unwrap();
        assert_eq!(
            op.completion_fds
                .as_ref()
                .unwrap()
                .each_ref()
                .map(AsRawFd::as_raw_fd),
            descriptors
        );
        // Model a queued SQE; no ambient runtime is entered during cancellation.
        op.completion_token = Some(7);
        drop(op);
        drop(handle);
        drop(input);
        drop(destination);
        let AnyDriver::Mock(mock) = driver.as_ref() else {
            unreachable!()
        };
        let (token, payload) = mock.ignored.borrow_mut().pop().unwrap();
        assert_eq!(token, 7);
        let [input, destination] = (*payload.downcast::<Option<[OwnedFd; 2]>>().unwrap()).unwrap();
        assert_eq!([input.as_raw_fd(), destination.as_raw_fd()], descriptors);
        let mut input = std::fs::File::from(input);
        let mut destination = std::fs::File::from(destination);
        producer.write_all(b"x").unwrap();
        let mut byte = [0];
        input.read_exact(&mut byte).unwrap();
        assert_eq!(&byte, b"x");
        destination.write_all(b"y").unwrap();
        consumer.read_exact(&mut byte).unwrap();
        assert_eq!(&byte, b"y");
    }

    #[test]
    fn invalid_completion_descriptors_fail_without_retained_storage() {
        let driver = Rc::new(AnyDriver::new_mock());
        let handle = InnerRawHandle::for_mock_completion(driver);
        let (input, _producer) = std::io::pipe().unwrap();
        // First fail source duplication, then destination duplication after
        // obtaining a valid source duplicate. Neither may publish an SQE.
        for source in [-1, input.as_raw_fd()] {
            let mut op = SpliceOp::new(source, &handle, 1);
            let error = op.build_completion_entry(0).unwrap_err();
            assert_eq!(error.raw_os_error(), Some(libc::EBADF));
            assert!(op.completion_fds.is_none());
            assert!(op.completion_token.is_none());
        }
    }

    impl Wake for WakeCount {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn empty_source_waits_for_input_not_writable_destination() {
        let driver = Rc::new(AnyDriver::new_mio().unwrap());
        let (input, mut producer) = std::io::pipe().unwrap();
        let (mut output, destination) = std::io::pipe().unwrap();
        let _source_handle = InnerRawHandle::new_with_driver_and_mode(
            &driver,
            input.as_raw_fd(),
            Interest::READABLE,
            RegistrationMode::Poll,
        )
        .unwrap();
        let handle = InnerRawHandle::new_with_driver_and_mode(
            &driver,
            destination.as_raw_fd(),
            Interest::WRITABLE,
            RegistrationMode::Poll,
        )
        .unwrap();
        let wakes = Arc::new(WakeCount(AtomicUsize::new(0)));
        let waker = Waker::from(wakes.clone());
        let mut cx = Context::from_waker(&waker);
        let mut op = SpliceOp::new(input.as_raw_fd(), &handle, 1);
        assert!(op.poll_poll(&mut cx, &driver).is_pending());
        driver.wait(Some(Duration::ZERO));
        assert_eq!(
            wakes.0.load(Ordering::SeqCst),
            0,
            "an empty source must not wake on destination writability"
        );
        producer.write_all(b"x").unwrap();
        driver.wait(Some(Duration::from_millis(100)));
        assert_eq!(wakes.0.load(Ordering::SeqCst), 1);
        assert!(matches!(op.poll_poll(&mut cx, &driver), Poll::Ready(Ok(1))));
        let mut byte = [0];
        output.read_exact(&mut byte).unwrap();
        assert_eq!(&byte, b"x");
    }

    #[test]
    fn full_destination_waits_for_output_with_ready_input() {
        let driver = Rc::new(AnyDriver::new_mio().unwrap());
        let (input, mut producer) = std::io::pipe().unwrap();
        let (mut output, mut destination) = std::io::pipe().unwrap();
        crate::vibeio::fd_inner::set_nonblocking(destination.as_raw_fd(), true).unwrap();
        let mut filled = 0;
        loop {
            match destination.write(&[0; 4096]) {
                Ok(count) => {
                    assert_ne!(count, 0);
                    filled += count;
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => panic!("filling destination failed: {error}"),
            }
        }
        producer.write_all(b"x").unwrap();
        let handle = InnerRawHandle::new_with_driver_and_mode(
            &driver,
            destination.as_raw_fd(),
            Interest::WRITABLE,
            RegistrationMode::Poll,
        )
        .unwrap();
        let wakes = Arc::new(WakeCount(AtomicUsize::new(0)));
        let waker = Waker::from(wakes.clone());
        let mut cx = Context::from_waker(&waker);
        let mut op = SpliceOp::new(input.as_raw_fd(), &handle, 1);
        assert!(op.poll_poll(&mut cx, &driver).is_pending());
        assert!(op.source_registration.is_none());
        driver.wait(Some(Duration::ZERO));
        assert_eq!(wakes.0.load(Ordering::SeqCst), 0);
        output.read_exact(&mut vec![0; filled]).unwrap();
        driver.wait(Some(Duration::from_millis(100)));
        assert_eq!(wakes.0.load(Ordering::SeqCst), 1);
        assert!(matches!(op.poll_poll(&mut cx, &driver), Poll::Ready(Ok(1))));
        let mut byte = [0];
        output.read_exact(&mut byte).unwrap();
        assert_eq!(&byte, b"x");
    }

    #[test]
    fn cancelled_source_waiter_is_removed_and_eof_wakes_its_replacement() {
        let driver = Rc::new(AnyDriver::new_mio().unwrap());
        let (input, producer) = std::io::pipe().unwrap();
        let (_output, destination) = std::io::pipe().unwrap();
        let handle = InnerRawHandle::new_with_driver_and_mode(
            &driver,
            destination.as_raw_fd(),
            Interest::WRITABLE,
            RegistrationMode::Poll,
        )
        .unwrap();
        let cancelled = Arc::new(WakeCount(AtomicUsize::new(0)));
        let waker = Waker::from(cancelled.clone());
        let mut op = SpliceOp::new(input.as_raw_fd(), &handle, 1);
        assert!(
            op.poll_poll(&mut Context::from_waker(&waker), &driver)
                .is_pending()
        );
        drop(op);
        let replacement = Arc::new(WakeCount(AtomicUsize::new(0)));
        let waker = Waker::from(replacement.clone());
        let mut cx = Context::from_waker(&waker);
        let mut op = SpliceOp::new(input.as_raw_fd(), &handle, 1);
        assert!(op.poll_poll(&mut cx, &driver).is_pending());
        drop(producer);
        driver.wait(Some(Duration::from_millis(100)));
        assert_eq!(cancelled.0.load(Ordering::SeqCst), 0);
        assert_eq!(replacement.0.load(Ordering::SeqCst), 1);
        assert!(matches!(op.poll_poll(&mut cx, &driver), Poll::Ready(Ok(0))));
    }

    #[test]
    fn oversized_splice_requests_do_not_wrap_to_zero() {
        let (reader, _writer) = std::io::pipe().unwrap();
        let driver = Rc::new(AnyDriver::new_mock());
        let handle = InnerRawHandle::for_mock_completion(driver);
        for len in [
            0,
            17,
            u32::MAX as usize,
            (u32::MAX as usize).saturating_add(1),
            usize::MAX,
        ] {
            let op = SpliceOp::new(reader.as_raw_fd(), &handle, len);
            assert_eq!(op.len, len.min(u32::MAX as usize));
        }
    }
}
