#![warn(clippy::undocumented_unsafe_blocks)]

use std::cell::RefCell;
use std::io::{self, ErrorKind};
use std::mem::MaybeUninit;
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd, RawFd};
use std::os::unix::net::UnixDatagram;
use std::sync::Arc;
use std::task::Waker;
use std::time::Duration;

use mio::{Interest, Token};
use slab::Slab;

use crate::vibeio::driver::{Driver, Interruptor};
use crate::vibeio::fd_inner::InnerRawHandle;

const EVENT_CAPACITY: usize = 1024;
const WAKE_KEY: usize = usize::MAX;
const MAX_WAIT: Duration = Duration::from_secs(24 * 60 * 60);

pub struct KqueueInterruptor {
    waker: std::sync::Weak<DriverWaker>,
}

impl Interruptor for KqueueInterruptor {
    #[inline]
    fn interrupt(&self) {
        if let Some(waker) = self.waker.upgrade() {
            let _ = waker.wake();
        }
    }
}

struct DriverWaker {
    sender: UnixDatagram,
    receiver: UnixDatagram,
}

impl DriverWaker {
    fn new() -> io::Result<Self> {
        let (sender, receiver) = UnixDatagram::pair()?;
        sender.set_nonblocking(true)?;
        receiver.set_nonblocking(true)?;
        Ok(Self { sender, receiver })
    }

    #[inline]
    fn wake(&self) -> io::Result<()> {
        super::send_wake_datagram(|| self.sender.send(&[1]))
    }

    fn acknowledge(&self) {
        let mut buffer = [0_u8; 256];
        loop {
            match self.receiver.recv(&mut buffer) {
                Ok(_) => {}
                Err(err) if err.kind() == ErrorKind::Interrupted => continue,
                Err(_) => return,
            }
        }
    }
}

struct Registration {
    fd: RawFd,
    read_waiter: Option<Waker>,
    write_waiter: Option<Waker>,
    read_ready: bool,
    write_ready: bool,
    registered_read: bool,
    registered_write: bool,
    generation: u32,
}

struct DriverState {
    registrations: Slab<Registration>,
    next_generation: u32,
}

pub struct KqueueDriver {
    // Close the queue before dropping registrations and their user wakers.
    kqueue: OwnedFd,
    state: RefCell<DriverState>,
    waker: Arc<DriverWaker>,
    ready_wakers: RefCell<Vec<Waker>>,
}

impl KqueueDriver {
    pub(crate) fn new() -> io::Result<Self> {
        // SAFETY: kqueue takes no pointers and returns a newly owned descriptor.
        let kqueue = unsafe { libc::kqueue() };
        if kqueue < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: the successful syscall transferred this valid descriptor;
        // no other owner exists. OwnedFd also closes it on later setup failures.
        let kqueue = unsafe { OwnedFd::from_raw_fd(kqueue) };
        let waker = Arc::new(DriverWaker::new()?);
        let driver = Self {
            kqueue,
            state: RefCell::new(DriverState {
                registrations: Slab::with_capacity(1024),
                next_generation: 0,
            }),
            waker,
            ready_wakers: RefCell::new(Vec::with_capacity(64)),
        };
        driver.apply_change(Self::change(
            driver.waker.receiver.as_raw_fd(),
            libc::EVFILT_READ,
            libc::EV_ADD | libc::EV_ENABLE | libc::EV_CLEAR,
            WAKE_KEY,
        ))?;
        Ok(driver)
    }

    #[inline]
    fn change(fd: RawFd, filter: i16, flags: u16, key: usize) -> libc::kevent {
        libc::kevent {
            ident: fd as usize,
            filter,
            flags,
            fflags: 0,
            data: 0,
            udata: key as *mut libc::c_void,
        }
    }

    #[inline]
    fn encode_key(token: Token, generation: u32) -> usize {
        ((generation as usize) << 32) | (token.0 & u32::MAX as usize)
    }

    #[inline]
    fn decode_key(key: usize) -> (Token, u32) {
        (Token(key & u32::MAX as usize), (key >> 32) as u32)
    }

    fn apply_change(&self, change: libc::kevent) -> io::Result<()> {
        self.apply_changes(std::slice::from_ref(&change))
    }

