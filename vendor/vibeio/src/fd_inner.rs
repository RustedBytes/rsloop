use std::rc::Rc;
use std::task::Poll;
use std::{io, task::Context};

use mio::{Interest, Token};

use crate::{
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
    Handle(std::os::windows::io::RawHandle),
}

pub struct InnerRawHandle {
    pub(crate) handle: RawOsHandle,
    pub(crate) token: Token,
    interest: Interest,
    mode: RegistrationMode,
    driver: Rc<AnyDriver>,
}

impl InnerRawHandle {
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
            token: Token(0),
            interest,
            mode,
            driver: driver.clone(),
        };

        inner.token = driver.register_handle_with_mode(&inner, interest, mode)?;
        Ok(inner)
    }

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

        if self.mode == mode {
            return Ok(());
        }

        self.driver.deregister_handle(self)?;
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
        let _ = self.driver.deregister_handle(self);
    }
}
