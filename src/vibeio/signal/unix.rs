//! Unix signal handling implementation for `vibeio`.
//!
//! This module provides async signal handling for Unix systems using a
//! dedicated dispatch thread and pipe-based communication.
//!
//! # Implementation details
//! - A background thread reads from a pipe connected to signal handlers.
//! - Signal handlers mark atomic pending flags and write wakeups to the pipe.
//! - The dispatch thread wakes registered wakers for received signals.
//! - Multiple listeners for the same signal share the same handler.

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(clippy::undocumented_unsafe_blocks)]

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, AtomicI32, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use futures_util::future::poll_fn;
use once_cell::sync::OnceCell;

/// Unix signal kind wrapper.
///
/// Represents a Unix signal number. Common signal kinds are provided as
/// convenience methods:
/// - `SignalKind::interrupt()` - SIGINT (Ctrl-C)
/// - `SignalKind::terminate()` - SIGTERM
/// - `SignalKind::hangup()` - SIGHUP
/// - `SignalKind::quit()` - SIGQUIT
/// - `SignalKind::user_defined1()` - SIGUSR1
/// - `SignalKind::user_defined2()` - SIGUSR2
/// - `SignalKind::child()` - SIGCHLD
/// - `SignalKind::alarm()` - SIGALRM
/// - `SignalKind::pipe()` - SIGPIPE
#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct SignalKind(libc::c_int);

impl SignalKind {
    /// Create a new `SignalKind` from a raw signal number.
    #[inline]
    pub const fn new(raw: libc::c_int) -> Self {
        Self(raw)
    }

    /// Return the raw signal number.
    #[inline]
    pub const fn as_raw(self) -> libc::c_int {
        self.0
    }

    /// SIGINT - interrupt signal (Ctrl-C).
    #[inline]
    pub const fn interrupt() -> Self {
        Self(libc::SIGINT)
    }

    /// SIGTERM - termination signal.
    #[inline]
    pub const fn terminate() -> Self {
        Self(libc::SIGTERM)
    }

    /// SIGHUP - hangup signal.
    #[inline]
    pub const fn hangup() -> Self {
        Self(libc::SIGHUP)
    }

    /// SIGQUIT - quit signal.
    #[inline]
    pub const fn quit() -> Self {
        Self(libc::SIGQUIT)
    }

    /// SIGUSR1 - user-defined signal 1.
    #[inline]
    pub const fn user_defined1() -> Self {
        Self(libc::SIGUSR1)
    }

    /// SIGUSR2 - user-defined signal 2.
    #[inline]
    pub const fn user_defined2() -> Self {
        Self(libc::SIGUSR2)
    }

    /// SIGCHLD - child process terminated or stopped.
    #[inline]
    pub const fn child() -> Self {
        Self(libc::SIGCHLD)
    }

    /// SIGALRM - alarm clock signal.
    #[inline]
    pub const fn alarm() -> Self {
        Self(libc::SIGALRM)
    }

    /// SIGPIPE - write to pipe with no readers.
    #[inline]
    pub const fn pipe() -> Self {
        Self(libc::SIGPIPE)
    }
}

impl From<SignalKind> for libc::c_int {
    #[inline]
    fn from(kind: SignalKind) -> libc::c_int {
        kind.0
    }
}

struct SignalState {
    counter: AtomicUsize,
    wakers: Mutex<slab::Slab<Option<Waker>>>,
}

struct RegisteredSignal {
    state: Arc<SignalState>,
    refs: usize,
    prev_action: libc::sigaction,
}

struct Registry {
    signals: Mutex<HashMap<libc::c_int, RegisteredSignal>>,
    // These live as long as the process-wide registry, including if its dispatch
    // thread exits. An installed handler must never write to a recycled fd.
    read_fd: OwnedFd,
    write_fd: OwnedFd,
}

static REGISTRY: OnceCell<Arc<Registry>> = OnceCell::new();
static SIGNAL_WRITE_FD: AtomicI32 = AtomicI32::new(-1);
static PENDING_SIGNALS: PendingSignals = PendingSignals::new();

