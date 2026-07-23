#[cfg(windows)]
mod iocp;
#[cfg(unix)]
mod mio;
mod mock;
#[cfg(target_os = "linux")]
mod uring;

use std::task::Waker;
use std::{io, time::Duration};

use ::mio::{Interest, Token};

#[cfg(windows)]
use crate::driver::iocp::{IocpDriver, IocpInterruptor};
#[cfg(unix)]
use crate::driver::mio::{MioDriver, MioInterruptor};
use crate::driver::mock::MockInterruptor;
#[cfg(target_os = "linux")]
use crate::driver::uring::{UringDriver, UringInterruptor};
use crate::op::Op;
use crate::{driver::mock::MockDriver, fd_inner::InnerRawHandle};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegistrationMode {
    Poll,
    Completion,
}

#[derive(Debug)]
pub enum CompletionIoResult {
    #[allow(dead_code)]
    Ok(i32),
    Retry(usize), // usize -> token
    SubmitErr(std::io::Error),
}

#[inline]
fn unsupported_completion_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "driver does not support completion-based I/O submission",
    )
}

#[inline]
fn unsupported_poll_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "driver does not support poll-based I/O submission",
    )
}

pub trait Interruptor {
    /// Interrupts a waiting I/O operation.
    fn interrupt(&self);
}

pub enum AnyInterruptor {
    Mock(MockInterruptor),
    #[cfg(windows)]
    Iocp(IocpInterruptor),
    #[cfg(unix)]
    Mio(MioInterruptor),
    #[cfg(target_os = "linux")]
    IoUring(UringInterruptor),
}

impl AnyInterruptor {
    pub(crate) fn interrupt(&self) {
        match self {
            AnyInterruptor::Mock(interruptor) => interruptor.interrupt(),
            #[cfg(windows)]
            AnyInterruptor::Iocp(interruptor) => interruptor.interrupt(),
            #[cfg(unix)]
            AnyInterruptor::Mio(interruptor) => interruptor.interrupt(),
            #[cfg(target_os = "linux")]
            AnyInterruptor::IoUring(interruptor) => interruptor.interrupt(),
        }
    }
}

pub trait Driver {
    type Interruptor: Interruptor;

    /// Flushes the driver's I/O.
    #[inline]
    fn flush(&self) {}

    /// Returns whether the executor should call `flush` after polling a task batch.
    #[inline]
    fn should_flush(&self) -> bool {
        true
    }

    /// Waits for the I/O.
    fn wait(&self, timeout: Option<Duration>);

    /// Registers an I/O source and returns its token.
    fn register_handle(
        &self,
        handle: &InnerRawHandle,
        interest: Interest,
    ) -> Result<Token, std::io::Error>;

    /// Registers an I/O source with the requested mode.
    fn register_handle_with_mode(
        &self,
        handle: &InnerRawHandle,
        interest: Interest,
        _mode: RegistrationMode,
    ) -> Result<Token, io::Error> {
        self.register_handle(handle, interest)
    }

    /// Updates the interest set for a registered I/O source.
    fn reregister_handle(
        &self,
        handle: &InnerRawHandle,
        interest: Interest,
    ) -> Result<(), std::io::Error>;

    /// Removes an I/O source from the poller.
    fn deregister_handle(&self, handle: &InnerRawHandle) -> Result<(), std::io::Error>;

    /// Returns whether the driver supports completion-based I/O operations.
    #[inline]
    fn supports_completion(&self) -> bool {
        false
    }

    /// Submits a completion-based I/O operation.
    #[inline]
    fn submit_completion<O>(&self, _op: &mut O, _waker: Waker) -> CompletionIoResult
    where
        O: Op,
    {
        CompletionIoResult::SubmitErr(unsupported_completion_error())
    }

    /// Re-registers interest and submits a waker for poll-based I/O.
    #[inline]
    fn submit_poll(
        &self,
        _handle: &InnerRawHandle,
        _waker: Waker,
        _interest: Interest,
    ) -> Result<(), io::Error> {
        Err(unsupported_poll_error())
    }

    /// Obtains the result for a completion-based I/O operation.
    #[inline]
    fn get_completion_result(&self, _token: usize) -> Option<i32> {
        None
    }

    /// Sets the waker for a completion-based I/O operation.
    #[inline]
    fn set_completion_waker(&self, _token: usize, _waker: Waker) {}

    /// Cancels a completion-based I/O operation.
    #[inline]
    fn ignore_completion(&self, _token: usize, _data: Box<dyn std::any::Any>) {}

    /// Cancels a Windows completion operation while retaining its owned data
    /// until the completion packet is observed.
    #[cfg(windows)]
    #[inline]
    fn cancel_completion(
        &self,
        token: usize,
        _handle: crate::fd_inner::RawOsHandle,
        data: Box<dyn std::any::Any>,
    ) {
        self.ignore_completion(token, data);
    }

