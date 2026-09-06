#![warn(clippy::undocumented_unsafe_blocks)]

use std::cell::RefCell;
use std::collections::VecDeque;
use std::io::{self, ErrorKind};
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd, OwnedFd, RawFd};
use std::sync::Arc as StdArc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Waker};
use std::time::Duration;

use io_uring::types::{SubmitArgs, Timespec};
use io_uring::{IoUring, cqueue, opcode, squeue, types};
use mio::{Interest, Token};
use slab::Slab;

use crate::vibeio::driver::{CompletionIoResult, Interruptor};
use crate::vibeio::{
    driver::{Driver, RegistrationMode},
    fd_inner::InnerRawHandle,
};

const KEY_KIND_BITS: u64 = 2;
const KEY_KIND_MASK: u64 = (1u64 << KEY_KIND_BITS) - 1;
const POLL_KEY_KIND: u8 = 0;
const COMPLETION_KEY_KIND: u8 = 1;
const ACCEPT_KEY_KIND: u8 = 2;
const MEMORY_FALLBACK_ENTRIES: [u32; 2] = [256, 64];

fn build_with_memory_fallback<T>(
    entries: u32,
    mut build: impl FnMut(u32) -> io::Result<T>,
) -> io::Result<(T, u32)> {
    let mut previous = None;
    let mut last_memory_error = None;

    for candidate in std::iter::once(entries).chain(
        MEMORY_FALLBACK_ENTRIES
            .into_iter()
            .map(|fallback| entries.min(fallback)),
    ) {
        if previous == Some(candidate) {
            continue;
        }
        previous = Some(candidate);

        match build(candidate) {
            Ok(value) => return Ok((value, candidate)),
            Err(err) if err.raw_os_error() == Some(libc::ENOMEM) => {
                last_memory_error = Some(err);
            }
            Err(err) => return Err(err),
        }
    }

    Err(last_memory_error.expect("at least one io_uring build was attempted"))
}

pub struct UringInterruptor {
    eventfd: std::sync::Weak<OwnedFd>,
}

impl Interruptor for UringInterruptor {
    #[inline]
    fn interrupt(&self) {
        if let Some(eventfd) = self.eventfd.upgrade() {
            let value: u64 = 1;
            // SAFETY: the upgraded Arc owns the descriptor throughout write,
            // including when the driver is concurrently shutting down. value is
            // initialized for the required eight-byte eventfd write.
            let _ = unsafe {
                libc::write(
                    eventfd.as_raw_fd(),
                    &value as *const u64 as *const std::ffi::c_void,
                    std::mem::size_of::<u64>(),
                )
            };
        }
    }
}

struct PollRegistration {
    fd: RawFd,
    poll_mask: u32,
    waiter: Option<Waker>,
    poll_armed: bool,
    generation: u32,
}

struct AcceptRegistration {
    results: VecDeque<Result<OwnedFd, i32>>,
    waiter: Option<Waker>,
    armed: bool,
}

struct CompletionRegistration {
    fd: RawFd,
    generation: u32,
    accept: Option<AcceptRegistration>,
}

enum HandleRegistration {
    Completion(CompletionRegistration),
    Poll(PollRegistration),
}

struct Completion {
    waiter: Option<Waker>,
    completed: Option<i32>,
    ignored_data: Option<Box<dyn std::any::Any>>,
    returns_fd: bool,
}

impl Drop for Completion {
    fn drop(&mut self) {
        if self.returns_fd
            && let Some(fd) = self.completed.take().filter(|fd| *fd >= 0)
        {
            // SAFETY: fd-producing operations transfer a new descriptor to this
            // registration. A consumer takes completed before removing it; any
            // result left here is unclaimed and must be closed exactly once.
            unsafe { libc::close(fd) };
        }
    }
}

struct DriverState {
    registrations: Slab<HandleRegistration>,
    completions: Slab<Completion>,
    next_registration_generation: u32,
}

struct CompletionBatch {
    interrupt: bool,
    fast_wakers: [Option<Waker>; 8],
    overflow_wakers: Vec<Waker>,
    retired: Vec<Completion>,
}

impl CompletionBatch {
    fn dispatch(self) {
        // Arbitrary payload destructors and wakers must run outside ring/state
        // borrows; either can reenter the driver or submit another operation.
        drop(self.retired);
        for waker in self.fast_wakers.into_iter().flatten() {
            waker.wake();
        }
        for waker in self.overflow_wakers {
            waker.wake();
        }
    }
}

impl DriverState {
    fn ignore_completion(
        &mut self,
        token: usize,
        data: Box<dyn std::any::Any>,
    ) -> Option<Completion> {
        let Some(completion) = self.completions.get_mut(token) else {
            // Even an unknown token may carry a destructor that reenters the
            // driver. Return its storage for retirement outside the state borrow.
            return Some(Completion {
                waiter: None,
                completed: None,
                ignored_data: Some(data),
                returns_fd: false,
            });
        };
        super::retain_completion_data(&mut completion.ignored_data, data);
        if completion.completed.is_some() {
            // The CQE may have arrived just before cancellation. No further
            // completion will arrive to release this entry and its storage.
            Some(self.completions.remove(token))
        } else {
            None
        }
    }
}

pub struct UringDriver {
    ring: RefCell<IoUring>,
    state: RefCell<DriverState>,
    interrupt_eventfd: StdArc<OwnedFd>,
    interrupt_buffer: RefCell<Box<[u8; 8]>>,
    pending_submissions: AtomicBool,
}

impl Drop for UringDriver {
    fn drop(&mut self) {
        if self.quiesce().is_err() {
            // Ring close can defer cancellation. If the kernel cannot confirm
            // quiescence, retain all potentially kernel-visible allocations.
            // This exceptional-path leak is preferable to freeing live pointers.
            let state = self.state.get_mut();
            std::mem::forget(std::mem::take(&mut state.completions));
            std::mem::forget(std::mem::take(&mut state.registrations));
            let buffer = std::mem::replace(self.interrupt_buffer.get_mut(), Box::new([0; 8]));
            std::mem::forget(buffer);
        }
    }
}