// Covers signal numbers on supported Linux and Apple targets, including Linux
// real-time signals. Reject larger values at registration rather than dropping
// them silently in the handler.
const SIGNAL_SLOTS: usize = 128;

struct PendingSignals([AtomicBool; SIGNAL_SLOTS]);

impl PendingSignals {
    const fn new() -> Self {
        Self([const { AtomicBool::new(false) }; SIGNAL_SLOTS])
    }

    fn notify(&self, fd: RawFd, signum: libc::c_int) {
        if let Some(slot) = self.0.get(signum as usize) {
            // Publish before writing. If the pipe is full, a queued wake already
            // ensures the dispatcher will scan this pending notification.
            slot.store(true, Ordering::SeqCst);
            write_signal_notification(fd, signum);
        }
    }

    fn take(&self, signum: usize) -> bool {
        self.0[signum].swap(false, Ordering::SeqCst)
    }
}

/// Async signal listener for a specific Unix signal.
///
/// This type listens for occurrences of a specific signal. Multiple `Signal`
/// instances can be created for the same signal kind; all will be woken when
/// the signal is received.
///
/// # Examples
/// See "Registering and cancelling signal waits" in
/// `tools/vibeio-check/EXAMPLES.md` for an executable SIGTERM registration and
/// receive-timeout example. Dropping a receive future alone does not unregister
/// a retained listener; drop the listener when it is no longer needed.
pub struct Signal {
    kind: SignalKind,
    state: Arc<SignalState>,
    last_seen: usize,
    waker_slot: usize,
}

impl Signal {
    /// Register for a Unix signal.
    ///
    /// Creates a new signal listener for the given signal kind. If this is the
    /// first listener for this signal, the signal handler will be installed.
    pub fn new(kind: SignalKind) -> io::Result<Self> {
        let state = register_signal(kind)?;
        let last_seen = state.counter.load(Ordering::Acquire);
        let waker_slot = state.wakers.lock().unwrap().insert(None);
        Ok(Self {
            kind,
            state,
            last_seen,
            waker_slot,
        })
    }

    /// Returns the signal kind being listened to.
    #[inline]
    pub fn kind(&self) -> SignalKind {
        self.kind
    }

    /// Wait for the next occurrence of the signal.
    ///
    /// This method returns a future that resolves when the signal is received.
    /// Multiple listeners for the same signal will all be woken on each signal.
    /// Repeated occurrences may coalesce before dispatch; this API does not
    /// preserve exact counts or ordering, including for real-time signal kinds.
    pub async fn recv(&mut self) -> io::Result<()> {
        poll_fn(|cx| self.poll_recv(cx)).await
    }

    fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        // Serialize checking the counter and registering with dispatcher wakeup.
        // Each listener owns one slot so its drop can release its waker.
        let mut replacement = None;
        loop {
            let mut wakers = self.state.wakers.lock().unwrap();
            let current = self.state.counter.load(Ordering::Acquire);
            if current != self.last_seen {
                self.last_seen = current;
                let retired = wakers[self.waker_slot].take();
                drop(wakers);
                drop(retired);
                return Poll::Ready(Ok(()));
            }
            let slot = &mut wakers[self.waker_slot];
            if slot
                .as_ref()
                .is_some_and(|waker| waker.will_wake(cx.waker()))
            {
                return Poll::Pending;
            }
            if let Some(replacement) = replacement.take() {
                let retired = slot.replace(replacement);
                drop(wakers);
                drop(retired);
                return Poll::Pending;
            }
            // RawWaker clone callbacks are user code and may reenter this state.
            // Recheck the counter after reacquiring the lock: a signal may arrive
            // while cloning, before this listener has installed its new waker.
            drop(wakers);
            replacement = Some(cx.waker().clone());
        }
    }
}

impl Drop for Signal {
    fn drop(&mut self) {
        let retired = self.state.wakers.lock().unwrap().remove(self.waker_slot);
        drop(retired);
        unregister_signal(self.kind);
    }
}

