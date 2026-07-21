//! Unix signal handling implementation for `vibeio`.
//!
//! This module provides async signal handling for Unix systems using a
//! dedicated dispatch thread and pipe-based communication.
//!
//! # Implementation details
//! - A background thread reads from a pipe connected to signal handlers.
//! - Signal handlers write the signal number to the pipe.
//! - The dispatch thread wakes registered wakers for received signals.
//! - Multiple listeners for the same signal share the same handler.

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::os::fd::RawFd;
use std::pin::Pin;
use std::sync::atomic::{AtomicI32, AtomicUsize, Ordering};
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
    wakers: Mutex<Vec<Waker>>,
}

struct RegisteredSignal {
    state: Arc<SignalState>,
    refs: usize,
    prev_action: libc::sigaction,
}

struct Registry {
    signals: Mutex<HashMap<libc::c_int, RegisteredSignal>>,
}

static REGISTRY: OnceCell<Arc<Registry>> = OnceCell::new();
static SIGNAL_WRITE_FD: AtomicI32 = AtomicI32::new(-1);

/// Async signal listener for a specific Unix signal.
///
/// This type listens for occurrences of a specific signal. Multiple `Signal`
/// instances can be created for the same signal kind; all will be woken when
/// the signal is received.
///
/// # Examples
/// ```ignore
/// let mut sig = Signal::new(SignalKind::terminate())?;
/// sig.recv().await?;  // Wait for SIGTERM
/// ```
pub struct Signal {
    kind: SignalKind,
    state: Arc<SignalState>,
    last_seen: usize,
}