impl UringDriver {
    /// Stop all submitted work before retained operation storage is released.
    fn quiesce(&mut self) -> io::Result<()> {
        let ring = self.ring.get_mut();
        let state = self.state.get_mut();
        // Sync cancellation does not cover SQEs still waiting in userspace.
        // Bound retries: an unconsumed SQ must take the retention fallback.
        for _ in 0..4 {
            Self::drain_shutdown_cq(ring, state);
            if ring.submission().is_empty() {
                break;
            }
            ring.submit()?;
        }
        if !ring.submission().is_empty() {
            return Err(io::Error::other(
                "io_uring shutdown left pending submissions",
            ));
        }
        match ring.submitter().register_sync_cancel(
            Some(Timespec::from(Duration::from_secs(1))),
            types::CancelBuilder::any().all(),
        ) {
            Ok(()) => {}
            Err(err) if err.raw_os_error() == Some(libc::ENOENT) => {}
            Err(err) => return Err(err),
        }

        // No user callbacks or rearming during shutdown. Preserve successful
        // descriptor results so their registration destructors can close them.
        loop {
            Self::drain_shutdown_cq(ring, state);
            if !ring.submission().cq_overflow() {
                break;
            }
            // With NODROP, CQEs can remain in the kernel overflow list after
            // cancellation. Publish consumed slots, then flush that list before
            // discarding the ring (otherwise successful open/accept fds leak).
            ring.submit_and_wait(0)?;
        }
        Ok(())
    }

    fn drain_shutdown_cq(ring: &mut IoUring, state: &mut DriverState) {
        for cqe in ring.completion() {
            let key = cqe.user_data();
            if key == u64::MAX {
                continue;
            }
            match Self::decode_key_kind(key) {
                ACCEPT_KEY_KIND if cqe.result() >= 0 => {
                    // SAFETY: an unconsumed successful accept CQE owns a fresh fd.
                    unsafe { libc::close(cqe.result()) };
                }
                COMPLETION_KEY_KIND => {
                    if let Some(completion) = state.completions.get_mut(Self::decode_token(key).0) {
                        completion.completed = Some(cqe.result());
                    }
                }
                _ => {}
            }
        }
    }

