use std::cell::RefCell;
use std::io::{self, ErrorKind};
use std::os::fd::RawFd;
use std::sync::Arc;
use std::task::Waker;
use std::time::Duration;

#[cfg(target_vendor = "apple")]
use std::os::fd::AsRawFd;
#[cfg(target_vendor = "apple")]
use std::os::unix::net::UnixDatagram;

#[cfg(not(target_vendor = "apple"))]
use mio::Waker as MioWaker;
use mio::{Events, Interest, Poll, Registry, Token};
use slab::Slab;

use crate::vibeio::driver::Interruptor;
use crate::vibeio::{driver::Driver, fd_inner::InnerRawHandle};

// Keep one selector wait within the same one-day bound used by asyncio.
// Darwin's kqueue rejects very large timespec values with EINVAL, while
// asyncio APIs legitimately use infinite delays for cancellable sleeps.
// Re-polling once per day preserves those far-future deadlines without
// passing an invalid timeout to the operating system.
const MAX_POLL_TIMEOUT: Duration = Duration::from_secs(24 * 60 * 60);
const WAKE_TOKEN: Token = Token(usize::MAX);

#[inline]
fn bounded_poll_timeout(timeout: Option<Duration>) -> Option<Duration> {
    timeout.map(|timeout| timeout.min(MAX_POLL_TIMEOUT))
}

pub struct MioInterruptor {
    waker: std::sync::Weak<DriverWaker>,
}

impl Interruptor for MioInterruptor {
    #[inline]
    fn interrupt(&self) {
        if let Some(waker) = self.waker.upgrade() {
            let _ = waker.wake();
        }
    }
}

#[cfg(not(target_vendor = "apple"))]
struct DriverWaker(MioWaker);

#[cfg(not(target_vendor = "apple"))]
impl DriverWaker {
    fn new(registry: &Registry) -> io::Result<Self> {
        MioWaker::new(registry, WAKE_TOKEN).map(Self)
    }

    #[inline]
    fn wake(&self) -> io::Result<()> {
        self.0.wake()
    }

    #[inline]
    fn acknowledge(&self) {}
}

/// Apple kqueue wake source whose readiness remains observable until the
/// driver drains it. This avoids relying on EVFILT_USER delivery for
/// cross-thread runtime and event-loop notifications.
#[cfg(target_vendor = "apple")]
struct DriverWaker {
    sender: UnixDatagram,
    receiver: UnixDatagram,
}

#[cfg(target_vendor = "apple")]
impl DriverWaker {
    fn new(registry: &Registry) -> io::Result<Self> {
        let (sender, receiver) = UnixDatagram::pair()?;
        sender.set_nonblocking(true)?;
        receiver.set_nonblocking(true)?;
        let receiver_fd = receiver.as_raw_fd();
        registry.register(
            &mut mio::unix::SourceFd(&receiver_fd),
            WAKE_TOKEN,
            Interest::READABLE,
        )?;
        Ok(Self { sender, receiver })
    }

    #[inline]
    fn wake(&self) -> io::Result<()> {
        match self.sender.send(&[1]) {
            Ok(_) => Ok(()),
            // A full socket is already readable and therefore already carries
            // the wake notification we need.
            Err(err) if err.kind() == ErrorKind::WouldBlock => Ok(()),
            Err(err) if err.kind() == ErrorKind::Interrupted => self.wake(),
            Err(err) => Err(err),
        }
    }