    /// Interrupts a waiting I/O operation.
    fn get_interruptor(&self) -> Self::Interruptor;
}

#[allow(clippy::large_enum_variant)]
pub enum AnyDriver {
    Mock(MockDriver),
    #[cfg(windows)]
    Iocp(IocpDriver),
    #[cfg(unix)]
    Mio(MioDriver),
    #[cfg(target_os = "linux")]
    IoUring(UringDriver),
}

impl AnyDriver {
    #[cfg(unix)]
    #[inline]
    pub(crate) fn new_mio() -> Result<Self, std::io::Error> {
        Ok(AnyDriver::Mio(MioDriver::new()?))
    }

    #[inline]
    pub(crate) fn new_mock() -> Self {
        AnyDriver::Mock(MockDriver::new())
    }

    #[cfg(windows)]
    #[inline]
    pub(crate) fn new_iocp() -> Result<Self, io::Error> {
        Ok(AnyDriver::Iocp(IocpDriver::new()?))
    }

    #[cfg(target_os = "linux")]
    #[inline]
    pub(crate) fn new_uring_custom(builder: io_uring::Builder) -> Result<Self, io::Error> {
        Ok(AnyDriver::IoUring(UringDriver::new(1024, builder)?))
    }

    #[cfg(target_os = "linux")]
    #[inline]
    pub(crate) fn new_uring() -> Result<Self, io::Error> {
        let mut builder = io_uring::IoUring::builder();
        builder
            .setup_single_issuer()
            .setup_coop_taskrun()
            .setup_taskrun_flag()
            .setup_submit_all();
        if let Ok(driver) = Self::new_uring_custom(builder) {
            return Ok(driver);
        }

        // Fallback for older kernels
        Self::new_uring_custom(io_uring::IoUring::builder())
    }

    #[inline]
    pub(crate) fn new_best() -> Result<Self, io::Error> {
        #[cfg(target_os = "linux")]
        if let Ok(driver) = Self::new_uring() {
            return Ok(driver);
        }

        #[cfg(unix)]
        let driver = Self::new_mio()?;
        #[cfg(windows)]
        let driver = Self::new_iocp()?;

        Ok(driver)
    }

    #[inline]
    pub(crate) fn flush(&self) {
        match self {
            #[cfg(windows)]
            AnyDriver::Iocp(driver) => driver.flush(),
            #[cfg(unix)]
            AnyDriver::Mio(driver) => driver.flush(),
            AnyDriver::Mock(driver) => driver.flush(),
            #[cfg(target_os = "linux")]
            AnyDriver::IoUring(driver) => driver.flush(),
        }
    }

    #[inline]
    pub(crate) fn should_flush(&self) -> bool {
        match self {
            #[cfg(windows)]
            AnyDriver::Iocp(driver) => driver.should_flush(),
            #[cfg(unix)]
            AnyDriver::Mio(driver) => driver.should_flush(),
            AnyDriver::Mock(driver) => driver.should_flush(),
            #[cfg(target_os = "linux")]
            AnyDriver::IoUring(driver) => driver.should_flush(),
        }
    }

    #[inline]
    pub(crate) fn wait(&self, timeout: Option<Duration>) {
        match self {
            #[cfg(windows)]
            AnyDriver::Iocp(driver) => driver.wait(timeout),
            #[cfg(unix)]
            AnyDriver::Mio(driver) => driver.wait(timeout),
            AnyDriver::Mock(driver) => driver.wait(timeout),
            #[cfg(target_os = "linux")]
            AnyDriver::IoUring(driver) => driver.wait(timeout),
        }
    }

    #[allow(dead_code)]
    #[inline]
    pub(crate) fn register_handle(
        &self,
        handle: &InnerRawHandle,
        interest: Interest,
    ) -> Result<Token, std::io::Error> {
        match self {
            #[cfg(windows)]
            AnyDriver::Iocp(driver) => driver.register_handle(handle, interest),
            #[cfg(unix)]
            AnyDriver::Mio(driver) => driver.register_handle(handle, interest),
            AnyDriver::Mock(driver) => driver.register_handle(handle, interest),
            #[cfg(target_os = "linux")]
            AnyDriver::IoUring(driver) => driver.register_handle(handle, interest),
        }
    }

