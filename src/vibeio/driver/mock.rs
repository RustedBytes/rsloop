use std::time::Duration;

use crate::vibeio::driver::{Driver, Interruptor};
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

#[cfg(test)]
type IgnoredCompletion = (usize, Box<dyn std::any::Any>);

pub struct MockDriver {
    #[cfg(test)]
    pub(crate) ignored: std::cell::RefCell<Vec<IgnoredCompletion>>,
    #[cfg(test)]
    pub(crate) registrations: Option<MockRegistrations>,
}

#[cfg(test)]
#[derive(Default)]
pub(crate) struct MockRegistrations {
    pub(crate) results: std::cell::RefCell<std::collections::VecDeque<std::io::Result<Token>>>,
    pub(crate) deregistered: std::cell::RefCell<Vec<Token>>,
}

impl MockDriver {
    #[inline]
    pub(crate) fn new() -> Self {
        MockDriver {
            #[cfg(test)]
            ignored: std::cell::RefCell::new(Vec::new()),
            #[cfg(test)]
            registrations: None,
        }
    }
}

impl Driver for MockDriver {
    type Interruptor = MockInterruptor;

    #[cfg(test)]
    fn supports_completion(&self) -> bool {
        self.registrations.is_some()
    }

    #[cfg(test)]
    fn ignore_completion(&self, token: usize, data: Box<dyn std::any::Any>) {
        self.ignored.borrow_mut().push((token, data));
    }

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
        _handle: &crate::vibeio::fd_inner::InnerRawHandle,
        _interest: Interest,
    ) -> Result<Token, std::io::Error> {
        #[cfg(test)]
        if let Some(registrations) = &self.registrations {
            return registrations
                .results
                .borrow_mut()
                .pop_front()
                .expect("unscripted registration");
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "MockDriver does not support I/O handle registration",
        ))
    }

    #[inline]
    fn reregister_handle(
        &self,
        _handle: &crate::vibeio::fd_inner::InnerRawHandle,
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
        _handle: &crate::vibeio::fd_inner::InnerRawHandle,
    ) -> Result<(), std::io::Error> {
        #[cfg(test)]
        if let Some(registrations) = &self.registrations {
            registrations.deregistered.borrow_mut().push(_handle.token);
            return Ok(());
        }
        Err(std::io::Error::new(
            std::io::ErrorKind::Unsupported,
            "MockDriver does not support I/O handle deregistration",
        ))
    }
}
