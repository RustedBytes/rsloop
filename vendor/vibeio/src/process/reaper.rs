use std::io::{self};
use std::process::ExitStatus;
use std::task::{Context, Poll, Waker};

use futures_util::FutureExt;
#[cfg(windows)]
use windows_sys::Win32::Foundation::INVALID_HANDLE_VALUE;

#[cfg(windows)]
use std::os::windows::io::AsRawHandle;

use crate::current_zombie_reaper;

pub(crate) struct ZombieReaper;

pub(crate) type ZombieReaperMessage = (
    std::process::Child,
    Option<oneshot::Sender<std::io::Result<ExitStatus>>>,
);

/// Zombie reaper process that waits on child processes asynchronously.
impl ZombieReaper {
    /// Creates a new zombie reaper instance.
    #[inline]
    pub(crate) fn new() -> Self {
        Self
    }

    /// Waits on a child process asynchronously.
    #[inline]
    pub(crate) async fn wait(&self, mut child: std::process::Child) -> io::Result<ExitStatus> {
        if let Some(reaper_send) = current_zombie_reaper().await {
            // Send the child to the zombie reaper to wait on it asynchronously.
            let (sender, recver) = oneshot::async_channel();
            let _ = reaper_send.send((child, Some(sender))).await;
            recver
                .await
                .map_err(|_| std::io::Error::other("zombie reaper error"))?
        } else {
            child.wait()
        }
    }

    /// Reaps a child process on drop, waiting asynchronously if possible.
    #[inline]
    pub(crate) fn reap_on_drop(&self, mut child: std::process::Child) {
        if let Ok(Some(_)) = child.try_wait() {
            return;
        }
        if let Poll::Ready(Some(reaper_send)) =
            Box::pin(current_zombie_reaper()).poll_unpin(&mut Context::from_waker(Waker::noop()))
        {
            // Send the child to the zombie reaper, so it can wait on it asynchronously.
            let (sender, _) = oneshot::async_channel();
            let _ = reaper_send.try_send((child, Some(sender)));
        } else {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
        }
    }
}

#[inline]
pub(crate) async fn start_zombie_reaper() -> async_channel::Sender<ZombieReaperMessage> {
    let (tx, rx) = async_channel::unbounded();
    crate::spawn(zombie_reaper_fn(rx));
    tx
}

// ---------------------------------------------------------------------------
// Windows reaper implementation
// ---------------------------------------------------------------------------

#[cfg(windows)]
struct WaitContext {
    child: std::process::Child,
    sender: Option<oneshot::Sender<std::io::Result<ExitStatus>>>,
    wait_handle: windows_sys::Win32::Foundation::HANDLE,
}

// SAFETY: `wait_handle` isn't used after the callback returns, so it's safe to send across threads.
#[cfg(windows)]
unsafe impl Send for WaitContext {}

#[cfg(windows)]
unsafe extern "system" fn wait_callback(ctx: *mut std::ffi::c_void, _timed_out: bool) {
    let mut ctx = Box::from_raw(ctx as *mut WaitContext);
    let result = ctx.child.wait();
    if let Some(sender) = ctx.sender.take() {
        let _ = sender.send(result);
    }
    // WT_EXECUTEONLYONCE unregisters the wait after this callback returns.
}

#[inline]
#[cfg(windows)]
async fn zombie_reaper_fn(rx: async_channel::Receiver<ZombieReaperMessage>) {
    use windows_sys::Win32::System::Threading::{
        RegisterWaitForSingleObject, INFINITE, WT_EXECUTEONLYONCE,
    };

    while let Ok((mut child, sender)) = rx.recv().await {
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

        let ctx = Box::new(WaitContext {
            child,
            sender,
            wait_handle: INVALID_HANDLE_VALUE,
        });
        let process_handle = ctx.child.as_raw_handle();
        let ctx_ptr = Box::into_raw(ctx);

        let ok = unsafe {
            RegisterWaitForSingleObject(
                &mut (*ctx_ptr).wait_handle,
                process_handle,
                Some(wait_callback),
                ctx_ptr as *mut std::ffi::c_void,
                INFINITE,
                WT_EXECUTEONLYONCE,
            )
        };

        if ok == 0 {
            // Fall back to a dedicated thread if registration fails.
            let mut ctx = unsafe { Box::from_raw(ctx_ptr) };
            std::thread::spawn(move || {
                let result = ctx.child.wait();
                if let Some(sender) = ctx.sender.take() {
                    let _ = sender.send(result);
                }
            });
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
    // Try opening a pidfd for our own PID — it will succeed on 5.3+ and fail
    // with ENOSYS on older kernels.
    let fd = unsafe { libc::syscall(libc::SYS_pidfd_open, libc::getpid(), 0 as libc::c_uint) };
    if fd >= 0 {
        unsafe {
            libc::close(fd as libc::c_int);
        }
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

    use crate::op::{Op, WaitPidOp};

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
        crate::spawn(async move {
            let mut op = WaitPidOp::new(pid);
            let result = poll_fn(|cx| {
                // The WaitPidOp uses poll-based I/O internally (pidfd registered
                // in Poll mode), so we always go through poll_poll regardless of
                // whether the driver supports completions.
                op.poll(cx, &crate::executor::current_driver().expect("no driver"))
            })
            .await;

            let status = result.map(exit_status_from_raw);
            if let Some(sender) = sender {
                let _ = sender.send(status);
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

    let mut signal = crate::signal::Signal::new(crate::signal::SignalKind::child())
        .expect("cannot create signal handler for zombie reaper");
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
                    break;
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
}

#[inline]
#[cfg(all(unix, not(feature = "signal")))]
async fn zombie_reaper_fn_unix(rx: async_channel::Receiver<ZombieReaperMessage>) {
    while let Ok(mut msg) = rx.recv().await {
        crate::spawn(async move {
            crate::spawn_blocking(move || {
                let result = msg.0.wait();
                if let Some(sender) = msg.1 {
                    let _ = sender.send(result);
                }
            })
            .await
        });
    }
}