/// Convenience builder for Unix signals.
///
/// This is a wrapper around `Signal::new()` that provides a more ergonomic API.
#[inline]
pub fn signal(kind: SignalKind) -> io::Result<Signal> {
    Signal::new(kind)
}

/// Cross-platform Ctrl-C future (Unix implementation uses SIGINT).
///
/// This type provides a future that resolves when Ctrl-C (SIGINT) is received.
/// On Unix, this is implemented as a `Signal` for `SignalKind::interrupt()`.
pub struct CtrlC {
    signal: Signal,
}

impl CtrlC {
    /// Create a new Ctrl-C listener.
    pub fn new() -> io::Result<Self> {
        Ok(Self {
            signal: Signal::new(SignalKind::interrupt())?,
        })
    }
}

impl Future for CtrlC {
    type Output = io::Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        this.signal.poll_recv(cx)
    }
}

/// Cross-platform Ctrl-C support.
///
/// Returns a future that resolves when Ctrl-C is received.
#[inline]
pub fn ctrl_c() -> io::Result<CtrlC> {
    CtrlC::new()
}

fn register_signal(kind: SignalKind) -> io::Result<Arc<SignalState>> {
    if !(1..SIGNAL_SLOTS as libc::c_int).contains(&kind.0) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "unsupported signal number",
        ));
    }
    let registry = registry()?;
    let mut signals = registry.signals.lock().unwrap();
    if let Some(entry) = signals.get_mut(&kind.0) {
        entry.refs += 1;
        return Ok(entry.state.clone());
    }

    let prev_action = install_handler(kind.0)?;
    let state = Arc::new(SignalState {
        counter: AtomicUsize::new(0),
        wakers: Mutex::new(slab::Slab::new()),
    });

    signals.insert(
        kind.0,
        RegisteredSignal {
            state: state.clone(),
            refs: 1,
            prev_action,
        },
    );

    Ok(state)
}

fn unregister_signal(kind: SignalKind) {
    let Some(registry) = REGISTRY.get() else {
        return;
    };
    let mut signals = registry.signals.lock().unwrap();
    let Some(entry) = signals.get_mut(&kind.0) else {
        return;
    };

    if entry.refs > 1 {
        entry.refs -= 1;
        return;
    }
    // Serialize restoration with new registrations. Restoring after unlocking
    // could overwrite a newly installed handler for this same signal.
    // SAFETY: prev_action was returned by sigaction for this signal.
    let restored = unsafe { restore_handler(kind.0, &entry.prev_action) };
    if restored.is_ok() {
        signals.remove(&kind.0);
    } else {
        // Preserve the original disposition for a later restoration attempt.
        entry.refs = 0;
    }
}

fn registry() -> io::Result<&'static Arc<Registry>> {
    REGISTRY.get_or_try_init(init_registry)
}

fn init_registry() -> io::Result<Arc<Registry>> {
    init_registry_with_start(start_dispatch_thread)
}

fn init_registry_with_start(
    start: impl FnOnce(Arc<Registry>) -> io::Result<()>,
) -> io::Result<Arc<Registry>> {
    let (read_fd, write_fd) = create_pipe()?;
    let registry = Arc::new(Registry {
        signals: Mutex::new(HashMap::new()),
        read_fd,
        write_fd,
    });

    start(Arc::clone(&registry))?;
    // Publish only after successful startup. On spawn failure both owned ends
    // are closed, and no signal handler can observe a stale descriptor.
    SIGNAL_WRITE_FD.store(registry.write_fd.as_raw_fd(), Ordering::Release);
    Ok(registry)
}

fn start_dispatch_thread(registry: Arc<Registry>) -> io::Result<()> {
    std::thread::Builder::new()
        .name("vibeio-signal-dispatch".to_string())
        .spawn(move || dispatch_loop(registry))
        .map_err(io::Error::other)?;
    Ok(())
}

