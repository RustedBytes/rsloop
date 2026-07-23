use std::time::Duration;

use crate::driver::{Driver, Interruptor};
use mio::{Interest, Token};

pub struct MockInterruptor {
    thread: std::thread::Thread,
}

impl Interruptor for MockInterruptor {
    #[inline]
    fn interrupt(&self) {
        self.thread.unpark();
    }
}

pub struct MockDriver;

impl MockDriver {
    #[inline]
    pub(crate) fn new() -> Self {
        MockDriver {}
    }
}

impl Driver for MockDriver {
    type Interruptor = MockInterruptor;

    #[inline]
    fn wait(&self, timeout: Option<Duration>) {
        if let Some(timeout) = timeout {
            std::thread::park_timeout(timeout);
        } else {
            std::thread::park();
        }
    }

    #[inline]
    fn get_interruptor(&self) -> Self::Interruptor {
        MockInterruptor {
            thread: std::thread::current(),
        }
    }

    #[inline]
    fn register_handle(
        &self,
        _handle: &crate::fd_inner::InnerRawHandle,
        _interest: Interest,
    ) -> Result<Token, std::io::Error> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "MockDriver does not support I/O handle registration",
        ))
    }

    #[inline]
    fn reregister_handle(
        &self,
        _handle: &crate::fd_inner::InnerRawHandle,
        _interest: Interest,
    ) -> Result<(), std::io::Error> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "MockDriver does not support I/O handle re-registration",
        ))
    }

    #[inline]
    fn deregister_handle(
        &self,
        _handle: &crate::fd_inner::InnerRawHandle,
    ) -> Result<(), std::io::Error> {
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "MockDriver does not support I/O handle deregistration",
        ))
    }
}
