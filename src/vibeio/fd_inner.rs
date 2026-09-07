use std::rc::Rc;
use std::task::Poll;
use std::{io, task::Context};

use mio::{Interest, Token};

use crate::vibeio::{
    driver::{AnyDriver, RegistrationMode},
    executor::current_driver,
    op::Op,
};

#[cfg(unix)]
pub type RawOsHandle = std::os::fd::RawFd;
#[cfg(windows)]
#[derive(Copy, Clone, Debug, Eq, PartialEq, Hash)]
pub enum RawOsHandle {
    Socket(std::os::windows::io::RawSocket),
    #[allow(dead_code)]
    Handle(std::os::windows::io::RawHandle),
}

pub struct InnerRawHandle {
    pub(crate) handle: RawOsHandle,
    pub(crate) token: Token,
    interest: Interest,
    mode: RegistrationMode,
    driver: Rc<AnyDriver>,
}

// Slab-backed drivers cannot allocate this index. A handle owns a registration
// only after registration succeeds, and relinquishes it before re-registering.
const UNREGISTERED: Token = Token(usize::MAX);

/// Set the descriptor's blocking mode without changing unrelated status flags.
#[cfg(all(
    unix,
    any(
        test,
        feature = "pipe",
        feature = "process",
        feature = "signal",
        all(target_os = "linux", feature = "splice")
    )
))]
pub(crate) fn set_nonblocking(fd: RawOsHandle, nonblocking: bool) -> io::Result<()> {
    fn fcntl(fd: RawOsHandle, command: libc::c_int, value: libc::c_int) -> io::Result<libc::c_int> {
        loop {
            // SAFETY: F_GETFL and F_SETFL take integer arguments, not pointers.
            // An invalid descriptor is reported by the syscall as an error.
            let result = unsafe { libc::fcntl(fd, command, value) };
            if result != -1 {
                return Ok(result);
            }
            let error = io::Error::last_os_error();
            if error.kind() != io::ErrorKind::Interrupted {
                return Err(error);
            }
        }
    }
    let flags = fcntl(fd, libc::F_GETFL, 0)?;
    let updated = if nonblocking {
        flags | libc::O_NONBLOCK
    } else {
        flags & !libc::O_NONBLOCK
    };
    if updated != flags {
        fcntl(fd, libc::F_SETFL, updated)?;
    }
    Ok(())
}

impl InnerRawHandle {
    #[cfg(test)]
    pub(crate) fn for_mock_completion(driver: Rc<AnyDriver>) -> Self {
        assert!(matches!(driver.as_ref(), AnyDriver::Mock(_)));
        Self {
            #[cfg(unix)]
            handle: -1,
            #[cfg(windows)]
            handle: RawOsHandle::Socket(usize::MAX as _),
            token: UNREGISTERED,
            interest: Interest::READABLE | Interest::WRITABLE,
            mode: RegistrationMode::Completion,
            driver,
        }
    }
    /// Share ownership with operations using this registration's driver.
    #[cfg(all(target_os = "linux", any(feature = "fs", feature = "splice")))]
    pub(crate) fn driver_owner(&self) -> Rc<AnyDriver> {
        self.driver.clone()
    }

    /// Retain operation storage on the registration's owner until completion.
    #[inline]
    pub(crate) fn cancel_completion(&self, token: usize, data: Box<dyn std::any::Any>) {
        #[cfg(windows)]
        self.driver.cancel_completion(token, self.handle, data);
        #[cfg(not(windows))]
        self.driver.ignore_completion(token, data);
    }

    #[inline]
    pub(crate) fn new(handle: RawOsHandle, interest: Interest) -> Result<Self, io::Error> {
        let default_mode = if current_driver()
            .as_ref()
            .is_some_and(|driver| driver.supports_completion())
        {
            RegistrationMode::Completion
        } else {
            RegistrationMode::Poll
        };

        Self::new_with_mode(handle, interest, default_mode)
    }