fn dispatch_loop(registry: Arc<Registry>) {
    let read_fd = registry.read_fd.as_raw_fd();
    let mut buf = [0u8; 128];
    loop {
        // SAFETY: the registry owns read_fd and buf is writable for buf.len().
        // Only this dedicated thread blocks; the handler's write end is nonblocking.
        let n = unsafe { libc::read(read_fd, buf.as_mut_ptr().cast::<libc::c_void>(), buf.len()) };
        if n == 0 {
            return;
        }
        if n < 0 {
            let err = io::Error::last_os_error();
            match err.kind() {
                io::ErrorKind::Interrupted => continue,
                _ => return,
            }
        }

        // Pipe data is only a wakeup; authoritative state lives in atomics.
        // Repeated occurrences may coalesce, as they do for standard Unix signals.
        for signum in 1..SIGNAL_SLOTS {
            if PENDING_SIGNALS.take(signum) {
                dispatch_signal(&registry, signum as libc::c_int);
            }
        }
    }
}

fn dispatch_signal(registry: &Registry, signum: libc::c_int) {
    let state = {
        let signals = registry.signals.lock().unwrap();
        signals.get(&signum).map(|entry| entry.state.clone())
    };

    if let Some(state) = state {
        state.counter.fetch_add(1, Ordering::Release);
        let wakers = {
            let mut wakers = state.wakers.lock().unwrap();
            wakers
                .iter_mut()
                .filter_map(|(_, slot)| slot.take())
                .collect::<Vec<_>>()
        };
        for waker in wakers {
            waker.wake();
        }
    }
}

extern "C" fn signal_handler(signum: libc::c_int) {
    let fd = SIGNAL_WRITE_FD.load(Ordering::Relaxed);
    if fd < 0 {
        return;
    }

    PENDING_SIGNALS.notify(fd, signum);
}

fn write_signal_notification(fd: RawFd, signum: libc::c_int) {
    // errno's Unix accessors directly access the platform thread-local errno;
    // no allocation, locking or formatting may occur on this handler path.
    let saved_errno = errno::errno();
    let bytes = signum.to_ne_bytes();
    loop {
        // SAFETY: bytes is initialized for its full length. write is async-signal-safe;
        // the registry owns the nonblocking descriptor for the process lifetime.
        let result = unsafe { libc::write(fd, bytes.as_ptr().cast::<libc::c_void>(), bytes.len()) };
        if result >= 0 || errno::errno().0 != libc::EINTR {
            break;
        }
    }
    errno::set_errno(saved_errno);
}

fn install_handler(signum: libc::c_int) -> io::Result<libc::sigaction> {
    // SAFETY: sigaction's C fields admit zero initialization. The handler,
    // flags and mask are set below before the structure is passed to the OS.
    let mut action: libc::sigaction = unsafe { std::mem::zeroed() };
    action.sa_sigaction = signal_handler as extern "C" fn(libc::c_int) as usize;
    action.sa_flags = libc::SA_RESTART;
    // SAFETY: sa_mask is an aligned, writable sigset_t field owned locally.
    if unsafe { libc::sigemptyset(&mut action.sa_mask) } == -1 {
        return Err(io::Error::last_os_error());
    }

    // SAFETY: sigaction admits zero initialization; the OS fills this output.
    let mut prev: libc::sigaction = unsafe { std::mem::zeroed() };
    // SAFETY: action contains our process-lifetime C ABI handler and initialized
    // mask. Both structures are valid for the call; invalid signal numbers are
    // reported by the OS rather than used as memory addresses.
    let rc = unsafe { libc::sigaction(signum, &action, &mut prev) };
    if rc == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(prev)
}