    fn apply_changes(&self, changes: &[libc::kevent]) -> io::Result<()> {
        // SAFETY: changes is live input storage for this synchronous call.
        // Internal callers supply at most two entries; there is no output array.
        let result = unsafe {
            libc::kevent(
                self.kqueue.as_raw_fd(),
                changes.as_ptr(),
                changes.len() as i32,
                std::ptr::null_mut(),
                0,
                std::ptr::null(),
            )
        };
        if result < 0 {
            Err(io::Error::last_os_error())
        } else {
            Ok(())
        }
    }

    fn delete_filter(&self, fd: RawFd, filter: i16) -> io::Result<()> {
        match self.apply_change(Self::change(fd, filter, libc::EV_DELETE, 0)) {
            Err(err)
                if matches!(
                    err.raw_os_error(),
                    Some(libc::ENOENT) | Some(libc::EBADF) | Some(libc::EINVAL)
                ) =>
            {
                Ok(())
            }
            result => result,
        }
    }

    fn install_registration_with(
        &self,
        changes: &[libc::kevent],
        apply: impl FnOnce(&[libc::kevent]) -> io::Result<()>,
    ) -> io::Result<()> {
        if let Err(error) = apply(changes) {
            // A failed changelist may already have installed a prefix (or all
            // filters on interruption). Roll back every requested filter before
            // the caller discards the registration token. Missing filters are OK.
            let mut cleanup_error = None;
            for change in changes {
                if let Err(err) = self.delete_filter(change.ident as RawFd, change.filter) {
                    cleanup_error.get_or_insert(err);
                }
            }
            return match cleanup_error {
                None => Err(error),
                Some(cleanup) => Err(io::Error::new(
                    error.kind(),
                    format!(
                        "kqueue registration failed: {error}; filter cleanup failed: {cleanup}"
                    ),
                )),
            };
        }
        Ok(())
    }

    fn wait_events(&self, timeout: Option<Duration>) -> io::Result<()> {
        let timeout = timeout.map(|duration| duration.min(MAX_WAIT));
        let timespec = timeout.map(|duration| libc::timespec {
            tv_sec: duration.as_secs().try_into().unwrap_or(i64::MAX),
            tv_nsec: duration.subsec_nanos().into(),
        });
        let timeout_ptr = timespec
            .as_ref()
            .map_or(std::ptr::null(), |value| value as *const libc::timespec);
        let mut events: [MaybeUninit<libc::kevent>; EVENT_CAPACITY] =
            [const { MaybeUninit::uninit() }; EVENT_CAPACITY];

        // SAFETY: events provides EVENT_CAPACITY writable kevent slots and the
        // optional timespec remains live. No changelist is supplied or retained.
        let count = unsafe {
            libc::kevent(
                self.kqueue.as_raw_fd(),
                std::ptr::null(),
                0,
                events.as_mut_ptr().cast(),
                events.len() as i32,
                timeout_ptr,
            )
        };
        if count < 0 {
            let err = io::Error::last_os_error();
            return if err.kind() == ErrorKind::Interrupted {
                Ok(())
            } else {
                Err(err)
            };
        }

        let mut wakers = std::mem::take(&mut *self.ready_wakers.borrow_mut());
        let mut state = self.state.borrow_mut();
        for event in &events[..count as usize] {
            // SAFETY: successful kevent initialized exactly the returned prefix,
            // bounded by the output capacity passed above.
            let event = unsafe { event.assume_init_ref() };
            let key = event.udata as usize;
            if key == WAKE_KEY {
                self.waker.acknowledge();
                continue;
            }
            let (token, generation) = Self::decode_key(key);
            let Some(registration) = state.registrations.get_mut(token.0) else {
                continue;
            };
            if registration.generation != generation {
                continue;
            }
            let (waiter, ready) = if event.filter == libc::EVFILT_READ {
                (&mut registration.read_waiter, &mut registration.read_ready)
            } else if event.filter == libc::EVFILT_WRITE {
                (
                    &mut registration.write_waiter,
                    &mut registration.write_ready,
                )
            } else {
                continue;
            };
            if let Some(waker) = waiter.take() {
                wakers.push(waker);
            } else {
                *ready = true;
            }
        }
        drop(state);
        for waker in wakers.drain(..) {
            waker.wake();
        }
        let mut cache = self.ready_wakers.borrow_mut();
        if wakers.capacity() > cache.capacity() {
            *cache = wakers;
        }
        Ok(())
    }