    #[inline]
    pub(crate) fn new(entries: u32, builder: io_uring::Builder) -> Result<Self, io::Error> {
        // Ring teardown is deferred by the kernel. Rapid runtime churn can
        // therefore hit ENOMEM even though earlier rings have been dropped;
        // smaller queues keep initialization reliable while cleanup catches up.
        let (ring, ring_entries) =
            build_with_memory_fallback(entries, |candidate| builder.build(candidate))?;
        if !ring.params().is_feature_ext_arg() {
            return Err(io::Error::new(
                ErrorKind::Unsupported,
                "rsloop requires Linux 6.1+ with io_uring extended arguments",
            ));
        }

        // Create eventfd only after ring initialization succeeds so failed
        // attempts cannot leak descriptors.
        // SAFETY: eventfd takes only integer arguments. Success returns a new
        // descriptor, checked below and immediately acquired by OwnedFd.
        let eventfd = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
        if eventfd < 0 {
            return Err(io::Error::last_os_error());
        }
        let driver = Self {
            ring: RefCell::new(ring),
            state: RefCell::new(DriverState {
                registrations: Slab::with_capacity(ring_entries as usize),
                completions: Slab::with_capacity(ring_entries as usize),
                next_registration_generation: 0,
            }),
            // SAFETY: eventfd returned a fresh descriptor owned by this driver.
            interrupt_eventfd: StdArc::new(unsafe { OwnedFd::from_raw_fd(eventfd) }),
            interrupt_buffer: RefCell::new(Box::new([0; 8])),
            pending_submissions: AtomicBool::new(false),
        };

        driver.submit_interrupt();

        Ok(driver)
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
    fn encode_completion_key(token: usize) -> u64 {
        ((token as u64) << KEY_KIND_BITS) | COMPLETION_KEY_KIND as u64
    }

    #[inline]
    fn encode_poll_key(token: Token, generation: u32) -> u64 {
        ((u64::from(generation) & 0x3fff_ffff) << 34)
            | ((token.0 as u64 & u64::from(u32::MAX)) << KEY_KIND_BITS)
            | POLL_KEY_KIND as u64
    }

    #[inline]
    fn decode_token(key: u64) -> Token {
        Token(((key >> KEY_KIND_BITS) & u64::from(u32::MAX)) as usize)
    }

    #[inline]
    fn decode_poll_generation(key: u64) -> u32 {
        (key >> 34) as u32
    }

    #[inline]
    fn encode_accept_key(token: Token, generation: u32) -> u64 {
        ((u64::from(generation) & 0x3fff_ffff) << 34)
            | ((token.0 as u64 & u64::from(u32::MAX)) << KEY_KIND_BITS)
            | ACCEPT_KEY_KIND as u64
    }

    #[inline]
    fn decode_key_kind(key: u64) -> u8 {
        (key & KEY_KIND_MASK) as u8
    }

    #[inline]
    fn interest_to_poll_mask(interest: Interest) -> u32 {
        let mut mask = 0;
        if interest.is_readable() {
            mask |= libc::POLLIN as u32;
        }
        if interest.is_writable() {
            mask |= libc::POLLOUT as u32;
        }
        mask
    }

    #[inline]
    fn submitter_call_result(result: Result<usize, io::Error>) -> Result<(), io::Error> {
        match result {
            Ok(_) => Ok(()),
            Err(err) if err.raw_os_error() == Some(libc::EBUSY) => Ok(()),
            Err(err) if err.raw_os_error() == Some(libc::ETIME) => Ok(()), // io_uring Timeout
            Err(err) => Err(err),
        }
    }

    #[inline]
    fn push_entry(&self, entry: squeue::Entry) -> Result<(), io::Error> {
        let mut ring = self.ring.borrow_mut();

        if ring.submission().is_full() {
            Self::submitter_call_result(ring.submit())?;
        }

        let mut sq = ring.submission();
        // SAFETY: callers build entries from driver-owned interrupt/readiness
        // state or an operation whose storage is retained through completion.
        // Dropped operations transfer their kernel-visible allocations into
        // completion retention; shutdown quiesces or retains them on failure.
        // push copies the entry, so the local SQE itself need not outlive this call.
        unsafe {
            sq.push(&entry)
                .map_err(|_| io::Error::other("io_uring submission queue is full"))?;
        }

        self.pending_submissions.store(true, Ordering::Release);

        Ok(())
    }

    #[inline]
    fn push_poll_add(
        &self,
        token: Token,
        generation: u32,
        fd: RawFd,
        poll_mask: u32,
    ) -> Result<(), io::Error> {
        let entry = opcode::PollAdd::new(types::Fd(fd), poll_mask)
            .multi(true)
            .build()
            .user_data(Self::encode_poll_key(token, generation));
        self.push_entry(entry)
    }

    #[inline]
    fn collect_completions(
        &self,
        wait_for_one: bool,
        timeout: Option<Duration>,
    ) -> Result<(), io::Error> {
        {
            let mut ring = self.ring.borrow_mut();
            let should_submit = if wait_for_one {
                true
            } else {
                !ring.submission().is_empty()
            };

            if should_submit {
                let submit_result = if wait_for_one {
                    if let Some(timeout) = timeout {
                        let timespec = Timespec::from(timeout);
                        ring.submitter()
                            .submit_with_args(1, &SubmitArgs::new().timespec(&timespec))
                    } else {
                        ring.submit_and_wait(1)
                    }
                } else {
                    ring.submit()
                };
                Self::submitter_call_result(submit_result)?;
                self.pending_submissions
                    .store(!ring.submission().is_empty(), Ordering::Release);
            } else {
                self.pending_submissions.store(false, Ordering::Release);
            }
        }

        // Drain any new completions produced by the submit above.
        let batch = {
            let mut ring = self.ring.borrow_mut();
            let mut state = self.state.borrow_mut();
            Self::drain_cq(&mut ring, &mut state)
        };
        if batch.interrupt {
            self.submit_interrupt();
        }
        batch.dispatch();

        Ok(())
    }

    /// Drain the completion queue, deferring user callbacks until borrows end.
    #[inline]
    fn drain_cq(ring: &mut IoUring, state: &mut DriverState) -> CompletionBatch {
        let mut interrupt = false;

        // Collect wakers in a small inline array to avoid heap allocation
        // in the common case (0-8 completions per collect_completions call).
        // Most flush/wait calls produce very few completions.
        let mut fast_wakers: [Option<Waker>; 8] = Default::default();
        let mut fast_count = 0;
        let mut overflow_wakers: Vec<Waker> = Vec::new();
        let mut retired = Vec::new();

        {
            let cq = ring.completion();

            for cqe in cq {
                let key = cqe.user_data();
                let result = cqe.result();

                if key == u64::MAX {
                    // Task interrupted
                    interrupt = true;
                    continue;
                }

                let token = Self::decode_token(key);
                let key_kind = Self::decode_key_kind(key);

                if key_kind == POLL_KEY_KIND {
                    let generation = Self::decode_poll_generation(key);
                    let waiter = match state.registrations.get_mut(token.0) {
                        Some(HandleRegistration::Poll(registration))
                            if registration.generation == generation =>
                        {
                            registration.poll_armed = cqueue::more(cqe.flags());
                            registration.waiter.take()
                        }
                        _ => None,
                    };
                    if let Some(waiter) = waiter {
                        if fast_count < fast_wakers.len() {
                            fast_wakers[fast_count] = Some(waiter);
                        } else {
                            overflow_wakers.push(waiter);
                        }
                        fast_count += 1;
                    }
                    continue;
                }

                if key_kind == ACCEPT_KEY_KIND {
                    let generation = Self::decode_poll_generation(key);
                    let mut accepted = Some(if result >= 0 {
                        // SAFETY: a successful accept CQE transfers ownership of
                        // one new descriptor. Queue ownership or this local's
                        // drop closes it unless it is returned to a caller.
                        Ok(unsafe { OwnedFd::from_raw_fd(result) })
                    } else {
                        Err(result)
                    });
                    if let Some(HandleRegistration::Completion(registration)) =
                        state.registrations.get_mut(token.0)
                    {
                        if registration.generation == generation {
                            if let Some(accept) = registration.accept.as_mut() {
                                accept.armed = cqueue::more(cqe.flags());
                                accept.results.push_back(accepted.take().unwrap());
                                if let Some(waiter) = accept.waiter.take() {
                                    if fast_count < fast_wakers.len() {
                                        fast_wakers[fast_count] = Some(waiter);
                                    } else {
                                        overflow_wakers.push(waiter);
                                    }
                                    fast_count += 1;
                                }
                            }
                        }
                    }
                    // Stale registration/generation: drop any undelivered fd.
                    drop(accepted);
                    continue;
                }

                let mut remove_completion = false;
                let waiter = match state.completions.get_mut(token.0) {
                    Some(completion) => {
                        completion.completed = Some(result);
                        remove_completion = completion.ignored_data.is_some();
                        completion.waiter.take()
                    }
                    None => None,
                };
                if remove_completion {
                    retired.push(state.completions.remove(token.0));
                }
                if let Some(waiter) = waiter {
                    if fast_count < fast_wakers.len() {
                        fast_wakers[fast_count] = Some(waiter);
                    } else {
                        overflow_wakers.push(waiter);
                    }
                    fast_count += 1;
                }
            }
        }

        CompletionBatch {
            interrupt,
            fast_wakers,
            overflow_wakers,
            retired,
        }
    }

    #[inline]
    fn submit_interrupt(&self) {
        use io_uring::{opcode, types};
        // Submit a read operation to the eventfd to wake up the driver
        let mut buffer = self.interrupt_buffer.borrow_mut();
        let entry = opcode::Read::new(
            types::Fd(self.interrupt_eventfd.as_raw_fd()),
            buffer.as_mut_ptr(),
            buffer.len() as u32,
        )
        .build()
        .user_data(u64::MAX);

        // We use push_entry here. It handles submission if full.
        // We panic if it fails because we cannot recover (we won't be able to wake up).
        if let Err(err) = self.push_entry(entry) {
            panic!("io_uring: failed to submit interrupt task: {}", err);
        }
    }
}

#[cfg(test)]
mod memory_fallback_tests {
    #[test]
    fn live_shutdown_drains_overflowed_descriptor_results() {
        use super::*;
        let mut builder = IoUring::builder();
        builder.setup_cqsize(2);
        let mut driver = match UringDriver::new(2, builder) {
            Ok(driver) => driver,
            Err(err)
                if matches!(
                    err.raw_os_error(),
                    Some(libc::EPERM | libc::ENOSYS | libc::EOPNOTSUPP)
                ) =>
            {
                eprintln!("live io_uring overflow test unavailable: {err}");
                return;
            }
            Err(err) => panic!("io_uring initialization failed: {err}"),
        };
        assert!(driver.ring.get_mut().params().is_feature_nodrop());
        // Three open results exceed this deliberately tiny CQ without consuming
        // completions. The initial eventfd read may remain internally poll-armed.
        driver.ring.get_mut().submit().unwrap();
        let mut tokens = Vec::new();
        for _ in 0..3 {
            let path = std::ffi::CString::new("/dev/null").unwrap();
            let ptr = path.as_ptr();
            let token = driver.state.get_mut().completions.insert(Completion {
                waiter: None,
                completed: None,
                ignored_data: Some(Box::new(path)),
                returns_fd: true,
            });
            driver
                .push_entry(
                    opcode::OpenAt::new(types::Fd(libc::AT_FDCWD), ptr)
                        .flags(libc::O_RDONLY | libc::O_CLOEXEC)
                        .build()
                        .user_data(UringDriver::encode_completion_key(token)),
                )
                .unwrap();
            tokens.push(token);
        }
        driver.ring.get_mut().submit().unwrap();
        let deadline = std::time::Instant::now() + Duration::from_secs(5);
        while !driver.ring.get_mut().submission().cq_overflow() {
            assert!(std::time::Instant::now() < deadline, "CQ did not overflow");
            std::thread::sleep(Duration::from_millis(1));
        }
        driver.quiesce().unwrap();
        assert!(!driver.ring.get_mut().submission().cq_overflow());
        for token in tokens {
            assert!(driver.state.get_mut().completions[token].completed.unwrap() >= 0);
        }
        // Registration destructors own and close all successful descriptors.
        drop(driver);
    }

