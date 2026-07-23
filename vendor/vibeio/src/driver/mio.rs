use std::cell::RefCell;
use std::io::{self, ErrorKind};
use std::os::fd::RawFd;
use std::sync::Arc;
use std::task::Waker;
use std::time::Duration;

use mio::{Events, Interest, Poll, Registry, Token, Waker as MioWaker};
use slab::Slab;

use crate::driver::Interruptor;
use crate::{driver::Driver, fd_inner::InnerRawHandle};

// Keep one selector wait within the same one-day bound used by asyncio.
// Darwin's kqueue rejects very large timespec values with EINVAL, while
// asyncio APIs legitimately use infinite delays for cancellable sleeps.
// Re-polling once per day preserves those far-future deadlines without
// passing an invalid timeout to the operating system.
const MAX_POLL_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);

#[inline]
fn bounded_poll_timeout(timeout: Option<Duration>) -> Option<Duration> {
    timeout.map(|timeout| timeout.min(MAX_POLL_TIMEOUT))
}

pub struct MioInterruptor {
    waker: std::sync::Weak<MioWaker>,
}

impl Interruptor for MioInterruptor {
    #[inline]
    fn interrupt(&self) {
        if let Some(waker) = self.waker.upgrade() {
            let _ = waker.wake();
        }
    }
}

struct Registration {
    fd: RawFd,
    waiter: Option<Waker>,
    interest: Interest,
}

struct DriverState {
    registrations: Slab<Registration>,
}

pub struct MioDriver {
    poll: RefCell<Poll>,
    registry: Registry,
    events: RefCell<Events>,
    state: RefCell<DriverState>,
    waker: Arc<MioWaker>,
}

impl MioDriver {
    #[inline]
    pub(crate) fn new() -> Result<Self, io::Error> {
        let poll = Poll::new()?;
        let registry = poll.registry().try_clone()?;
        let waker = MioWaker::new(&registry, Token(usize::MAX))?;

        Ok(Self {
            poll: RefCell::new(poll),
            registry,
            events: RefCell::new(Events::with_capacity(1024)),
            state: RefCell::new(DriverState {
                registrations: Slab::with_capacity(1024),
            }),
            waker: Arc::new(waker),
        })
    }

    #[inline]
    fn update_waiter(waiter_slot: &mut Option<Waker>, waker: Waker) {
        if !waiter_slot
            .as_ref()
            .is_some_and(|waiter| waiter.will_wake(&waker))
        {
            *waiter_slot = Some(waker);
        }
    }

    #[inline]
    pub(crate) fn wait_timeout(&self, timeout: Option<Duration>) {
        let mut poll = self.poll.borrow_mut();
        let mut events = self.events.borrow_mut();
        match poll.poll(&mut events, bounded_poll_timeout(timeout)) {
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {} // strace edge case
            Err(e) => panic!("mio poll failed while waiting for I/O events: {}", e),
        };

        {
            let mut state = self.state.borrow_mut();
            for event in events.iter() {
                // Check if this is an interrupt event
                if event.token().0 == usize::MAX {
                    continue;
                }

                if let Some(registration) = state.registrations.get_mut(event.token().0) {
                    if let Some(task) = registration.waiter.take() {
                        task.wake();
                    }
                }
            }
        }
    }
}

impl Driver for MioDriver {
    type Interruptor = MioInterruptor;

    #[inline]
    fn flush(&self) {
        self.wait_timeout(Some(Duration::ZERO));
    }

    #[inline]
    fn wait(&self, timeout: Option<Duration>) {
        self.wait_timeout(timeout);
    }

    #[inline]
    fn get_interruptor(&self) -> Self::Interruptor {
        MioInterruptor {
            waker: Arc::downgrade(&self.waker),
        }
    }

    #[inline]
    fn register_handle(
        &self,
        handle: &InnerRawHandle,
        interest: Interest,
    ) -> Result<Token, io::Error> {
        let token = {
            let mut state = self.state.borrow_mut();
            let entry = state.registrations.vacant_entry();
            let token = Token(entry.key());
            entry.insert(Registration {
                fd: handle.handle,
                waiter: None,
                interest,
            });
            token
        };

        let mut source = mio::unix::SourceFd(&handle.handle);
        if let Err(err) = self.registry.register(&mut source, token, interest) {
            let mut state = self.state.borrow_mut();
            let _ = state.registrations.try_remove(token.0);
            return Err(err);
        }

        Ok(token)
    }