    #[inline]
    fn filter(interest: Interest) -> i16 {
        if interest.is_readable() {
            libc::EVFILT_READ
        } else {
            libc::EVFILT_WRITE
        }
    }

    fn deregister_with(
        &self,
        handle: &InnerRawHandle,
        mut delete: impl FnMut(RawFd, i16) -> io::Result<()>,
    ) -> io::Result<()> {
        let registration = self
            .state
            .borrow_mut()
            .registrations
            .try_remove(handle.token.0)
            .ok_or_else(|| {
                io::Error::new(
                    ErrorKind::NotFound,
                    format!("I/O token {} is not registered", handle.token.0),
                )
            })?;
        // The token is retired regardless of kernel errors. Attempt both
        // deletions, preserving the first error rather than abandoning the
        // second filter. Retired wakers drop outside the state borrow.
        let mut error = None;
        for (filter, installed) in [
            (libc::EVFILT_READ, registration.registered_read),
            (libc::EVFILT_WRITE, registration.registered_write),
        ] {
            if installed {
                if let Err(err) = delete(registration.fd, filter) {
                    error.get_or_insert(err);
                }
            }
        }
        error.map_or(Ok(()), Err)
    }

    fn reregister_with(
        &self,
        handle: &InnerRawHandle,
        interest: Interest,
        mut change: impl FnMut(RawFd, i16, bool, usize) -> io::Result<()>,
    ) -> io::Result<()> {
        // Keep retired wakers outside the state borrow, including on errors.
        let mut retired = [None, None];
        let mut state = self.state.borrow_mut();
        let registration = state.registrations.get_mut(handle.token.0).ok_or_else(|| {
            io::Error::new(
                ErrorKind::NotFound,
                format!("I/O token {} is not registered", handle.token.0),
            )
        })?;
        let key = Self::encode_key(handle.token, registration.generation);
        // Install new filters first, so a failed addition cannot remove an
        // existing waiter. Record each successful syscall individually: kqueue
        // changes are not atomic, and a later failure must remain retryable.
        for (filter, wanted, installed) in [
            (
                libc::EVFILT_READ,
                interest.is_readable(),
                &mut registration.registered_read,
            ),
            (
                libc::EVFILT_WRITE,
                interest.is_writable(),
                &mut registration.registered_write,
            ),
        ] {
            if wanted && !*installed {
                change(registration.fd, filter, true, key)?;
                *installed = true;
            }
        }
        for (index, (filter, wanted, installed, waiter, ready)) in [
            (
                libc::EVFILT_READ,
                interest.is_readable(),
                &mut registration.registered_read,
                &mut registration.read_waiter,
                &mut registration.read_ready,
            ),
            (
                libc::EVFILT_WRITE,
                interest.is_writable(),
                &mut registration.registered_write,
                &mut registration.write_waiter,
                &mut registration.write_ready,
            ),
        ]
        .into_iter()
        .enumerate()
        {
            if !wanted && *installed {
                change(registration.fd, filter, false, key)?;
                *installed = false;
                retired[index] = waiter.take();
                *ready = false;
            }
        }
        drop(state);
        drop(retired);
        Ok(())
    }
}

impl Driver for KqueueDriver {
    type Interruptor = KqueueInterruptor;

    #[inline]
    fn should_flush(&self) -> bool {
        false
    }

    #[inline]
    fn wait(&self, timeout: Option<Duration>) {
        if let Err(err) = self.wait_events(timeout) {
            panic!("kqueue wait failed: {err}");
        }
    }

    fn register_handle(&self, handle: &InnerRawHandle, interest: Interest) -> io::Result<Token> {
        let (token, generation) = {
            let mut state = self.state.borrow_mut();
            state.next_generation = state.next_generation.wrapping_add(1);
            if state.next_generation == 0 {
                state.next_generation = 1;
            }
            let generation = state.next_generation;
            let entry = state.registrations.vacant_entry();
            let token = Token(entry.key());
            entry.insert(Registration {
                fd: handle.handle,
                read_waiter: None,
                write_waiter: None,
                read_ready: false,
                write_ready: false,
                registered_read: interest.is_readable(),
                registered_write: interest.is_writable(),
                generation,
            });
            (token, generation)
        };
        let key = Self::encode_key(token, generation);
        let mut changes = Vec::with_capacity(2);
        if interest.is_readable() {
            changes.push(Self::change(
                handle.handle,
                libc::EVFILT_READ,
                libc::EV_ADD | libc::EV_ENABLE | libc::EV_CLEAR,
                key,
            ));
        }
        if interest.is_writable() {
            changes.push(Self::change(
                handle.handle,
                libc::EVFILT_WRITE,
                libc::EV_ADD | libc::EV_ENABLE | libc::EV_CLEAR,
                key,
            ));
        }
        if let Err(err) =
            self.install_registration_with(&changes, |changes| self.apply_changes(changes))
        {
            let _ = self.state.borrow_mut().registrations.try_remove(token.0);
            return Err(err);
        }
        Ok(token)
    }

