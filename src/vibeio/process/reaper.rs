#![warn(clippy::undocumented_unsafe_blocks)]

use std::io::{self};
use std::process::ExitStatus;
use std::task::{Context, Poll, Waker};

use futures_util::FutureExt;

#[cfg(windows)]
use std::os::windows::io::AsRawHandle;

use crate::vibeio::current_zombie_reaper;

pub(crate) struct ZombieReaper;

pub(crate) type ZombieReaperMessage = (
    ReapChild,
    Option<oneshot::Sender<std::io::Result<ExitStatus>>>,
);

/// Retains reaping responsibility in queued messages and cancelled futures.
pub(crate) struct ReapChild(Option<std::process::Child>);

impl std::ops::Deref for ReapChild {
    type Target = std::process::Child;
    fn deref(&self) -> &Self::Target {
        self.0.as_ref().expect("child already consumed")
    }
}

impl std::ops::DerefMut for ReapChild {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.0.as_mut().expect("child already consumed")
    }
}

impl Drop for ReapChild {
    fn drop(&mut self) {
        let Some(mut child) = self.0.take() else {
            return;
        };
        if matches!(child.try_wait(), Ok(Some(_))) {
            return;
        }
        reap_with_worker(child, |worker| {
            std::thread::Builder::new()
                .name("vibeio-reap".into())
                .spawn(move || wait_pending_child(&worker))
                .map(drop)
        });
    }
}

type PendingChild = std::sync::Arc<std::sync::Mutex<Option<std::process::Child>>>;

fn wait_pending_child(pending: &PendingChild) {
    // This mutex only transfers ownership; never retain its guard while waiting.
    let child = pending.lock().unwrap().take();
    if let Some(mut child) = child {
        let _ = child.wait();
    }
}

fn reap_with_worker(
    child: std::process::Child,
    start: impl FnOnce(PendingChild) -> io::Result<()>,
) {
    // Keep ownership outside the spawn closure so a thread-creation failure
    // cannot discard an unreaped child. Blocking is a last resort only when
    // the OS cannot create the fallback worker.
    let pending = std::sync::Arc::new(std::sync::Mutex::new(Some(child)));
    if start(pending.clone()).is_err() {
        wait_pending_child(&pending);
    }
}

/// Zombie reaper process that waits on child processes asynchronously.
impl ZombieReaper {
    /// Creates a new zombie reaper instance.
    #[inline]
    pub(crate) fn new() -> Self {
        Self
    }

    /// Waits on a child process asynchronously.
    #[inline]
    pub(crate) async fn wait(&self, child: std::process::Child) -> io::Result<ExitStatus> {
        let child = ReapChild(Some(child));
        let (sender, recver) = oneshot::async_channel();
        if let Some(reaper_send) = current_zombie_reaper().await {
            // Send the child to the zombie reaper to wait on it asynchronously.
            if let Err(err) = reaper_send.try_send((child, Some(sender))) {
                wait_in_background(err.into_inner());
            }
        } else {
            wait_in_background((child, Some(sender)));
        }
        recver
            .await
            .map_err(|_| io::Error::other("zombie reaper error"))?
    }

    /// Reaps a child process on drop, waiting asynchronously if possible.
    #[inline]
    pub(crate) fn reap_on_drop(&self, mut child: std::process::Child) {
        if let Ok(Some(_)) = child.try_wait() {
            return;
        }
        let child = ReapChild(Some(child));
        if let Poll::Ready(Some(reaper_send)) =
            Box::pin(current_zombie_reaper()).poll_unpin(&mut Context::from_waker(Waker::noop()))
        {
            // Send the child to the zombie reaper, so it can wait on it asynchronously.
            // A rejected message drops its guard and starts fallback reaping.
            let _ = reaper_send.try_send((child, None));
        }
    }
}

fn wait_in_background((mut child, sender): ZombieReaperMessage) {
    // On spawn failure, dropping the closure still drops the reaping guard.
    let _ = std::thread::Builder::new()
        .name("vibeio-wait".into())
        .spawn(move || {
            let result = child.wait();
            if let Some(sender) = sender {
                let _ = sender.send(result);
            }
        });
}

#[cfg(all(test, unix))]
mod ownership_tests {
    use super::*;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    fn held_child() -> (std::process::Child, std::process::ChildStdin) {
        let mut child = Command::new("sh")
            .args(["-c", "read line; exit 9"])
            .stdin(Stdio::piped())
            .spawn()
            .unwrap();
        let stdin = child.stdin.take().unwrap();
        (child, stdin)
    }