    #[test]
    fn live_shutdown_cancels_queued_and_submitted_reads() {
        use super::*;
        use std::cell::Cell;
        use std::rc::Rc;

        struct Buffer {
            bytes: Box<[u8; 8]>,
            dropped: Rc<Cell<bool>>,
        }
        impl Drop for Buffer {
            fn drop(&mut self) {
                self.dropped.set(true);
            }
        }
        for submit_first in [false, true] {
            let mut driver = match UringDriver::new(8, IoUring::builder()) {
                Ok(driver) => driver,
                Err(err)
                    if matches!(
                        err.raw_os_error(),
                        Some(libc::EPERM | libc::ENOSYS | libc::EOPNOTSUPP)
                    ) =>
                {
                    eprintln!("live io_uring shutdown test unavailable: {err}");
                    return;
                }
                Err(err) => panic!("io_uring initialization failed: {err}"),
            };
            let (reader, _writer) = std::os::unix::net::UnixStream::pair().unwrap();
            let dropped = Rc::new(Cell::new(false));
            let mut buffer = Buffer {
                bytes: Box::new([0; 8]),
                dropped: dropped.clone(),
            };
            let ptr = buffer.bytes.as_mut_ptr();
            let token = driver.state.get_mut().completions.insert(Completion {
                waiter: None,
                completed: None,
                ignored_data: Some(Box::new(buffer)),
                returns_fd: false,
            });
            let entry = opcode::Read::new(types::Fd(reader.as_raw_fd()), ptr, 8)
                .build()
                .user_data(UringDriver::encode_completion_key(token));
            driver.push_entry(entry).unwrap();
            if submit_first {
                driver.ring.get_mut().submit().unwrap();
            }
            assert!(!dropped.get());
            driver.quiesce().unwrap();
            assert_eq!(
                driver.state.get_mut().completions[token].completed,
                Some(-libc::ECANCELED)
            );
            assert!(
                !dropped.get(),
                "storage must survive cancellation acknowledgement"
            );
            drop(driver);
            assert!(dropped.get(), "confirmed shutdown should release storage");
        }
    }

    #[test]
    fn interrupt_descriptor_survives_in_flight_wake_owner() {
        use super::*;
        // SAFETY: eventfd takes only integer arguments and returns a fresh fd.
        let raw = unsafe { libc::eventfd(0, libc::EFD_NONBLOCK | libc::EFD_CLOEXEC) };
        assert!(raw >= 0);
        // SAFETY: the successful syscall transfers ownership exactly once.
        let owner = StdArc::new(unsafe { OwnedFd::from_raw_fd(raw) });
        let weak = StdArc::downgrade(&owner);
        let interruptor = UringInterruptor {
            eventfd: weak.clone(),
        };
        // Model an interrupt thread upgrading just before driver teardown.
        let in_flight = weak.upgrade().unwrap();
        drop(owner);
        interruptor.interrupt();
        let mut count = 0u64;
        // SAFETY: in_flight owns the fd and count is an eight-byte writable value.
        let read = unsafe {
            libc::read(
                in_flight.as_raw_fd(),
                std::ptr::from_mut(&mut count).cast(),
                std::mem::size_of_val(&count),
            )
        };
        assert_eq!(read, 8);
        assert_eq!(count, 1);
        drop(in_flight);
        assert!(weak.upgrade().is_none());
        // A wake after final ownership release is a no-op, not a raw-fd write.
        interruptor.interrupt();
    }
    use super::*;

    #[test]
    fn retries_smaller_rings_after_out_of_memory() {
        let mut attempts = Vec::new();
        let selected = build_with_memory_fallback(1024, |entries| {
            attempts.push(entries);
            if entries > 64 {
                Err(io::Error::from_raw_os_error(libc::ENOMEM))
            } else {
                Ok(entries)
            }
        })
        .expect("small ring should initialize");

        assert_eq!(selected, (64, 64));
        assert_eq!(attempts, [1024, 256, 64]);
    }