impl Signal {
    /// Register for a Unix signal.
    ///
    /// Creates a new signal listener for the given signal kind. If this is the
    /// first listener for this signal, the signal handler will be installed.
    pub fn new(kind: SignalKind) -> io::Result<Self> {
        let state = register_signal(kind)?;
        let last_seen = state.counter.load(Ordering::Acquire);
        Ok(Self {
            kind,
            state,
            last_seen,
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
    pub async fn recv(&mut self) -> io::Result<()> {
        poll_fn(|cx| self.poll_recv(cx)).await
    }

    fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let current = self.state.counter.load(Ordering::Acquire);
        if current != self.last_seen {
            self.last_seen = current;
            return Poll::Ready(Ok(()));
        }

        register_waker(&self.state, cx.waker());
        Poll::Pending
    }
}

impl Drop for Signal {
    fn drop(&mut self) {
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
        // SAFETY: CtrlC is not self-referential.
        let this = unsafe { self.get_unchecked_mut() };
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

fn register_waker(state: &SignalState, waker: &Waker) {
    let mut wakers = state.wakers.lock().unwrap();
    if let Some(existing) = wakers.iter_mut().find(|existing| existing.will_wake(waker)) {
        *existing = waker.clone();
    } else {
        wakers.push(waker.clone());
    }
}

fn register_signal(kind: SignalKind) -> io::Result<Arc<SignalState>> {
    let registry = registry()?;
    let mut signals = registry.signals.lock().unwrap();
    if let Some(entry) = signals.get_mut(&kind.0) {
        entry.refs += 1;
        return Ok(entry.state.clone());
    }

    let prev_action = unsafe { install_handler(kind.0)? };
    let state = Arc::new(SignalState {
        counter: AtomicUsize::new(0),
        wakers: Mutex::new(Vec::new()),
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
    let registry = match registry() {
        Ok(registry) => registry,
        Err(_) => return,
    };

    let prev_action = {
        let mut signals = registry.signals.lock().unwrap();
        let entry = match signals.get_mut(&kind.0) {
            Some(entry) => entry,
            None => return,
        };

        if entry.refs > 1 {
            entry.refs -= 1;
            return;
        }

        let prev = std::mem::replace(&mut entry.prev_action, unsafe { std::mem::zeroed() });
        signals.remove(&kind.0);
        prev
    };

    unsafe {
        let _ = restore_handler(kind.0, &prev_action);
    }
}

fn registry() -> io::Result<&'static Arc<Registry>> {
    REGISTRY.get_or_try_init(init_registry)
}

fn init_registry() -> io::Result<Arc<Registry>> {
    let (read_fd, write_fd) = create_pipe()?;
    SIGNAL_WRITE_FD.store(write_fd, Ordering::Release);

    let registry = Arc::new(Registry {
        signals: Mutex::new(HashMap::new()),
    });

    start_dispatch_thread(read_fd, Arc::clone(&registry))?;
    Ok(registry)
}

fn start_dispatch_thread(read_fd: RawFd, registry: Arc<Registry>) -> io::Result<()> {
    std::thread::Builder::new()
        .name("vibeio-signal-dispatch".to_string())
        .spawn(move || dispatch_loop(read_fd, registry))
        .map_err(io::Error::other)?;
    Ok(())
}

fn dispatch_loop(read_fd: RawFd, registry: Arc<Registry>) {
    let mut buf = [0u8; 128];
    let mut pending = Vec::with_capacity(4);
    loop {
        let n = unsafe { libc::read(read_fd, buf.as_mut_ptr().cast::<libc::c_void>(), buf.len()) };
        if n == 0 {
            continue;
        }
        if n < 0 {
            let err = io::Error::last_os_error();
            match err.kind() {
                io::ErrorKind::Interrupted => continue,
                io::ErrorKind::WouldBlock => {
                    std::thread::sleep(std::time::Duration::from_millis(10));
                    continue;
                }
                _ => continue,
            }
        }

        let n = n as usize;
        for byte in &buf[..n] {
            pending.push(*byte);
            if pending.len() == 4 {
                let mut raw = [0u8; 4];
                raw.copy_from_slice(&pending);
                pending.clear();
                let signum = i32::from_ne_bytes(raw) as libc::c_int;
                dispatch_signal(&registry, signum);
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
            std::mem::take(&mut *wakers)
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

    let bytes = signum.to_ne_bytes();
    unsafe {
        let _ = libc::write(fd, bytes.as_ptr().cast::<libc::c_void>(), bytes.len());
    }
}

unsafe fn install_handler(signum: libc::c_int) -> io::Result<libc::sigaction> {
    let mut action: libc::sigaction = std::mem::zeroed();
    action.sa_sigaction = signal_handler as extern "C" fn(libc::c_int) as usize;
    action.sa_flags = libc::SA_RESTART;
    libc::sigemptyset(&mut action.sa_mask);

    let mut prev: libc::sigaction = std::mem::zeroed();
    let rc = libc::sigaction(signum, &action, &mut prev);
    if rc == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(prev)
}

unsafe fn restore_handler(signum: libc::c_int, prev: &libc::sigaction) -> io::Result<()> {
    let rc = libc::sigaction(signum, prev, std::ptr::null_mut());
    if rc == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn create_pipe() -> io::Result<(RawFd, RawFd)> {
    let mut fds = [0; 2];
    #[cfg(syscall_pipe2)]
    let rc = unsafe { libc::pipe2(fds.as_mut_ptr(), libc::O_CLOEXEC | libc::O_NONBLOCK) };
    #[cfg(not(syscall_pipe2))]
    let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
    if rc == -1 {
        return Err(io::Error::last_os_error());
    }

    // On non-Linux, set close-on-exec and non-blocking manually.
    #[cfg(not(syscall_pipe2))]
    {
        if let Err(err) = set_cloexec(fds[0]).and_then(|_| set_cloexec(fds[1])) {
            unsafe {
                let _ = libc::close(fds[0]);
                let _ = libc::close(fds[1]);
            }
            return Err(err);
        }

        if let Err(err) = set_nonblock(fds[1]) {
            unsafe {
                let _ = libc::close(fds[0]);
                let _ = libc::close(fds[1]);
            }
            return Err(err);
        }
    }

    Ok((fds[0], fds[1]))
}

#[cfg(not(target_os = "linux"))]
fn set_cloexec(fd: RawFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    let rc = unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) };
    if rc == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn set_nonblock(fd: RawFd) -> io::Result<()> {
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 {
        return Err(io::Error::last_os_error());
    }
    let rc = unsafe { libc::fcntl(fd, libc::F_SETFL, flags | libc::O_NONBLOCK) };
    if rc == -1 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::AnyDriver;
    use std::time::Duration;

    #[cfg(feature = "time")]
    async fn await_signal_with_timeout(
        fut: impl Future<Output = io::Result<()>>,
    ) -> io::Result<()> {
        crate::time::timeout(Duration::from_secs(1), fut)
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "signal timeout"))?
    }

    #[cfg(not(feature = "time"))]
    async fn await_signal_with_timeout(
        fut: impl Future<Output = io::Result<()>>,
    ) -> io::Result<()> {
        fut.await
    }

    fn spawn_signal_after_delay(signum: libc::c_int) {
        let pid = unsafe { libc::getpid() };
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(10));
            unsafe {
                libc::kill(pid, signum);
            }
        });
    }

    #[test]
    fn signal_recv_unblocks() {
        let rt = crate::executor::Runtime::new(AnyDriver::new_mock());
        let result = rt.block_on(async {
            let mut sig = signal(SignalKind::user_defined1())?;
            spawn_signal_after_delay(SignalKind::user_defined1().as_raw());
            await_signal_with_timeout(sig.recv()).await
        });
        assert!(result.is_ok());
    }

    #[test]
    fn ctrl_c_unblocks_on_sigint() {
        let rt = crate::executor::Runtime::new(AnyDriver::new_mock());
        let result = rt.block_on(async {
            let ctrlc = ctrl_c()?;

            spawn_signal_after_delay(SignalKind::interrupt().as_raw());
            await_signal_with_timeout(ctrlc).await
        });
        assert!(result.is_ok());
    }
}