    fn reregister_handle(&self, handle: &InnerRawHandle, interest: Interest) -> io::Result<()> {
        self.reregister_with(handle, interest, |fd, filter, add, key| {
            if add {
                self.apply_change(Self::change(
                    fd,
                    filter,
                    libc::EV_ADD | libc::EV_ENABLE | libc::EV_CLEAR,
                    key,
                ))
            } else {
                self.delete_filter(fd, filter)
            }
        })
    }

    fn deregister_handle(&self, handle: &InnerRawHandle) -> io::Result<()> {
        self.deregister_with(handle, |fd, filter| self.delete_filter(fd, filter))
    }

    fn submit_poll(
        &self,
        handle: &InnerRawHandle,
        waker: Waker,
        interest: Interest,
    ) -> io::Result<()> {
        let filter = Self::filter(interest);
        let mut incoming = Some(waker);
        let mut replaced = None;
        let wake_now = {
            let mut state = self.state.borrow_mut();
            let registration = state.registrations.get_mut(handle.token.0).ok_or_else(|| {
                io::Error::new(
                    ErrorKind::NotFound,
                    format!("I/O token {} is not registered", handle.token.0),
                )
            })?;
            let (waiter, ready) = if filter == libc::EVFILT_READ {
                (&mut registration.read_waiter, &mut registration.read_ready)
            } else {
                (
                    &mut registration.write_waiter,
                    &mut registration.write_ready,
                )
            };
            if *ready {
                *ready = false;
                true
            } else {
                if !waiter
                    .as_ref()
                    .is_some_and(|current| current.will_wake(incoming.as_ref().unwrap()))
                {
                    replaced = std::mem::replace(waiter, incoming.take());
                }
                false
            }
        };
        drop(replaced);
        if wake_now {
            incoming.take().unwrap().wake();
        }
        Ok(())
    }