    #[test]
    fn does_not_retry_non_memory_errors() {
        let mut attempts = Vec::new();
        let err = build_with_memory_fallback(1024, |entries| -> io::Result<()> {
            attempts.push(entries);
            Err(io::Error::from_raw_os_error(libc::EINVAL))
        })
        .expect_err("invalid configuration should be preserved");

        assert_eq!(err.raw_os_error(), Some(libc::EINVAL));
        assert_eq!(attempts, [1024]);
    }
}

impl Driver for UringDriver {
    type Interruptor = UringInterruptor;

    #[inline]
    fn flush(&self) {
        match self.collect_completions(false, None) {
            Ok(_) => {}
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => panic!("io_uring submit failed while processing I/O completions: {err}"),
        }
    }

    #[inline]
    fn should_flush(&self) -> bool {
        self.pending_submissions.load(Ordering::Acquire)
    }

    #[inline]
    fn wait(&self, timeout: Option<Duration>) {
        match self.collect_completions(true, timeout) {
            Ok(_) => {}
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => panic!("io_uring submit_and_wait failed while waiting for I/O: {err}"),
        }
    }

    #[inline]
    fn get_interruptor(&self) -> Self::Interruptor {
        UringInterruptor {
            eventfd: StdArc::downgrade(&self.interrupt_eventfd),
        }
    }

    #[inline]
    fn register_handle(
        &self,
        handle: &InnerRawHandle,
        interest: Interest,
    ) -> Result<Token, io::Error> {
        self.register_handle_with_mode(handle, interest, RegistrationMode::Completion)
    }