    #[inline]
    pub(crate) fn register_handle_with_mode(
        &self,
        handle: &InnerRawHandle,
        interest: Interest,
        mode: RegistrationMode,
    ) -> Result<Token, io::Error> {
        match self {
            #[cfg(windows)]
            AnyDriver::Iocp(driver) => driver.register_handle_with_mode(handle, interest, mode),
            #[cfg(unix)]
            AnyDriver::Mio(driver) => driver.register_handle_with_mode(handle, interest, mode),
            AnyDriver::Mock(driver) => driver.register_handle_with_mode(handle, interest, mode),
            #[cfg(target_os = "linux")]
            AnyDriver::IoUring(driver) => driver.register_handle_with_mode(handle, interest, mode),
        }
    }

    #[inline]
    pub(crate) fn reregister_handle(
        &self,
        handle: &InnerRawHandle,
        interest: Interest,
    ) -> Result<(), std::io::Error> {
        match self {
            #[cfg(windows)]
            AnyDriver::Iocp(driver) => driver.reregister_handle(handle, interest),
            #[cfg(unix)]
            AnyDriver::Mio(driver) => driver.reregister_handle(handle, interest),
            AnyDriver::Mock(driver) => driver.reregister_handle(handle, interest),
            #[cfg(target_os = "linux")]
            AnyDriver::IoUring(driver) => driver.reregister_handle(handle, interest),
        }
    }

    #[inline]
    pub(crate) fn deregister_handle(&self, handle: &InnerRawHandle) -> Result<(), std::io::Error> {
        match self {
            #[cfg(windows)]
            AnyDriver::Iocp(driver) => driver.deregister_handle(handle),
            #[cfg(unix)]
            AnyDriver::Mio(driver) => driver.deregister_handle(handle),
            AnyDriver::Mock(driver) => driver.deregister_handle(handle),
            #[cfg(target_os = "linux")]
            AnyDriver::IoUring(driver) => driver.deregister_handle(handle),
        }
    }

    #[inline]
    pub(crate) fn supports_completion(&self) -> bool {
        match self {
            #[cfg(windows)]
            AnyDriver::Iocp(driver) => driver.supports_completion(),
            #[cfg(unix)]
            AnyDriver::Mio(driver) => driver.supports_completion(),
            AnyDriver::Mock(driver) => driver.supports_completion(),
            #[cfg(target_os = "linux")]
            AnyDriver::IoUring(driver) => driver.supports_completion(),
        }
    }

    #[inline]
    pub(crate) fn submit_completion<O>(&self, op: &mut O, waker: Waker) -> CompletionIoResult
    where
        O: Op,
    {
        match self {
            #[cfg(windows)]
            AnyDriver::Iocp(driver) => driver.submit_completion(op, waker),
            #[cfg(unix)]
            AnyDriver::Mio(driver) => driver.submit_completion(op, waker),
            AnyDriver::Mock(driver) => driver.submit_completion(op, waker),
            #[cfg(target_os = "linux")]
            AnyDriver::IoUring(driver) => driver.submit_completion(op, waker),
        }
    }

    #[inline]
    pub(crate) fn submit_poll(
        &self,
        handle: &InnerRawHandle,
        waker: Waker,
        interest: Interest,
    ) -> Result<(), io::Error> {
        match self {
            #[cfg(windows)]
            AnyDriver::Iocp(driver) => driver.submit_poll(handle, waker, interest),
            #[cfg(unix)]
            AnyDriver::Mio(driver) => driver.submit_poll(handle, waker, interest),
            AnyDriver::Mock(driver) => driver.submit_poll(handle, waker, interest),
            #[cfg(target_os = "linux")]
            AnyDriver::IoUring(driver) => driver.submit_poll(handle, waker, interest),
        }
    }

    #[inline]
    pub(crate) fn get_completion_result(&self, token: usize) -> Option<i32> {
        match self {
            #[cfg(windows)]
            AnyDriver::Iocp(driver) => driver.get_completion_result(token),
            #[cfg(unix)]
            AnyDriver::Mio(driver) => driver.get_completion_result(token),
            AnyDriver::Mock(driver) => driver.get_completion_result(token),
            #[cfg(target_os = "linux")]
            AnyDriver::IoUring(driver) => driver.get_completion_result(token),
        }
    }

    #[inline]
    pub(crate) fn set_completion_waker(&self, token: usize, waker: Waker) {
        match self {
            #[cfg(windows)]
            AnyDriver::Iocp(driver) => driver.set_completion_waker(token, waker),
            #[cfg(unix)]
            AnyDriver::Mio(driver) => driver.set_completion_waker(token, waker),
            AnyDriver::Mock(driver) => driver.set_completion_waker(token, waker),
            #[cfg(target_os = "linux")]
            AnyDriver::IoUring(driver) => driver.set_completion_waker(token, waker),
        }
    }