    #[inline]
    fn reregister_handle(
        &self,
        handle: &InnerRawHandle,
        interest: Interest,
    ) -> Result<(), io::Error> {
        let mut state = self.state.borrow_mut();
        let registration = state.registrations.get_mut(handle.token.0).ok_or_else(|| {
            io::Error::new(
                ErrorKind::NotFound,
                format!(
                    "I/O token {} is not registered with this driver",
                    handle.token.0
                ),
            )
        })?;

        let mut source = mio::unix::SourceFd(&registration.fd);
        self.registry
            .reregister(&mut source, handle.token, interest)?;
        registration.interest = interest;
        Ok(())
    }

    #[inline]
    fn deregister_handle(&self, handle: &InnerRawHandle) -> Result<(), io::Error> {
        let fd = {
            let state = self.state.borrow();
            let registration = state.registrations.get(handle.token.0).ok_or_else(|| {
                io::Error::new(
                    ErrorKind::NotFound,
                    format!(
                        "I/O token {} is not registered with this driver",
                        handle.token.0
                    ),
                )
            })?;
            registration.fd
        };

        let mut source = mio::unix::SourceFd(&fd);
        self.registry.deregister(&mut source)?;

        let mut state = self.state.borrow_mut();
        let _ = state.registrations.try_remove(handle.token.0);
        Ok(())
    }

    #[inline]
    fn submit_poll(
        &self,
        handle: &InnerRawHandle,
        waker: Waker,
        interest: Interest,
    ) -> Result<(), io::Error> {
        let token = handle.token();

        let mut state = self.state.borrow_mut();
        let registration = state.registrations.get_mut(token.0).ok_or_else(|| {
            io::Error::new(
                ErrorKind::NotFound,
                format!("I/O token {} is not registered with this driver", token.0),
            )
        })?;

        if registration.interest != interest {
            // Re-register, but only if the interest has change
            self.registry.reregister(
                &mut mio::unix::SourceFd(&registration.fd),
                token,
                interest,
            )?;
            registration.interest = interest;
        }

        Self::update_waiter(&mut registration.waiter, waker);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    use super::{bounded_poll_timeout, MioDriver, MAX_POLL_TIMEOUT};

    #[test]
    fn poll_timeout_is_bounded_for_platform_selectors() {
        use std::time::Duration;

        assert_eq!(bounded_poll_timeout(None), None);
        assert_eq!(
            bounded_poll_timeout(Some(Duration::from_secs(1))),
            Some(Duration::from_secs(1))
        );
        assert_eq!(
            bounded_poll_timeout(Some(Duration::MAX)),
            Some(MAX_POLL_TIMEOUT)
        );
    }

    struct TestWake {
        count: AtomicUsize,
    }

    impl TestWake {
        #[inline]
        fn new() -> Self {
            Self {
                count: AtomicUsize::new(0),
            }
        }

        #[inline]
        fn wake_count(&self) -> usize {
            self.count.load(Ordering::SeqCst)
        }
    }

    impl std::task::Wake for TestWake {
        #[inline]
        fn wake(self: Arc<Self>) {
            self.count.fetch_add(1, Ordering::SeqCst);
        }

        #[inline]
        fn wake_by_ref(self: &Arc<Self>) {
            self.count.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn wait_wakes_task_for_ready_token() {
        use std::{
            io::Write,
            os::fd::AsRawFd,
            rc::Rc,
            task::{Context, Poll},
            time::Duration,
        };

        use crate::{driver::AnyDriver, fd_inner::InnerRawHandle, op::ReadOp};

        let driver = Rc::new(AnyDriver::Mio(
            MioDriver::new().expect("mio driver should initialize"),
        ));
        let wake = Arc::new(TestWake::new());
        let waker = std::task::Waker::from(wake.clone());

        // Since the driver already has a waker, let's use an Unix pipe instead
        let (side1, mut side2) =
            std::os::unix::net::UnixStream::pair().expect("failed to create pipe");
        let buffer = [0u8; 1];
        side1
            .set_nonblocking(true)
            .expect("failed to set non-blocking");
        let inner_raw_handle = InnerRawHandle::new_with_driver_and_mode(
            &driver,
            side1.as_raw_fd(),
            mio::Interest::READABLE,
            crate::driver::RegistrationMode::Poll,
        )
        .expect("failed to register pipe");
        let mut read_op = ReadOp::new(&inner_raw_handle, buffer);
        match inner_raw_handle.poll_op(&mut Context::from_waker(&waker), &mut read_op) {
            Poll::Pending => {}
            Poll::Ready(Ok(_)) => panic!("unexpected success"),
            Poll::Ready(Err(e)) => panic!("failed to submit operation: {}", e),
        };

        side2.write_all(b"!").expect("failed to write to pipe"); // Exact data written doesn't matter...

        driver.wait(Some(Duration::from_millis(100)));
        assert_eq!(wake.wake_count(), 1);
    }
}