    #[inline]
    fn register_handle_with_mode(
        &self,
        handle: &InnerRawHandle,
        interest: Interest,
        mode: RegistrationMode,
    ) -> Result<Token, io::Error> {
        let mut state = self.state.borrow_mut();
        state.next_registration_generation =
            state.next_registration_generation.wrapping_add(1) & 0x3fff_ffff;
        if state.next_registration_generation == 0 {
            state.next_registration_generation = 1;
        }
        let generation = state.next_registration_generation;
        let entry = state.registrations.vacant_entry();
        let token = Token(entry.key());

        match mode {
            RegistrationMode::Completion => {
                entry.insert(HandleRegistration::Completion(CompletionRegistration {
                    fd: handle.handle,
                    generation,
                    accept: None,
                }));
            }
            RegistrationMode::Poll => {
                entry.insert(HandleRegistration::Poll(PollRegistration {
                    fd: handle.handle,
                    poll_mask: Self::interest_to_poll_mask(interest),
                    waiter: None,
                    poll_armed: false,
                    generation,
                }));
            }
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
        match state.registrations.get_mut(handle.token.0) {
            Some(HandleRegistration::Completion(_)) => Ok(()),
            Some(HandleRegistration::Poll(registration)) => {
                registration.poll_mask = Self::interest_to_poll_mask(interest);
                Ok(())
            }
            None => Err(io::Error::new(
                ErrorKind::NotFound,
                format!(
                    "I/O token {} is not registered with this driver",
                    handle.token.0
                ),
            )),
        }
    }

    #[inline]
    fn deregister_handle(&self, handle: &InnerRawHandle) -> Result<(), io::Error> {
        {
            // Cancel any pending io_uring operations for this handle
            let ring = self.ring.borrow_mut();
            let _ = ring.submitter().register_sync_cancel(
                Some(Timespec::new().nsec(0).sec(0)),
                types::CancelBuilder::fd(types::Fd(handle.handle)),
            );
        }

        let registration = self
            .state
            .borrow_mut()
            .registrations
            .try_remove(handle.token.0);
        if registration.is_none() {
            return Err(io::Error::new(
                ErrorKind::NotFound,
                format!(
                    "I/O token {} is not registered with this driver",
                    handle.token.0
                ),
            ));
        }
        drop(registration);
        Ok(())
    }

    #[inline]
    fn supports_completion(&self) -> bool {
        true
    }

    #[inline]
    fn submit_poll(
        &self,
        handle: &InnerRawHandle,
        waker: Waker,
        interest: Interest,
    ) -> Result<(), io::Error> {
        let token = handle.token();
        let old_waker;
        let poll_spec = {
            let mut state = self.state.borrow_mut();
            let registration = match state.registrations.get_mut(token.0) {
                Some(HandleRegistration::Poll(registration)) => registration,
                Some(HandleRegistration::Completion(_)) => {
                    return Err(io::Error::new(
                        ErrorKind::Unsupported,
                        format!(
                            "I/O token {} is registered for completion mode, not poll mode",
                            token.0
                        ),
                    ));
                }
                None => {
                    return Err(io::Error::new(
                        ErrorKind::NotFound,
                        format!("I/O token {} is not registered with this driver", token.0),
                    ));
                }
            };

            old_waker = Self::update_waiter(&mut registration.waiter, waker);
            let desired_mask = Self::interest_to_poll_mask(interest);
            registration.poll_mask = desired_mask;

            if registration.poll_armed {
                None
            } else {
                registration.poll_armed = true;
                Some((registration.generation, registration.fd, desired_mask))
            }
        };

        if let Some((generation, fd, poll_mask)) = poll_spec {
            if let Err(submit_err) = self.push_poll_add(token, generation, fd, poll_mask) {
                let mut state = self.state.borrow_mut();
                let failed_waker = if let Some(HandleRegistration::Poll(registration)) =
                    state.registrations.get_mut(token.0)
                {
                    registration.poll_armed = false;
                    registration.waiter.take()
                } else {
                    None
                };
                drop(state);
                drop(failed_waker);
                return Err(submit_err);
            }
        }

        drop(old_waker);
        Ok(())
    }

    #[inline]
    fn submit_completion<O>(&self, op: &mut O, waker: Waker) -> super::CompletionIoResult
    where
        O: crate::vibeio::op::Op,
    {
        let mut state = self.state.borrow_mut();
        let vacant_completion = state.completions.vacant_entry();
        let token = vacant_completion.key();

        // Build the SQE. If this fails, return the error.
        let entry = match op.build_completion_entry(Self::encode_completion_key(token)) {
            Ok(entry) => entry,
            Err(err) => return CompletionIoResult::SubmitErr(err),
        };

        // Push the SQE into the submission queue. If this fails, undo the inflight
        // flag and clear waiters on the registration.
        if let Err(err) = self.push_entry(entry) {
            return CompletionIoResult::SubmitErr(err);
        }

        // Store the operation in the completions slab.
        vacant_completion.insert(Completion {
            waiter: Some(waker),
            completed: None,
            ignored_data: None,
            returns_fd: op.completion_returns_fd(),
        });

        CompletionIoResult::Retry(token)
    }

    #[inline]
    fn get_completion_result(&self, token: usize) -> Option<i32> {
        let mut state = self.state.borrow_mut();
        let completed = state
            .completions
            .get_mut(token)
            .and_then(|c| c.completed.take());
        let retired = completed.map(|_| state.completions.remove(token));
        drop(state);
        drop(retired);
        completed
    }

    fn poll_multishot_accept(
        &self,
        handle: &InnerRawHandle,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<i32>> {
        let waker = cx.waker().clone();
        let old_waker;
        let submission = {
            let mut state = self.state.borrow_mut();
            let registration = match state.registrations.get_mut(handle.token.0) {
                Some(HandleRegistration::Completion(registration)) => registration,
                Some(HandleRegistration::Poll(_)) => {
                    return Poll::Ready(Err(io::Error::new(
                        ErrorKind::Unsupported,
                        "multishot accept requires completion registration",
                    )));
                }
                None => {
                    return Poll::Ready(Err(io::Error::new(
                        ErrorKind::NotFound,
                        format!("I/O token {} is not registered", handle.token.0),
                    )));
                }
            };
            let accept = registration
                .accept
                .get_or_insert_with(|| AcceptRegistration {
                    results: VecDeque::new(),
                    waiter: None,
                    armed: false,
                });
            if let Some(result) = accept.results.pop_front() {
                return Poll::Ready(
                    result
                        .map(IntoRawFd::into_raw_fd)
                        .map_err(super::completion_error),
                );
            }
            old_waker = Self::update_waiter(&mut accept.waiter, waker);
            if accept.armed {
                None
            } else {
                accept.armed = true;
                Some((registration.fd, registration.generation))
            }
        };

        if let Some((fd, generation)) = submission {
            let entry = opcode::AcceptMulti::new(types::Fd(fd))
                .flags(libc::SOCK_CLOEXEC | libc::SOCK_NONBLOCK)
                .build()
                .user_data(Self::encode_accept_key(handle.token, generation));
            if let Err(err) = self.push_entry(entry) {
                let mut state = self.state.borrow_mut();
                let failed_waker = if let Some(HandleRegistration::Completion(registration)) =
                    state.registrations.get_mut(handle.token.0)
                {
                    if let Some(accept) = registration.accept.as_mut() {
                        accept.armed = false;
                        accept.waiter.take()
                    } else {
                        None
                    }
                } else {
                    None
                };
                drop(state);
                drop(failed_waker);
                return Poll::Ready(Err(err));
            }
        }
        drop(old_waker);
        Poll::Pending
    }

    #[inline]
    fn set_completion_waker(&self, token: usize, waker: Waker) {
        let mut state = self.state.borrow_mut();
        let old_waker = if let Some(c) = state.completions.get_mut(token) {
            Self::update_waiter(&mut c.waiter, waker)
        } else {
            Some(waker)
        };
        drop(state);
        drop(old_waker);
    }

    #[inline]
    fn ignore_completion(&self, token: usize, data: Box<dyn std::any::Any>) {
        let retired = self.state.borrow_mut().ignore_completion(token, data);
        // Drop user-owned storage outside the state borrow.
        drop(retired);
    }
}

#[cfg(test)]
mod completion_cleanup_tests {
    use super::*;
    use std::io::Read;
    use std::os::fd::{FromRawFd, IntoRawFd, OwnedFd};
    use std::os::unix::net::UnixStream;
    use std::sync::Arc;

    #[test]
    fn listener_teardown_releases_accept_waker_outside_driver_borrows() {
        struct ReentrantDrop(Arc<std::sync::atomic::AtomicUsize>);
        #[allow(
            clippy::manual_noop_waker,
            reason = "The destructor probes driver reentrancy; Waker::noop cannot exercise it"
        )]
        impl std::task::Wake for ReentrantDrop {
            fn wake(self: Arc<Self>) {}
        }
        impl Drop for ReentrantDrop {
            fn drop(&mut self) {
                let owner = crate::vibeio::executor::current_driver().unwrap();
                let crate::vibeio::driver::AnyDriver::IoUring(driver) = owner.as_ref() else {
                    unreachable!()
                };
                assert!(driver.state.try_borrow_mut().is_ok());
                assert!(driver.ring.try_borrow_mut().is_ok());
                self.0.fetch_add(1, Ordering::SeqCst);
            }
        }
        let runtime =
            crate::vibeio::Runtime::new(crate::vibeio::driver::AnyDriver::new_uring().unwrap());
        runtime.block_on(async {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let handle = InnerRawHandle::new(listener.as_raw_fd(), Interest::READABLE).unwrap();
            let (queued, mut peer) = UnixStream::pair().unwrap();
            peer.set_nonblocking(true).unwrap();
            let drops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
            let owner = crate::vibeio::executor::current_driver().unwrap();
            let crate::vibeio::driver::AnyDriver::IoUring(driver) = owner.as_ref() else {
                unreachable!()
            };
            {
                let mut state = driver.state.borrow_mut();
                let HandleRegistration::Completion(registration) =
                    state.registrations.get_mut(handle.token.0).unwrap()
                else {
                    unreachable!()
                };
                registration.accept = Some(AcceptRegistration {
                    results: VecDeque::from([Ok(OwnedFd::from(queued))]),
                    waiter: Some(Waker::from(Arc::new(ReentrantDrop(drops.clone())))),
                    armed: false,
                });
            }
            assert_eq!(drops.load(Ordering::SeqCst), 0);
            drop(handle);
            assert_eq!(drops.load(Ordering::SeqCst), 1);
            assert_eq!(peer.read(&mut [0]).unwrap(), 0);
        });
    }