    fn acknowledge(&self) {
        let mut buffer = [0_u8; 256];
        loop {
            match self.receiver.recv(&mut buffer) {
                Ok(_) => {}
                Err(err) if err.kind() == ErrorKind::Interrupted => continue,
                Err(err) if err.kind() == ErrorKind::WouldBlock => return,
                Err(_) => return,
            }
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
    ready_wakers: RefCell<Vec<Waker>>,
    state: RefCell<DriverState>,
    waker: Arc<DriverWaker>,
}

impl MioDriver {
    #[inline]
    pub(crate) fn new() -> Result<Self, io::Error> {
        let poll = Poll::new()?;
        let registry = poll.registry().try_clone()?;
        let waker = DriverWaker::new(&registry)?;

        Ok(Self {
            poll: RefCell::new(poll),
            registry,
            events: RefCell::new(Events::with_capacity(1024)),
            ready_wakers: RefCell::new(Vec::with_capacity(64)),
            state: RefCell::new(DriverState {
                registrations: Slab::with_capacity(1024),
            }),
            waker: Arc::new(waker),
        })
    }

    #[inline]
    fn update_waiter(waiter_slot: &mut Option<Waker>, waker: Waker) -> Option<Waker> {
        if !waiter_slot
            .as_ref()
            .is_some_and(|waiter| waiter.will_wake(&waker))
        {
            waiter_slot.replace(waker)
        } else {
            Some(waker)
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

        let mut ready = std::mem::take(&mut *self.ready_wakers.borrow_mut());
        {
            let mut state = self.state.borrow_mut();
            for event in events.iter() {
                // Check if this is an interrupt event
                if event.token() == WAKE_TOKEN {
                    self.waker.acknowledge();
                    continue;
                }

                if let Some(registration) = state.registrations.get_mut(event.token().0) {
                    if let Some(task) = registration.waiter.take() {
                        ready.push(task);
                    }
                }
            }
        }
        drop(events);
        drop(poll);
        for waker in ready.drain(..) {
            waker.wake();
        }
        let mut cache = self.ready_wakers.borrow_mut();
        if ready.capacity() > cache.capacity() {
            *cache = ready;
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
    fn should_flush(&self) -> bool {
        // Registration and re-registration are applied synchronously. Polling
        // with a zero timeout after every task batch only duplicates the wait
        // the executor performs as soon as its ready queue becomes empty.
        false
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

        let registration = self
            .state
            .borrow_mut()
            .registrations
            .try_remove(handle.token.0);
        drop(registration);
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

        let old_waker = Self::update_waiter(&mut registration.waiter, waker);
        drop(state);
        drop(old_waker);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::{MAX_POLL_TIMEOUT, MioDriver, bounded_poll_timeout};
    use crate::vibeio::driver::{Driver, Interruptor};

    thread_local! {
        static REENTER: std::cell::RefCell<Option<Box<dyn Fn()>>> = std::cell::RefCell::new(None);
    }
    struct ReentryScope;
    impl Drop for ReentryScope {
        fn drop(&mut self) {
            let callback = REENTER.with(|slot| slot.borrow_mut().take());
            drop(callback);
        }
    }
    fn reenter() {
        REENTER.with(|slot| {
            if let Some(callback) = slot.borrow().as_ref() {
                callback();
            }
        });
    }
    struct ReentrantWake;
    impl std::task::Wake for ReentrantWake {
        fn wake(self: Arc<Self>) {
            reenter();
        }
    }
    impl Drop for ReentrantWake {
        fn drop(&mut self) {
            reenter();
        }
    }

    #[test]
    fn waiter_callbacks_can_reenter_on_replace_remove_and_readiness() {
        use crate::vibeio::{
            driver::{AnyDriver, RegistrationMode},
            fd_inner::InnerRawHandle,
        };
        use std::{io::Write, os::fd::AsRawFd, rc::Rc, task::Waker, time::Duration};
        for action in 0..3 {
            let driver = Rc::new(AnyDriver::Mio(MioDriver::new().unwrap()));
            let (socket, mut peer) = std::os::unix::net::UnixStream::pair().unwrap();
            socket.set_nonblocking(true).unwrap();
            let handle = InnerRawHandle::new_with_driver_and_mode(
                &driver,
                socket.as_raw_fd(),
                mio::Interest::READABLE,
                RegistrationMode::Poll,
            )
            .unwrap();
            let inner = driver.clone();
            let calls = Rc::new(std::cell::Cell::new(0));
            let called = calls.clone();
            REENTER.with(|slot| {
                *slot.borrow_mut() = Some(Box::new(move || {
                    let AnyDriver::Mio(driver) = inner.as_ref() else {
                        unreachable!()
                    };
                    let _ = driver.state.borrow().registrations.len();
                    driver.wait_timeout(Some(Duration::ZERO));
                    called.set(called.get() + 1);
                }))
            });
            let _scope = ReentryScope;
            let AnyDriver::Mio(mio) = driver.as_ref() else {
                unreachable!()
            };
            mio.submit_poll(
                &handle,
                Waker::from(Arc::new(ReentrantWake)),
                mio::Interest::READABLE,
            )
            .unwrap();
            match action {
                0 => mio
                    .submit_poll(&handle, Waker::noop().clone(), mio::Interest::READABLE)
                    .unwrap(),
                1 => drop(handle),
                _ => {
                    peer.write_all(b"x").unwrap();
                    mio.wait_timeout(Some(Duration::from_secs(1)));
                }
            }
            assert!(calls.get() > 0);
            assert!(mio.ready_wakers.borrow().is_empty());
        }
    }

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

        use crate::vibeio::{driver::AnyDriver, fd_inner::InnerRawHandle, op::ReadOp};

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
            crate::vibeio::driver::RegistrationMode::Poll,
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

    #[test]
    fn repeated_cross_thread_interrupts_wake_driver() {
        use std::sync::mpsc;
        use std::time::Duration;

        let driver = MioDriver::new().expect("mio driver should initialize");
        let interruptor = driver.get_interruptor();
        let (request_tx, request_rx) = mpsc::channel::<()>();
        let worker = std::thread::spawn(move || {
            while request_rx.recv().is_ok() {
                interruptor.interrupt();
            }
        });

        let started = std::time::Instant::now();
        for _ in 0..5_000 {
            request_tx.send(()).expect("interrupt worker stopped");
            driver.wait(Some(Duration::from_millis(100)));
        }

        drop(request_tx);
        worker.join().expect("interrupt worker panicked");
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "cross-thread interrupt stress run took {:?}",
            started.elapsed()
        );
    }
}