    #[inline]
    fn get_interruptor(&self) -> Self::Interruptor {
        KqueueInterruptor {
            waker: Arc::downgrade(&self.waker),
        }
    }
}

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::os::fd::AsRawFd;
    use std::rc::Rc;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use super::*;
    use crate::vibeio::driver::{AnyDriver, RegistrationMode};

    thread_local! {
        static REENTER: RefCell<Option<Box<dyn Fn()>>> = RefCell::new(None);
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
    fn callbacks_can_reenter_on_replacement_interest_removal_and_readiness() {
        for action in 0..3 {
            let driver = Rc::new(AnyDriver::Kqueue(KqueueDriver::new().unwrap()));
            let (reader, mut writer) = std::os::unix::net::UnixStream::pair().unwrap();
            reader.set_nonblocking(true).unwrap();
            let handle = InnerRawHandle::new_with_driver_and_mode(
                &driver,
                reader.as_raw_fd(),
                Interest::READABLE,
                RegistrationMode::Poll,
            )
            .unwrap();
            let inner = driver.clone();
            let calls = Rc::new(std::cell::Cell::new(0));
            let called = calls.clone();
            REENTER.with(|slot| {
                *slot.borrow_mut() = Some(Box::new(move || {
                    let AnyDriver::Kqueue(driver) = inner.as_ref() else {
                        unreachable!()
                    };
                    assert!(driver.state.try_borrow_mut().is_ok());
                    driver.wait_events(Some(Duration::ZERO)).unwrap();
                    called.set(called.get() + 1);
                }))
            });
            let _scope = ReentryScope;
            let AnyDriver::Kqueue(kqueue) = driver.as_ref() else {
                unreachable!()
            };
            kqueue
                .submit_poll(
                    &handle,
                    Waker::from(Arc::new(ReentrantWake)),
                    Interest::READABLE,
                )
                .unwrap();
            match action {
                0 => kqueue
                    .submit_poll(&handle, Waker::noop().clone(), Interest::READABLE)
                    .unwrap(),
                1 => kqueue
                    .reregister_handle(&handle, Interest::WRITABLE)
                    .unwrap(),
                _ => {
                    writer.write_all(b"x").unwrap();
                    kqueue.wait_events(Some(Duration::from_secs(1))).unwrap();
                }
            }
            assert!(calls.get() > 0);
            assert!(kqueue.ready_wakers.borrow().is_empty());
        }
    }

    struct WakeCount(AtomicUsize);

    #[test]
    fn deregistration_attempts_both_filters_and_preserves_first_error() {
        for (fail_read, fail_write) in [(true, false), (false, true), (true, true)] {
            let driver = Rc::new(AnyDriver::Kqueue(KqueueDriver::new().unwrap()));
            let (reader, _writer) = std::os::unix::net::UnixStream::pair().unwrap();
            let handle = InnerRawHandle::new_with_driver_and_mode(
                &driver,
                reader.as_raw_fd(),
                Interest::READABLE | Interest::WRITABLE,
                RegistrationMode::Poll,
            )
            .unwrap();
            let AnyDriver::Kqueue(kqueue) = driver.as_ref() else {
                unreachable!()
            };
            let mut attempts = Vec::new();
            let error = kqueue
                .deregister_with(&handle, |fd, filter| {
                    assert!(kqueue.state.try_borrow_mut().is_ok());
                    attempts.push(filter);
                    match filter {
                        libc::EVFILT_READ if fail_read => {
                            Err(io::Error::from_raw_os_error(libc::EIO))
                        }
                        libc::EVFILT_WRITE if fail_write => {
                            Err(io::Error::from_raw_os_error(libc::ENOMEM))
                        }
                        _ => kqueue.delete_filter(fd, filter),
                    }
                })
                .unwrap_err();
            assert_eq!(attempts, [libc::EVFILT_READ, libc::EVFILT_WRITE]);
            assert_eq!(
                error.raw_os_error(),
                Some(if fail_read { libc::EIO } else { libc::ENOMEM })
            );
            assert!(!kqueue.state.borrow().registrations.contains(handle.token.0));
            for (filter, failed) in [
                (libc::EVFILT_READ, fail_read),
                (libc::EVFILT_WRITE, fail_write),
            ] {
                let result = kqueue.apply_change(KqueueDriver::change(
                    reader.as_raw_fd(),
                    filter,
                    libc::EV_DELETE,
                    0,
                ));
                if failed {
                    result.unwrap(); // Release the deliberately retained filter.
                } else {
                    assert_eq!(result.unwrap_err().raw_os_error(), Some(libc::ENOENT));
                }
            }
        }
    }

    #[test]
    fn failed_initial_registration_removes_partially_installed_filters() {
        for applied in 0..=2 {
            let driver = KqueueDriver::new().unwrap();
            let (reader, _writer) = std::os::unix::net::UnixStream::pair().unwrap();
            let changes = [libc::EVFILT_READ, libc::EVFILT_WRITE].map(|filter| {
                KqueueDriver::change(
                    reader.as_raw_fd(),
                    filter,
                    libc::EV_ADD | libc::EV_ENABLE | libc::EV_CLEAR,
                    KqueueDriver::encode_key(Token(0), 1),
                )
            });
            let error = driver
                .install_registration_with(&changes, |changes| {
                    for change in &changes[..applied] {
                        driver.apply_change(*change)?;
                    }
                    Err(io::Error::from_raw_os_error(libc::EIO))
                })
                .unwrap_err();
            assert_eq!(error.raw_os_error(), Some(libc::EIO));
            for filter in [libc::EVFILT_READ, libc::EVFILT_WRITE] {
                // Use the raw deletion helper, not delete_filter's missing-entry
                // normalization, to prove that cleanup removed kernel state.
                let error = driver
                    .apply_change(KqueueDriver::change(
                        reader.as_raw_fd(),
                        filter,
                        libc::EV_DELETE,
                        0,
                    ))
                    .unwrap_err();
                assert_eq!(error.raw_os_error(), Some(libc::ENOENT));
            }
            // The still-open descriptor remains usable for a fresh registration.
            driver
                .install_registration_with(&changes, |changes| driver.apply_changes(changes))
                .unwrap();
            for filter in [libc::EVFILT_READ, libc::EVFILT_WRITE] {
                driver.delete_filter(reader.as_raw_fd(), filter).unwrap();
            }
        }
    }

    #[test]
    fn failed_interest_changes_preserve_waiters_and_retry_remaining_changes() {
        for fail_add in [true, false] {
            let driver = Rc::new(AnyDriver::Kqueue(KqueueDriver::new().unwrap()));
            let (reader, _writer) = std::os::unix::net::UnixStream::pair().unwrap();
            let handle = InnerRawHandle::new_with_driver_and_mode(
                &driver,
                reader.as_raw_fd(),
                Interest::READABLE,
                RegistrationMode::Poll,
            )
            .unwrap();
            let AnyDriver::Kqueue(kqueue) = driver.as_ref() else {
                unreachable!()
            };
            kqueue
                .submit_poll(&handle, Waker::noop().clone(), Interest::READABLE)
                .unwrap();
            kqueue.state.borrow_mut().registrations[handle.token.0].read_ready = true;
            let mut attempted = Vec::new();
            let error = kqueue
                .reregister_with(&handle, Interest::WRITABLE, |fd, filter, add, key| {
                    attempted.push((filter, add));
                    if add == fail_add {
                        return Err(io::Error::from_raw_os_error(libc::EIO));
                    }
                    assert!(add);
                    kqueue.apply_change(KqueueDriver::change(
                        fd,
                        filter,
                        libc::EV_ADD | libc::EV_ENABLE | libc::EV_CLEAR,
                        key,
                    ))
                })
                .unwrap_err();
            assert_eq!(error.raw_os_error(), Some(libc::EIO));
            assert_eq!(attempted.len(), if fail_add { 1 } else { 2 });
            {
                let state = kqueue.state.borrow();
                let registration = &state.registrations[handle.token.0];
                assert!(registration.registered_read);
                assert_eq!(registration.registered_write, !fail_add);
                assert!(registration.read_waiter.is_some());
                assert!(registration.read_ready);
            }
            let mut retried = Vec::new();
            kqueue
                .reregister_with(&handle, Interest::WRITABLE, |fd, filter, add, key| {
                    retried.push((filter, add));
                    if add {
                        kqueue.apply_change(KqueueDriver::change(
                            fd,
                            filter,
                            libc::EV_ADD | libc::EV_ENABLE | libc::EV_CLEAR,
                            key,
                        ))
                    } else {
                        kqueue.delete_filter(fd, filter)
                    }
                })
                .unwrap();
            let expected = if fail_add {
                vec![(libc::EVFILT_WRITE, true), (libc::EVFILT_READ, false)]
            } else {
                vec![(libc::EVFILT_READ, false)]
            };
            assert_eq!(retried, expected);
            let state = kqueue.state.borrow();
            let registration = &state.registrations[handle.token.0];
            assert!(!registration.registered_read);
            assert!(registration.registered_write);
            assert!(registration.read_waiter.is_none());
            assert!(!registration.read_ready);
        }
    }

    impl std::task::Wake for WakeCount {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }

        fn wake_by_ref(self: &Arc<Self>) {
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn readiness_arriving_before_waiter_is_latched() {
        let driver = Rc::new(AnyDriver::Kqueue(
            KqueueDriver::new().expect("kqueue driver should initialize"),
        ));
        let (reader, mut writer) =
            std::os::unix::net::UnixStream::pair().expect("socket pair should initialize");
        reader
            .set_nonblocking(true)
            .expect("reader should become nonblocking");
        let handle = InnerRawHandle::new_with_driver_and_mode(
            &driver,
            reader.as_raw_fd(),
            Interest::READABLE,
            RegistrationMode::Poll,
        )
        .expect("reader should register");

        writer.write_all(b"!").expect("socket write should succeed");
        driver.wait(Some(Duration::from_millis(100)));

        let wakes = Arc::new(WakeCount(AtomicUsize::new(0)));
        driver
            .submit_poll(&handle, Waker::from(Arc::clone(&wakes)), Interest::READABLE)
            .expect("latched readiness should submit");
        assert_eq!(wakes.0.load(Ordering::SeqCst), 1);
    }
}