    #[test]
    fn accept_queue_closes_only_descriptors_it_still_owns() {
        let (abandoned, mut abandoned_peer) = UnixStream::pair().unwrap();
        let (transferred, mut transferred_peer) = UnixStream::pair().unwrap();
        abandoned_peer.set_nonblocking(true).unwrap();
        transferred_peer.set_nonblocking(true).unwrap();
        let mut accept = AcceptRegistration {
            results: VecDeque::from([
                Ok(OwnedFd::from(transferred)),
                Err(-libc::ECONNABORTED),
                Ok(OwnedFd::from(abandoned)),
            ]),
            waiter: None,
            armed: false,
        };
        let transferred = accept.results.pop_front().unwrap().unwrap().into_raw_fd();
        // SAFETY: the popped result just transferred sole fd ownership.
        let transferred = unsafe { OwnedFd::from_raw_fd(transferred) };
        drop(accept);
        assert_eq!(abandoned_peer.read(&mut [0]).unwrap(), 0);
        assert_eq!(
            transferred_peer.read(&mut [0]).unwrap_err().kind(),
            ErrorKind::WouldBlock
        );
        drop(transferred);
        assert_eq!(transferred_peer.read(&mut [0]).unwrap(), 0);
    }

    #[test]
    fn multishot_accept_decodes_extreme_errors_without_panicking() {
        let runtime =
            crate::vibeio::Runtime::new(crate::vibeio::driver::AnyDriver::new_uring().unwrap());
        runtime.block_on(async {
            let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
            let handle = InnerRawHandle::new(listener.as_raw_fd(), Interest::READABLE).unwrap();
            let owner = crate::vibeio::executor::current_driver().unwrap();
            let crate::vibeio::driver::AnyDriver::IoUring(driver) = owner.as_ref() else { unreachable!() };
            {
                let mut state = driver.state.borrow_mut();
                let HandleRegistration::Completion(registration) = state.registrations.get_mut(handle.token.0).unwrap() else { unreachable!() };
                registration.accept = Some(AcceptRegistration {
                    results: VecDeque::from([Err(i32::MIN), Err(-libc::ECONNABORTED)]),
                    waiter: None,
                    armed: false,
                });
            }
            let mut cx = Context::from_waker(Waker::noop());
            assert!(matches!(driver.poll_multishot_accept(&handle, &mut cx), Poll::Ready(Err(error)) if error.kind() == ErrorKind::InvalidData));
            assert!(matches!(driver.poll_multishot_accept(&handle, &mut cx), Poll::Ready(Err(error)) if error.raw_os_error() == Some(libc::ECONNABORTED)));
        });
    }