/// # Safety
/// `prev` must be the action saved for this signal, with any handler it refers
/// to still valid for subsequent signal delivery.
unsafe fn restore_handler(signum: libc::c_int, prev: &libc::sigaction) -> io::Result<()> {
    // SAFETY: the caller supplies the previous action returned by sigaction;
    // its handler and flags are restored unchanged. No old-action output is requested.
    let rc = unsafe { libc::sigaction(signum, prev, std::ptr::null_mut()) };
    if rc == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn create_pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let (reader, writer) = std::io::pipe()?;
    crate::vibeio::fd_inner::set_nonblocking(writer.as_raw_fd(), true)?;
    Ok((reader.into(), writer.into()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vibeio::driver::AnyDriver;

    #[test]
    fn waker_clone_runs_unlocked_and_rechecks_notifications() {
        use std::task::{RawWaker, RawWakerVTable};
        struct Probe {
            state: Arc<SignalState>,
            notify: bool,
            clones: AtomicUsize,
            callback_locked: AtomicBool,
        }
        unsafe fn clone(data: *const ()) -> RawWaker {
            // SAFETY: each raw waker owns one Arc<Probe> reference.
            let probe = unsafe { &*data.cast::<Probe>() };
            if probe.state.wakers.try_lock().is_err() {
                probe.callback_locked.store(true, Ordering::Relaxed);
            }
            probe.clones.fetch_add(1, Ordering::Relaxed);
            if probe.notify {
                probe.state.counter.fetch_add(1, Ordering::Release);
            }
            // SAFETY: the source waker retains a live Arc for this callback.
            unsafe { Arc::increment_strong_count(data.cast::<Probe>()) };
            RawWaker::new(data, &VTABLE)
        }
        unsafe fn release(data: *const ()) {
            // SAFETY: consume exactly the reference owned by this raw waker.
            let probe = unsafe { Arc::from_raw(data.cast::<Probe>()) };
            if probe.state.wakers.try_lock().is_err() {
                probe.callback_locked.store(true, Ordering::Relaxed);
            }
        }
        unsafe fn wake_by_ref(_: *const ()) {}
        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, release, wake_by_ref, release);

        for notify in [false, true] {
            let state = Arc::new(SignalState {
                counter: AtomicUsize::new(0),
                wakers: Mutex::new(slab::Slab::new()),
            });
            let waker_slot = state.wakers.lock().unwrap().insert(None);
            // No OS handler is installed; this isolated state exercises polling
            // without racing process-global signal tests.
            let mut signal = Signal {
                kind: SignalKind::new(-1),
                state: state.clone(),
                last_seen: 0,
                waker_slot,
            };
            let probe = Arc::new(Probe {
                state: state.clone(),
                notify,
                clones: AtomicUsize::new(0),
                callback_locked: AtomicBool::new(false),
            });
            // SAFETY: the vtable retains/releases Arc ownership, and Probe's
            // shared state is synchronized for thread-safe waker callbacks.
            let waker = unsafe {
                Waker::from_raw(RawWaker::new(Arc::into_raw(probe.clone()).cast(), &VTABLE))
            };
            let mut cx = Context::from_waker(&waker);
            if notify {
                assert!(matches!(signal.poll_recv(&mut cx), Poll::Ready(Ok(()))));
                assert!(state.wakers.lock().unwrap()[waker_slot].is_none());
            } else {
                assert!(signal.poll_recv(&mut cx).is_pending());
                assert!(signal.poll_recv(&mut cx).is_pending());
                assert_eq!(
                    probe.clones.load(Ordering::Relaxed),
                    1,
                    "unchanged waker should not be cloned again"
                );
            }
            drop(signal);
            drop(waker);
            assert!(state.wakers.lock().unwrap().is_empty());
            assert_eq!(Arc::strong_count(&probe), 1);
            assert!(
                !probe.callback_locked.load(Ordering::Relaxed),
                "waker callback ran under signal lock"
            );
        }
    }

    #[test]
    fn full_pipe_preserves_distinct_pending_signals() {
        use std::io::{Read, Write};
        let pending = PendingSignals::new();
        let (read_fd, write_fd) = create_pipe().unwrap();
        let mut writer = std::fs::File::from(write_fd);
        loop {
            match writer.write(&[0; 4]) {
                Ok(4) => {}
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                result => panic!("unexpected pipe fill result: {result:?}"),
            }
        }
        let saved = errno::errno();
        pending.notify(writer.as_raw_fd(), libc::SIGUSR1);
        pending.notify(writer.as_raw_fd(), libc::SIGUSR2);
        pending.notify(writer.as_raw_fd(), libc::SIGUSR1);
        assert_eq!(errno::errno(), saved);

        // Consume a pre-existing pipe wake, then scan exactly as dispatch does.
        let mut reader = std::fs::File::from(read_fd);
        reader.read_exact(&mut [0; 4]).unwrap();
        let received = (1..SIGNAL_SLOTS)
            .filter(|&signum| pending.take(signum))
            .collect::<Vec<_>>();
        let mut expected = vec![libc::SIGUSR1 as usize, libc::SIGUSR2 as usize];
        expected.sort_unstable();
        assert_eq!(received, expected);
        assert!(!(1..SIGNAL_SLOTS).any(|signum| pending.take(signum)));
        // A subsequent occurrence is not suppressed by a previous delivery.
        pending.notify(writer.as_raw_fd(), libc::SIGUSR1);
        assert!(pending.take(libc::SIGUSR1 as usize));
    }

    #[test]
    fn invalid_signal_numbers_are_rejected() {
        for signum in [-1, 0, SIGNAL_SLOTS as i32, i32::MAX] {
            assert!(matches!(Signal::new(SignalKind::new(signum)),
                Err(err) if err.kind() == io::ErrorKind::InvalidInput));
        }
    }

    #[test]
    fn failed_handler_installation_leaves_no_registration() {
        for signum in [libc::SIGKILL, libc::SIGSTOP] {
            for _ in 0..2 {
                let error = match Signal::new(SignalKind::new(signum)) {
                    Ok(_) => panic!("uncatchable signal unexpectedly registered"),
                    Err(error) => error,
                };
                assert_eq!(error.raw_os_error(), Some(libc::EINVAL));
                assert!(
                    !registry()
                        .unwrap()
                        .signals
                        .lock()
                        .unwrap()
                        .contains_key(&signum)
                );
            }
        }
    }

    #[test]
    fn notification_write_preserves_errno_on_success_and_failure() {
        use std::io::{Read, Write};
        let original = errno::errno();
        let (read_fd, write_fd) = create_pipe().unwrap();
        errno::set_errno(errno::Errno(libc::EDOM));
        write_signal_notification(write_fd.as_raw_fd(), 42);
        let after_success = errno::errno();
        let mut reader = std::fs::File::from(read_fd);
        let mut bytes = [0; 4];
        reader.read_exact(&mut bytes).unwrap();
        assert_eq!(i32::from_ne_bytes(bytes), 42);
        assert_eq!(after_success, errno::Errno(libc::EDOM));

        let mut writer = std::fs::File::from(write_fd);
        loop {
            match writer.write(&[0; 4]) {
                Ok(4) => {}
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                result => panic!("unexpected pipe fill result: {result:?}"),
            }
        }
        errno::set_errno(errno::Errno(libc::ERANGE));
        write_signal_notification(writer.as_raw_fd(), 42);
        let after_full_pipe = errno::errno();
        errno::set_errno(errno::Errno(libc::EDOM));
        write_signal_notification(-1, 42);
        let after_invalid_fd = errno::errno();
        errno::set_errno(original);
        assert_eq!(after_full_pipe, errno::Errno(libc::ERANGE));
        assert_eq!(after_invalid_fd, errno::Errno(libc::EDOM));
    }

    #[test]
    fn startup_failure_releases_registry_and_pipe_owners() {
        let mut weak = std::sync::Weak::new();
        let result = init_registry_with_start(|registry| {
            weak = Arc::downgrade(&registry);
            Err(io::Error::other("injected thread startup failure"))
        });
        assert!(result.is_err());
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn pipe_modes_and_owned_endpoint_cleanup() {
        use std::io::{Read, Write};
        let (read_fd, write_fd) = create_pipe().unwrap();
        for fd in [&read_fd, &write_fd] {
            // SAFETY: fd is owned and F_GETFD has no pointer arguments.
            let flags = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_GETFD) };
            assert_ne!(flags, -1);
            assert_ne!(flags & libc::FD_CLOEXEC, 0);
        }
        // SAFETY: both descriptors are owned, and F_GETFL only queries flags.
        let read_flags = unsafe { libc::fcntl(read_fd.as_raw_fd(), libc::F_GETFL) };
        // SAFETY: as above, querying the distinct owned write descriptor.
        let write_flags = unsafe { libc::fcntl(write_fd.as_raw_fd(), libc::F_GETFL) };
        assert_ne!(read_flags, -1);
        assert_ne!(write_flags, -1);
        assert_eq!(read_flags & libc::O_NONBLOCK, 0);
        assert_ne!(write_flags & libc::O_NONBLOCK, 0);
        let mut reader = std::fs::File::from(read_fd);
        let mut writer = std::fs::File::from(write_fd);
        writer.write_all(&42i32.to_ne_bytes()).unwrap();
        let mut bytes = [0; 4];
        reader.read_exact(&mut bytes).unwrap();
        assert_eq!(i32::from_ne_bytes(bytes), 42);
        drop(writer);
        assert_eq!(reader.read(&mut bytes).unwrap(), 0);
    }

    struct WakeCounter(AtomicUsize);
    impl std::task::Wake for WakeCounter {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn listeners_replace_and_release_wakers_independently() {
        let mut first = Signal::new(SignalKind::user_defined2()).unwrap();
        let mut second = Signal::new(SignalKind::user_defined2()).unwrap();
        let state = first.state.clone();
        let old = Arc::new(WakeCounter(AtomicUsize::new(0)));
        let new = Arc::new(WakeCounter(AtomicUsize::new(0)));
        let old_waker = Waker::from(old.clone());
        let new_waker = Waker::from(new.clone());
        assert!(
            first
                .poll_recv(&mut Context::from_waker(&old_waker))
                .is_pending()
        );
        assert!(
            second
                .poll_recv(&mut Context::from_waker(&old_waker))
                .is_pending()
        );
        assert_eq!(Arc::strong_count(&old), 4);
        assert!(
            first
                .poll_recv(&mut Context::from_waker(&new_waker))
                .is_pending()
        );
        assert_eq!(Arc::strong_count(&old), 3);
        drop(second);
        assert_eq!(Arc::strong_count(&old), 2);
        assert_eq!(state.wakers.lock().unwrap().len(), 1);
        dispatch_signal(registry().unwrap(), SignalKind::user_defined2().as_raw());
        assert_eq!(new.0.load(Ordering::Relaxed), 1);
        assert!(
            first
                .poll_recv(&mut Context::from_waker(&new_waker))
                .is_ready()
        );
        assert_eq!(Arc::strong_count(&new), 2);
        drop(first);
        assert!(state.wakers.lock().unwrap().is_empty());
    }

    async fn receive_signal(
        future: impl Future<Output = io::Result<()>>,
        signum: libc::c_int,
    ) -> io::Result<()> {
        let pid = std::process::id() as libc::pid_t;
        crate::vibeio::test_support::notify_after_pending(future, move || {
            // SAFETY: the listener is installed in this isolated, live test process.
            assert_eq!(unsafe { libc::kill(pid, signum) }, 0);
        })
        .await
    }

    #[test]
    fn signal_recv_unblocks() {
        if crate::vibeio::test_support::isolated_signal_test() {
            return;
        }
        let rt = crate::vibeio::executor::Runtime::new(AnyDriver::new_mock());
        let result = rt.block_on(async {
            let mut sig = signal(SignalKind::user_defined1())?;
            receive_signal(sig.recv(), SignalKind::user_defined1().as_raw()).await
        });
        assert!(result.is_ok());
    }

    #[test]
    fn ctrl_c_unblocks_on_sigint() {
        if crate::vibeio::test_support::isolated_signal_test() {
            return;
        }
        let rt = crate::vibeio::executor::Runtime::new(AnyDriver::new_mock());
        let result = rt.block_on(async {
            let ctrlc = ctrl_c()?;

            receive_signal(ctrlc, SignalKind::interrupt().as_raw()).await
        });
        assert!(result.is_ok());
    }
}