    #[inline]
    pub(crate) fn ignore_completion(&self, token: usize, data: Box<dyn std::any::Any>) {
        match self {
            #[cfg(windows)]
            AnyDriver::Iocp(driver) => driver.ignore_completion(token, data),
            #[cfg(unix)]
            AnyDriver::Mio(driver) => driver.ignore_completion(token, data),
            AnyDriver::Mock(driver) => driver.ignore_completion(token, data),
            #[cfg(target_os = "linux")]
            AnyDriver::IoUring(driver) => driver.ignore_completion(token, data),
        }
    }

    #[cfg(windows)]
    #[inline]
    pub(crate) fn cancel_completion(
        &self,
        token: usize,
        handle: crate::fd_inner::RawOsHandle,
        data: Box<dyn std::any::Any>,
    ) {
        match self {
            AnyDriver::Iocp(driver) => driver.cancel_completion(token, handle, data),
            AnyDriver::Mock(driver) => driver.ignore_completion(token, data),
        }
    }

    #[inline]
    pub(crate) fn get_interruptor(&self) -> AnyInterruptor {
        match self {
            #[cfg(windows)]
            AnyDriver::Iocp(driver) => AnyInterruptor::Iocp(driver.get_interruptor()),
            #[cfg(unix)]
            AnyDriver::Mio(driver) => AnyInterruptor::Mio(driver.get_interruptor()),
            AnyDriver::Mock(driver) => AnyInterruptor::Mock(driver.get_interruptor()),
            #[cfg(target_os = "linux")]
            AnyDriver::IoUring(driver) => AnyInterruptor::IoUring(driver.get_interruptor()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::AnyDriver;
    use std::{
        future::poll_fn,
        task::{Poll, Waker},
    };

    #[cfg(unix)]
    #[test]
    fn test_mio_driver_interrupt_basic() {
        let driver = AnyDriver::new_mio().expect("Failed to create MioDriver");
        let interruptor = driver.get_interruptor();

        // Test that interrupt doesn't panic and can be called multiple times
        interruptor.interrupt();
        interruptor.interrupt();
        interruptor.interrupt();
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_uring_driver_interrupt_basic() {
        let driver = AnyDriver::new_uring().expect("Failed to create UringDriver");
        let interruptor = driver.get_interruptor();

        // Test that interrupt doesn't panic and can be called multiple times
        interruptor.interrupt();
        interruptor.interrupt();
        interruptor.interrupt();
    }

    #[test]
    fn test_mock_driver_interrupt_basic() {
        let driver = AnyDriver::new_mock();
        let interruptor = driver.get_interruptor();

        // Test that interrupt doesn't panic and can be called multiple times
        interruptor.interrupt();
        interruptor.interrupt();
        interruptor.interrupt();
    }

    #[cfg(windows)]
    #[test]
    fn test_iocp_driver_interrupt_basic() {
        let driver = AnyDriver::new_iocp().expect("Failed to create IocpDriver");
        let interruptor = driver.get_interruptor();

        // Test that interrupt doesn't panic and can be called multiple times
        interruptor.interrupt();
        interruptor.interrupt();
        interruptor.interrupt();
    }

    #[cfg(unix)]
    #[test]
    fn test_interrupt_mio() {
        let runtime = crate::executor::Runtime::new(
            AnyDriver::new_mio().expect("Failed to create MioDriver"),
        );

        let (tx, rx) = std::sync::mpsc::channel();

        std::thread::spawn(move || {
            let waker: Waker = rx.recv().unwrap();
            drop(rx); // Drop the receiver before waking the task
            waker.wake();
        });

        runtime.block_on(poll_fn(move |cx| {
            if tx.send(cx.waker().clone()).is_ok() {
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        }));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_interruptor_uring() {
        let runtime = crate::executor::Runtime::new(
            AnyDriver::new_uring().expect("Failed to create UringDriver"),
        );

        let (tx, rx) = std::sync::mpsc::channel();

        std::thread::spawn(move || {
            let waker: Waker = rx.recv().unwrap();
            drop(rx); // Drop the receiver before waking the task
            waker.wake();
        });

        runtime.block_on(poll_fn(move |cx| {
            if tx.send(cx.waker().clone()).is_ok() {
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        }));
    }

    #[cfg(windows)]
    #[test]
    fn test_interrupt_iocp() {
        let runtime = crate::executor::Runtime::new(
            AnyDriver::new_iocp().expect("Failed to create IocpDriver"),
        );

        let (tx, rx) = std::sync::mpsc::channel();

        std::thread::spawn(move || {
            let waker: Waker = rx.recv().unwrap();
            drop(rx); // Drop the receiver before waking the task
            waker.wake();
        });

        runtime.block_on(poll_fn(move |cx| {
            if tx.send(cx.waker().clone()).is_ok() {
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        }));
    }
}