    #[test]
    fn thread_reaper_delivers_status_without_runtime_pool() {
        let runtime = crate::vibeio::executor::Runtime::with_options(
            crate::vibeio::driver::AnyDriver::new_mio().unwrap(),
            true,
            None,
            false,
            false,
        );
        let (child, stdin) = held_child();
        let (sender, receiver) = oneshot::channel();
        runtime.block_on(async move {
            let (tx, rx) = async_channel::unbounded();
            tx.try_send((ReapChild(Some(child)), Some(sender))).unwrap();
            drop(tx);
            zombie_reaper_fn_threads(rx).await;
        });
        drop(runtime);
        drop(stdin);
        assert_eq!(
            receiver
                .recv_timeout(crate::vibeio::test_support::WATCHDOG)
                .unwrap()
                .unwrap()
                .code(),
            Some(9)
        );
    }

    fn assert_reaped(pid: u32) {
        let deadline = Instant::now() + crate::vibeio::test_support::WATCHDOG;
        loop {
            let mut status = std::mem::MaybeUninit::<libc::siginfo_t>::uninit();
            // SAFETY: status is writable. WNOWAIT observes without reaping, so
            // this test cannot accidentally perform the cleanup it is checking.
            let rc = unsafe {
                libc::waitid(
                    libc::P_PID,
                    pid as _,
                    status.as_mut_ptr(),
                    libc::WEXITED | libc::WNOHANG | libc::WNOWAIT,
                )
            };
            if rc == -1 {
                assert_eq!(
                    io::Error::last_os_error().raw_os_error(),
                    Some(libc::ECHILD)
                );
                return;
            }
            assert!(Instant::now() < deadline, "child {pid} was not reaped");
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn failed_worker_start_preserves_child_for_synchronous_reaping() {
        let (child, stdin) = held_child();
        let pid = child.id();
        reap_with_worker(child, |worker| {
            assert_eq!(worker.lock().unwrap().as_ref().unwrap().id(), pid);
            // Model spawn rejecting and destroying its closure. Release the
            // child's input so the documented synchronous fallback can finish.
            drop(worker);
            drop(stdin);
            Err(io::Error::from(io::ErrorKind::WouldBlock))
        });
        // WNOWAIT in this assertion cannot perform the reaping being tested.
        assert_reaped(pid);
    }

    #[test]
    fn queued_and_rejected_messages_reap_on_drop() {
        for rejected in [false, true] {
            let (child, stdin) = held_child();
            let pid = child.id();
            let (tx, rx) = async_channel::unbounded();
            if rejected {
                tx.close();
            }
            let _ = tx.try_send((
                ReapChild(Some(child)),
                None::<oneshot::Sender<io::Result<ExitStatus>>>,
            ));
            drop(tx);
            drop(rx);
            drop(stdin);
            assert_reaped(pid);
        }
    }

    #[test]
    fn cancelled_wait_after_lazy_reaper_start_reaps_child() {
        let runtime = crate::vibeio::executor::Runtime::new(
            crate::vibeio::driver::AnyDriver::new_mio().unwrap(),
        );
        let (child, stdin) = held_child();
        let pid = child.id();
        runtime.block_on(async move {
            let reaper = ZombieReaper::new();
            let mut wait = Box::pin(reaper.wait(child));
            assert!(
                wait.as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
            drop(wait);
        });
        drop(runtime);
        drop(stdin);
        assert_reaped(pid);
    }

    #[test]
    fn wait_without_runtime_is_nonblocking() {
        use std::future::Future;
        let (child, stdin) = held_child();
        let pid = child.id();
        let reaper = ZombieReaper::new();
        let mut wait = Box::pin(reaper.wait(child));
        assert!(
            wait.as_mut()
                .poll(&mut Context::from_waker(Waker::noop()))
                .is_pending()
        );
        drop(stdin);
        assert_reaped(pid);
        // Reaping precedes delivery by a small interval, so poll under a deadline.
        let deadline = Instant::now() + crate::vibeio::test_support::WATCHDOG;
        loop {
            if let Poll::Ready(status) = wait.as_mut().poll(&mut Context::from_waker(Waker::noop()))
            {
                assert_eq!(status.unwrap().code(), Some(9));
                break;
            }
            assert!(Instant::now() < deadline);
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn runtime_shutdown_reaps_child_in_pidfd_task() {
        let runtime = crate::vibeio::executor::Runtime::new(
            crate::vibeio::driver::AnyDriver::new_mio().unwrap(),
        );
        let (child, stdin) = held_child();
        let pid = child.id();
        runtime.block_on(async move {
            let (tx, rx) = async_channel::unbounded();
            tx.try_send((ReapChild(Some(child)), None)).unwrap();
            // Poll the receiver explicitly: Pending means it consumed the
            // queued child and spawned its wait task, then awaited more input.
            let mut reaper = std::pin::pin!(zombie_reaper_fn_linux_pidfd(rx));
            assert!(
                reaper
                    .as_mut()
                    .poll(&mut Context::from_waker(Waker::noop()))
                    .is_pending()
            );
            drop(tx);
        });
        // Service the spawned wait task while the child is still pipe-held.
        runtime.poll_once();
        drop(runtime);
        drop(stdin);
        assert_reaped(pid);
    }
}

#[inline]
pub(crate) fn start_zombie_reaper() -> async_channel::Sender<ZombieReaperMessage> {
    let (tx, rx) = async_channel::unbounded();
    crate::vibeio::spawn(zombie_reaper_fn(rx));
    tx
}

// ---------------------------------------------------------------------------
// Windows reaper implementation
// ---------------------------------------------------------------------------

#[cfg(windows)]
struct WaitContext {
    message: std::sync::Mutex<ZombieReaperMessage>,
    wait_handle: std::sync::atomic::AtomicPtr<std::ffi::c_void>,
}

#[cfg(windows)]
impl Drop for WaitContext {
    fn drop(&mut self) {
        let handle = self.wait_handle.load(std::sync::atomic::Ordering::Acquire);
        if !handle.is_null() {
            // SAFETY: the last Arc owns this successfully registered wait.
            // There is only one callback, which has relinquished its Arc.
            // NULL requests nonblocking deletion, safe even inside that callback.
            // WT_EXECUTEONLYONCE does not remove the need to unregister.
            unsafe {
                windows_sys::Win32::System::Threading::UnregisterWaitEx(
                    handle,
                    std::ptr::null_mut(),
                );
            }
        }
    }
}

#[cfg(windows)]
unsafe extern "system" fn wait_callback(ctx: *mut std::ffi::c_void, _timed_out: bool) {
    // SAFETY: registration transfers exactly one Arc to this one-shot callback.
    // The registrar keeps another Arc until it has published the wait handle.
    let ctx = unsafe { std::sync::Arc::from_raw(ctx.cast::<WaitContext>()) };
    let mut message = ctx.message.lock().unwrap_or_else(|err| err.into_inner());
    let result = message.0.wait();
    if let Some(sender) = message.1.take() {
        let _ = sender.send(result);
    }
    // Keep the child/process handle alive in the context until registration has
    // returned, even when this callback finishes before RegisterWait returns.
}

#[cfg(windows)]
fn register_process_wait(message: ZombieReaperMessage) {
    use std::sync::{
        Arc, Mutex,
        atomic::{AtomicPtr, Ordering},
    };
    use windows_sys::Win32::System::Threading::{
        INFINITE, RegisterWaitForSingleObject, WT_EXECUTEONLYONCE,
    };

    let process_handle = message.0.as_raw_handle();
    let ctx = Arc::new(WaitContext {
        message: Mutex::new(message),
        wait_handle: AtomicPtr::new(std::ptr::null_mut()),
    });
    let callback_ref = Arc::into_raw(ctx.clone());
    // Output storage is independent of the callback context. An early callback
    // cannot free it, and only this thread publishes the completed registration.
    let mut wait_handle = std::ptr::null_mut();
    // SAFETY: the process is owned by ctx; callback_ref owns a strong reference.
    // No locks are held across registration, which may schedule an early callback.
    let ok = unsafe {
        RegisterWaitForSingleObject(
            &mut wait_handle,
            process_handle,
            Some(wait_callback),
            callback_ref.cast_mut().cast(),
            INFINITE,
            WT_EXECUTEONLYONCE,
        )
    };
    if ok != 0 {
        ctx.wait_handle.store(wait_handle, Ordering::Release);
    } else {
        // SAFETY: failed registration cannot invoke the callback; recover its Arc.
        drop(unsafe { Arc::from_raw(callback_ref) });
        let ctx = Arc::try_unwrap(ctx)
            .ok()
            .expect("failed wait retained callback");
        let mut message = ctx.message.lock().unwrap_or_else(|err| err.into_inner());
        let child = ReapChild(message.0.0.take());
        let sender = message.1.take();
        wait_in_background((child, sender));
    }
}

#[inline]
#[cfg(windows)]
async fn zombie_reaper_fn(rx: async_channel::Receiver<ZombieReaperMessage>) {
    while let Ok((mut child, sender)) = rx.recv().await {
        match child.try_wait() {
            Ok(Some(status)) => {
                if let Some(sender) = sender {
                    let _ = sender.send(Ok(status));
                }
            }
            Ok(None) => register_process_wait((child, sender)),
            Err(err) => {
                if let Some(sender) = sender {
                    let _ = sender.send(Err(err));
                }
            }
        }
    }
}

#[cfg(all(test, windows))]
mod windows_wait_tests {
    use super::*;
    use std::sync::{Arc, Mutex, atomic::AtomicPtr};

    #[test]
    fn early_callback_cannot_destroy_registrar_context() {
        let mut child = std::process::Command::new("cmd")
            .args(["/C", "exit 7"])
            .spawn()
            .unwrap();
        child.wait().unwrap();
        let (sender, receiver) = oneshot::channel();
        let ctx = Arc::new(WaitContext {
            message: Mutex::new((ReapChild(Some(child)), Some(sender))),
            wait_handle: AtomicPtr::new(std::ptr::null_mut()),
        });
        let weak = Arc::downgrade(&ctx);
        let raw = Arc::into_raw(ctx.clone());
        // SAFETY: model exactly one callback with its own transferred Arc.
        unsafe { wait_callback(raw.cast_mut().cast(), false) };
        assert_eq!(receiver.recv().unwrap().unwrap().code(), Some(7));
        assert_eq!(Arc::strong_count(&ctx), 1);
        assert!(weak.upgrade().is_some());
        drop(ctx);
        assert!(weak.upgrade().is_none());
    }

    #[test]
    fn real_process_wait_handles_fast_exit() {
        for _ in 0..32 {
            let child = std::process::Command::new("cmd")
                .args(["/C", "exit 7"])
                .spawn()
                .unwrap();
            let (sender, receiver) = oneshot::channel();
            register_process_wait((ReapChild(Some(child)), Some(sender)));
            let result = receiver
                .recv_timeout(crate::vibeio::test_support::WATCHDOG)
                .unwrap();
            assert_eq!(result.unwrap().code(), Some(7));
        }
    }
}

#[inline]
#[cfg(unix)]
async fn zombie_reaper_fn(rx: async_channel::Receiver<ZombieReaperMessage>) {
    // On Linux, prefer the pidfd-based reaper (no signals needed, works with
    // both mio/epoll and io_uring drivers).  Fall back to the generic Unix
    // implementation if pidfd_open is unavailable (kernel < 5.3).
    #[cfg(target_os = "linux")]
    {
        if pidfd_available() {
            return zombie_reaper_fn_linux_pidfd(rx).await;
        }
    }

    zombie_reaper_fn_unix(rx).await
}

// ---------------------------------------------------------------------------
// Linux pidfd-based reaper (kernel ≥ 5.3)
// ---------------------------------------------------------------------------

/// Probe whether `pidfd_open` is supported on this kernel.
#[cfg(target_os = "linux")]
#[inline]
fn pidfd_available() -> bool {
    use std::os::fd::{FromRawFd, OwnedFd};

    // Try opening a pidfd for our own PID — it will succeed on 5.3+ and fail
    // with ENOSYS on older kernels.
    let pid = std::process::id() as libc::pid_t;
    // SAFETY: pidfd_open takes the current process's integer PID and zero flags,
    // with no pointers. A successful result transfers a fresh descriptor.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, pid, 0 as libc::c_uint) };
    if fd >= 0 {
        // SAFETY: the successful syscall returned a valid, uniquely owned fd.
        let _pidfd = unsafe { OwnedFd::from_raw_fd(fd as libc::c_int) };
        true
    } else {
        let err = io::Error::last_os_error();
        // ENOSYS → syscall not available; anything else means it *is* available
        // but the specific call failed for another reason (shouldn't happen for
        // our own PID, but be safe).
        err.raw_os_error() != Some(libc::ENOSYS)
    }
}

/// Convert a raw `waitpid` status into a `std::process::ExitStatus`.
#[cfg(target_os = "linux")]
#[inline]
fn exit_status_from_raw(raw: i32) -> ExitStatus {
    use std::os::unix::process::ExitStatusExt;
    ExitStatus::from_raw(raw)
}

#[cfg(target_os = "linux")]
#[inline]
async fn zombie_reaper_fn_linux_pidfd(rx: async_channel::Receiver<ZombieReaperMessage>) {
    use std::future::poll_fn;

    use crate::vibeio::op::{Op, WaitPidOp};

    loop {
        let msg = rx.recv().await;
        let Ok((mut child, sender)) = msg else {
            // Channel closed — runtime is shutting down.
            break;
        };

        // Fast path: child may already have exited.
        match child.try_wait() {
            Ok(Some(status)) => {
                if let Some(sender) = sender {
                    let _ = sender.send(Ok(status));
                }
                continue;
            }
            Ok(None) => {}
            Err(err) => {
                if let Some(sender) = sender {
                    let _ = sender.send(Err(err));
                }
                continue;
            }
        }

        let pid = child.id();

        // Spawn a lightweight task per child that waits on the pidfd.
        // This lets us handle many children concurrently without blocking
        // the reaper loop.
        crate::vibeio::spawn(async move {
            // Keep ownership in this task so cancellation still reaps the child.
            let mut child = child;
            let mut op = WaitPidOp::new(pid);
            let result = poll_fn(|cx| {
                // The WaitPidOp uses poll-based I/O internally (pidfd registered
                // in Poll mode), so we always go through poll_poll regardless of
                // whether the driver supports completions.
                op.poll(
                    cx,
                    &crate::vibeio::executor::current_driver().expect("no driver"),
                )
            })
            .await;

            match result {
                Ok(raw) => {
                    // WaitPidOp already reaped it; do not wait on a recycled PID.
                    child.0.take();
                    if let Some(sender) = sender {
                        let _ = sender.send(Ok(exit_status_from_raw(raw)));
                    }
                }
                Err(_) => wait_in_background((child, sender)),
            }
        });
    }
}

// ---------------------------------------------------------------------------
// Generic Unix reaper implementations (non-Linux or kernel < 5.3)
// ---------------------------------------------------------------------------

#[inline]
#[cfg(all(unix, feature = "signal"))]
async fn zombie_reaper_fn_unix(rx: async_channel::Receiver<ZombieReaperMessage>) {
    use futures_util::future::Either;

    let mut signal =
        match crate::vibeio::signal::Signal::new(crate::vibeio::signal::SignalKind::child()) {
            Ok(signal) => signal,
            Err(_) => return zombie_reaper_fn_threads(rx).await,
        };
    let mut processes = Vec::new();
    loop {
        let select = futures_util::future::select(Box::pin(rx.recv()), Box::pin(signal.recv()));
        match select.await {
            Either::Left((process, _)) => {
                let Ok(mut process) = process else {
                    break;
                };
                let try_wait = process.0.try_wait();
                match try_wait {
                    Ok(Some(exit_code)) => {
                        if let Some(sender) = process.1 {
                            let _ = sender.send(Ok(exit_code));
                        }
                    }
                    Ok(None) => processes.push(process),
                    Err(err) => {
                        if let Some(sender) = process.1 {
                            let _ = sender.send(Err(err));
                        }
                    }
                }
            }
            Either::Right((signal, _)) => {
                if signal.is_err() {
                    for process in processes.drain(..) {
                        wait_in_background(process);
                    }
                    return zombie_reaper_fn_threads(rx).await;
                };
                for mut process in processes.split_off(0) {
                    let try_wait = process.0.try_wait();
                    match try_wait {
                        Ok(Some(exit_code)) => {
                            if let Some(sender) = process.1 {
                                let _ = sender.send(Ok(exit_code));
                            }
                        }
                        Ok(None) => processes.push(process),
                        Err(err) => {
                            if let Some(sender) = process.1 {
                                let _ = sender.send(Err(err));
                            }
                        }
                    }
                }
            }
        }
    }
    for process in processes {
        wait_in_background(process);
    }
}

#[inline]
#[cfg(all(unix, not(feature = "signal")))]
async fn zombie_reaper_fn_unix(rx: async_channel::Receiver<ZombieReaperMessage>) {
    zombie_reaper_fn_threads(rx).await;
}

#[cfg(unix)]
async fn zombie_reaper_fn_threads(rx: async_channel::Receiver<ZombieReaperMessage>) {
    // Process support does not require an optional runtime blocking pool.
    while let Ok(msg) = rx.recv().await {
        wait_in_background(msg);
    }
}