    thread_local! {
        static WAKER_DROP_DRIVER: RefCell<Option<std::rc::Rc<UringDriver>>> = const { RefCell::new(None) };
    }
    struct WakerDropScope;
    impl Drop for WakerDropScope {
        fn drop(&mut self) {
            let driver = WAKER_DROP_DRIVER.with(|slot| slot.borrow_mut().take());
            drop(driver);
        }
    }
    struct CheckWakerDrop(Arc<std::sync::atomic::AtomicUsize>);
    #[allow(
        clippy::manual_noop_waker,
        reason = "The custom destructor verifies replacement cleanup, unlike Waker::noop"
    )]
    impl std::task::Wake for CheckWakerDrop {
        fn wake(self: Arc<Self>) {}
    }
    impl Drop for CheckWakerDrop {
        fn drop(&mut self) {
            WAKER_DROP_DRIVER.with(|slot| {
                let slot = slot.borrow();
                let driver = slot.as_ref().unwrap();
                assert!(driver.state.try_borrow_mut().is_ok());
                assert!(driver.ring.try_borrow_mut().is_ok());
            });
            self.0.fetch_add(1, Ordering::SeqCst);
        }
    }

    #[test]
    fn completion_waker_replacement_and_unknown_token_drop_outside_borrows() {
        let driver = match UringDriver::new(8, IoUring::builder()) {
            Ok(driver) => std::rc::Rc::new(driver),
            Err(error)
                if matches!(
                    error.raw_os_error(),
                    Some(libc::EPERM | libc::ENOSYS | libc::EOPNOTSUPP)
                ) =>
            {
                eprintln!("io_uring waker replacement test unavailable: {error}");
                return;
            }
            Err(error) => panic!("io_uring initialization failed: {error}"),
        };
        WAKER_DROP_DRIVER.with(|slot| *slot.borrow_mut() = Some(driver.clone()));
        let _scope = WakerDropScope;
        let drops = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let token = driver.state.borrow_mut().completions.insert(Completion {
            waiter: Some(Waker::from(Arc::new(CheckWakerDrop(drops.clone())))),
            completed: Some(0),
            ignored_data: None,
            returns_fd: false,
        });
        driver.set_completion_waker(token, Waker::noop().clone());
        assert_eq!(drops.load(Ordering::SeqCst), 1);
        assert_eq!(driver.get_completion_result(token), Some(0));
        driver.set_completion_waker(token, Waker::from(Arc::new(CheckWakerDrop(drops.clone()))));
        assert_eq!(drops.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn live_completion_retires_payload_outside_driver_borrows_before_waking() {
        let driver = match UringDriver::new(8, IoUring::builder()) {
            Ok(driver) => std::rc::Rc::new(driver),
            Err(error)
                if matches!(
                    error.raw_os_error(),
                    Some(libc::EPERM | libc::ENOSYS | libc::EOPNOTSUPP)
                ) =>
            {
                eprintln!("live io_uring reentrancy test unavailable: {error}");
                return;
            }
            Err(error) => panic!("io_uring initialization failed: {error}"),
        };
        struct Payload {
            driver: std::rc::Weak<UringDriver>,
            retired: Arc<AtomicBool>,
        }
        impl Drop for Payload {
            fn drop(&mut self) {
                let driver = self.driver.upgrade().unwrap();
                assert!(driver.state.try_borrow_mut().is_ok());
                assert!(driver.ring.try_borrow_mut().is_ok());
                driver.collect_completions(false, None).unwrap();
                self.retired.store(true, Ordering::SeqCst);
            }
        }
        struct WakeAfterRetirement {
            retired: Arc<AtomicBool>,
            woke: Arc<AtomicBool>,
        }
        impl std::task::Wake for WakeAfterRetirement {
            fn wake(self: Arc<Self>) {
                assert!(self.retired.load(Ordering::SeqCst));
                self.woke.store(true, Ordering::SeqCst);
            }
        }
        let retired = Arc::new(AtomicBool::new(false));
        let woke = Arc::new(AtomicBool::new(false));
        let token = driver.state.borrow_mut().completions.insert(Completion {
            waiter: Some(Waker::from(Arc::new(WakeAfterRetirement {
                retired: retired.clone(),
                woke: woke.clone(),
            }))),
            completed: None,
            ignored_data: Some(Box::new(Payload {
                driver: std::rc::Rc::downgrade(&driver),
                retired: retired.clone(),
            })),
            returns_fd: false,
        });
        driver
            .push_entry(
                opcode::Nop::new()
                    .build()
                    .user_data(UringDriver::encode_completion_key(token)),
            )
            .unwrap();
        driver
            .collect_completions(true, Some(Duration::from_secs(1)))
            .unwrap();
        assert!(retired.load(Ordering::SeqCst));
        assert!(woke.load(Ordering::SeqCst));
        assert!(!driver.state.borrow().completions.contains(token));
    }

    fn registration(result: Option<i32>, returns_fd: bool) -> Completion {
        Completion {
            waiter: None,
            completed: result,
            ignored_data: None,
            returns_fd,
        }
    }

    #[test]
    fn unknown_completion_payload_drops_outside_driver_state_borrow() {
        struct Payload {
            driver: std::rc::Weak<UringDriver>,
            borrow_available: std::rc::Rc<std::cell::Cell<bool>>,
        }
        impl Drop for Payload {
            fn drop(&mut self) {
                let driver = self.driver.upgrade().unwrap();
                self.borrow_available
                    .set(driver.state.try_borrow_mut().is_ok());
            }
        }
        let driver = std::rc::Rc::new(UringDriver::new(8, IoUring::builder()).unwrap());
        let available = std::rc::Rc::new(std::cell::Cell::new(false));
        driver.ignore_completion(
            usize::MAX,
            Box::new(Payload {
                driver: std::rc::Rc::downgrade(&driver),
                borrow_available: available.clone(),
            }),
        );
        assert!(
            available.get(),
            "payload destructor ran under the state borrow"
        );
    }

    #[test]
    fn repeated_ignore_retains_all_payloads_until_completion() {
        let mut state = state();
        let token = state.completions.insert(registration(None, false));
        let first = Arc::new(());
        let first_weak = Arc::downgrade(&first);
        let second = Arc::new(());
        let second_weak = Arc::downgrade(&second);
        assert!(state.ignore_completion(token, Box::new(first)).is_none());
        assert!(state.ignore_completion(token, Box::new(second)).is_none());
        assert!(
            first_weak.upgrade().is_some(),
            "first retained payload was freed early"
        );
        assert!(second_weak.upgrade().is_some());
        state.completions[token].completed = Some(-libc::ECANCELED);
        drop(state.completions.remove(token));
        assert!(first_weak.upgrade().is_none());
        assert!(second_weak.upgrade().is_none());
    }

    fn state() -> DriverState {
        DriverState {
            registrations: Slab::new(),
            completions: Slab::new(),
            next_registration_generation: 0,
        }
    }

    #[test]
    fn unclaimed_fd_is_closed_after_completion() {
        let (socket, mut peer) = UnixStream::pair().unwrap();
        peer.set_nonblocking(true).unwrap();
        let completion = registration(Some(socket.into_raw_fd()), true);
        drop(completion);
        assert_eq!(peer.read(&mut [0; 1]).unwrap(), 0);
    }

    #[test]
    fn claimed_fd_survives_registration_removal() {
        let (socket, mut peer) = UnixStream::pair().unwrap();
        peer.set_nonblocking(true).unwrap();
        let mut completion = registration(Some(socket.into_raw_fd()), true);
        let fd = completion.completed.take().unwrap();
        // SAFETY: taking the successful result transfers its descriptor to us.
        let owned = unsafe { OwnedFd::from_raw_fd(fd) };
        drop(completion);
        assert_eq!(
            peer.read(&mut [0; 1]).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
        drop(owned);
        assert_eq!(peer.read(&mut [0; 1]).unwrap(), 0);
    }

    #[test]
    fn byte_count_completion_does_not_close_matching_descriptor() {
        let (socket, mut peer) = UnixStream::pair().unwrap();
        peer.set_nonblocking(true).unwrap();
        use std::os::fd::AsRawFd;
        drop(registration(Some(socket.as_raw_fd()), false));
        assert_eq!(
            peer.read(&mut [0; 1]).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }

    #[test]
    fn cancellation_after_cqe_releases_storage_and_unclaimed_fd() {
        let (socket, mut peer) = UnixStream::pair().unwrap();
        peer.set_nonblocking(true).unwrap();
        let mut state = state();
        let token = state
            .completions
            .insert(registration(Some(socket.into_raw_fd()), true));
        let lifetime = Arc::new(());
        let weak = Arc::downgrade(&lifetime);
        let retired = state.ignore_completion(token, Box::new(lifetime));
        assert!(state.completions.is_empty());
        assert!(weak.upgrade().is_some());
        drop(retired);
        assert!(weak.upgrade().is_none());
        assert_eq!(peer.read(&mut [0; 1]).unwrap(), 0);
    }

    #[test]
    fn cancellation_before_cqe_keeps_storage_until_acknowledgement() {
        let mut state = state();
        let token = state.completions.insert(registration(None, false));
        let lifetime = Arc::new(());
        let weak = Arc::downgrade(&lifetime);
        assert!(state.ignore_completion(token, Box::new(lifetime)).is_none());
        assert!(weak.upgrade().is_some());
        state.completions[token].completed = Some(-libc::ECANCELED);
        drop(state.completions.remove(token));
        assert!(weak.upgrade().is_none());
    }
}