    #[inline]
    pub(crate) fn new_with_mode(
        handle: RawOsHandle,
        interest: Interest,
        mode: RegistrationMode,
    ) -> Result<Self, io::Error> {
        let driver = current_driver().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::NotConnected,
                "can't register I/O handle outside runtime",
            )
        })?;
        Self::new_with_driver_and_mode(&driver, handle, interest, mode)
    }

    #[inline]
    pub(crate) fn new_with_driver_and_mode(
        driver: &Rc<AnyDriver>,
        handle: RawOsHandle,
        interest: Interest,
        mode: RegistrationMode,
    ) -> Result<Self, io::Error> {
        let mode = if matches!(mode, RegistrationMode::Completion) && !driver.supports_completion()
        {
            RegistrationMode::Poll
        } else {
            mode
        };
        let mut inner = InnerRawHandle {
            handle,
            token: UNREGISTERED,
            interest,
            mode,
            driver: driver.clone(),
        };

        inner.token = driver.register_handle_with_mode(&inner, interest, mode)?;
        Ok(inner)
    }

    #[cfg(unix)]
    #[inline]
    pub(crate) fn token(&self) -> Token {
        self.token
    }

    #[allow(dead_code)]
    #[inline]
    pub(crate) fn reregister(&self, interest: Interest) -> Result<(), io::Error> {
        self.driver.reregister_handle(self, interest)
    }

    #[inline]
    pub(crate) fn supports_completion(&self) -> bool {
        self.driver.supports_completion()
    }

    #[inline]
    pub(crate) fn uses_completion(&self) -> bool {
        self.supports_completion() && matches!(self.mode, RegistrationMode::Completion)
    }

    #[allow(dead_code)]
    #[inline]
    pub(crate) fn mode(&self) -> RegistrationMode {
        self.mode
    }

    /// Replace the registration. If deregistration fails, return its error
    /// without attempting a replacement. If acquiring the new registration fails, the
    /// handle is unregistered: callers must drop it or retry before doing I/O.
    #[inline]
    pub(crate) fn rebind_mode(
        &mut self,
        requested_mode: RegistrationMode,
    ) -> Result<(), io::Error> {
        let mode = if matches!(requested_mode, RegistrationMode::Completion)
            && !self.driver.supports_completion()
        {
            RegistrationMode::Poll
        } else {
            requested_mode
        };

        if self.mode == mode && self.token != UNREGISTERED {
            return Ok(());
        }

        if self.token != UNREGISTERED {
            self.driver.deregister_handle(self)?;
            self.token = UNREGISTERED;
        }
        self.token = self
            .driver
            .register_handle_with_mode(self, self.interest, mode)?;
        self.mode = mode;
        Ok(())
    }

    #[inline]
    pub(crate) fn poll_op<O, R>(
        &self,
        cx: &mut Context<'_>,
        op: &mut O,
    ) -> Poll<Result<R, io::Error>>
    where
        O: Op<Output = R>,
    {
        if self.uses_completion() {
            op.poll_completion(cx, &self.driver)
        } else {
            op.poll_poll(cx, &self.driver)
        }
    }

    #[inline]
    pub(crate) fn poll_op_poll<O, R>(
        &self,
        cx: &mut Context<'_>,
        op: &mut O,
    ) -> Poll<Result<R, io::Error>>
    where
        O: Op<Output = R>,
    {
        if self.uses_completion() {
            return Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::Unsupported,
                "poll-based I/O operation called on a completion-based I/O handle",
            )));
        }
        op.poll_poll(cx, &self.driver)
    }
}

impl Drop for InnerRawHandle {
    #[inline]
    fn drop(&mut self) {
        if self.token != UNREGISTERED {
            let _ = self.driver.deregister_handle(self);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::fd::AsRawFd;
    #[cfg(unix)]
    use std::os::unix::net::UnixStream;

    #[cfg(windows)]
    #[test]
    fn failed_iocp_detachment_preserves_registration_for_retry() {
        use std::os::windows::io::AsRawSocket;
        let driver = Rc::new(AnyDriver::new_iocp().unwrap());
        let socket = socket2::Socket::new(
            socket2::Domain::IPV4,
            socket2::Type::STREAM,
            Some(socket2::Protocol::TCP),
        )
        .unwrap();
        let raw = RawOsHandle::Socket(socket.as_raw_socket());
        let mut handle = InnerRawHandle::new_with_driver_and_mode(
            &driver,
            raw,
            Interest::READABLE,
            RegistrationMode::Completion,
        )
        .unwrap();
        let original_token = handle.token;
        // Inject an invalid target without closing the original owned socket.
        handle.handle = RawOsHandle::Handle(std::ptr::null_mut());
        let result = handle.rebind_mode(RegistrationMode::Poll);
        handle.handle = raw;
        assert!(result.is_err());
        assert_eq!(handle.token, original_token);
        assert!(handle.uses_completion());
        // Retrying detachment must find the original registration, then a new
        // port must be able to associate the still-live socket.
        driver.deregister_handle(&handle).unwrap();
        handle.token = UNREGISTERED;
        drop(handle);
        let other_driver = Rc::new(AnyDriver::new_iocp().unwrap());
        let other = InnerRawHandle::new_with_driver_and_mode(
            &other_driver,
            raw,
            Interest::READABLE,
            RegistrationMode::Completion,
        )
        .unwrap();
        drop(other);
        assert_eq!(socket.r#type().unwrap(), socket2::Type::STREAM);
    }

    #[cfg(unix)]
    #[test]
    fn descriptor_mode_changes_preserve_other_flags_and_report_invalid_fd() {
        let (socket, _peer) = UnixStream::pair().unwrap();
        let flags = || {
            // SAFETY: F_GETFL queries a live owned descriptor without pointers.
            let result = unsafe { libc::fcntl(socket.as_raw_fd(), libc::F_GETFL) };
            assert_ne!(result, -1);
            result
        };
        let original = flags();
        for nonblocking in [true, true, false, false] {
            set_nonblocking(socket.as_raw_fd(), nonblocking).unwrap();
            assert_eq!(flags() & !libc::O_NONBLOCK, original & !libc::O_NONBLOCK);
            assert_eq!(flags() & libc::O_NONBLOCK != 0, nonblocking);
        }
        assert_eq!(
            set_nonblocking(-1, true).unwrap_err().raw_os_error(),
            Some(libc::EBADF)
        );
    }

    #[cfg(unix)]
    #[test]
    fn failed_registration_does_not_deregister_an_existing_handle() {
        let driver = Rc::new(AnyDriver::new_mio().unwrap());
        let (socket, _peer) = UnixStream::pair().unwrap();
        socket.set_nonblocking(true).unwrap();
        let handle = InnerRawHandle::new_with_driver_and_mode(
            &driver,
            socket.as_raw_fd(),
            Interest::READABLE,
            RegistrationMode::Poll,
        )
        .unwrap();
        assert_eq!(handle.token, Token(0));

        // epoll/kqueue cannot register an invalid descriptor. Dropping the
        // partially constructed wrapper must not touch the live token above.
        assert!(
            InnerRawHandle::new_with_driver_and_mode(
                &driver,
                -1,
                Interest::READABLE,
                RegistrationMode::Poll,
            )
            .is_err()
        );
        handle.reregister(Interest::READABLE).unwrap();
    }

    #[test]
    fn failed_mode_switch_relinquishes_token_and_can_retry() {
        for retry in [false, true] {
            let mut driver = AnyDriver::new_mock();
            let AnyDriver::Mock(mock) = &mut driver else {
                unreachable!()
            };
            mock.registrations = Some(Default::default());
            mock.registrations
                .as_ref()
                .unwrap()
                .results
                .borrow_mut()
                .extend([
                    Ok(Token(0)),
                    Err(io::Error::other("injected registration failure")),
                    Ok(Token(1)),
                ]);
            let driver = Rc::new(driver);
            #[cfg(unix)]
            let raw = -1;
            #[cfg(windows)]
            let raw = RawOsHandle::Socket(usize::MAX as _);
            let mut handle = InnerRawHandle::new_with_driver_and_mode(
                &driver,
                raw,
                Interest::READABLE,
                RegistrationMode::Completion,
            )
            .unwrap();
            assert!(handle.rebind_mode(RegistrationMode::Poll).is_err());
            assert_eq!(handle.token, UNREGISTERED);
            // Retrying even the original mode must acquire a fresh registration.
            if retry {
                handle.rebind_mode(RegistrationMode::Completion).unwrap();
                assert_eq!(handle.token, Token(1));
            }
            drop(handle);
            let AnyDriver::Mock(mock) = driver.as_ref() else {
                unreachable!()
            };
            let expected = if retry {
                vec![Token(0), Token(1)]
            } else {
                vec![Token(0)]
            };
            assert_eq!(
                *mock.registrations.as_ref().unwrap().deregistered.borrow(),
                expected
            );
        }
    }
}
