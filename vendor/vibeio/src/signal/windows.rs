//! Windows signal handling implementation for `vibeio`.
//!
//! This module provides Ctrl-C support for Windows systems using
//! `SetConsoleCtrlHandler`.
//!
//! # Implementation details
//! - Ctrl-C events are handled via the Windows console control handler.
//! - The handler updates a counter and wakes registered wakers.
//! - Only Ctrl-C is supported on Windows (no arbitrary signals).

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use once_cell::sync::OnceCell;
use windows_sys::Win32::System::Console::{SetConsoleCtrlHandler, CTRL_C_EVENT};

struct CtrlCState {
    counter: AtomicUsize,
    wakers: Mutex<Vec<Waker>>,
}

static CTRL_C_STATE: OnceCell<Arc<CtrlCState>> = OnceCell::new();

/// Cross-platform Ctrl-C future (Windows implementation).
///
/// This type provides a future that resolves when Ctrl-C is received.
/// On Windows, this is implemented via `SetConsoleCtrlHandler`.
pub struct CtrlC {
    state: Arc<CtrlCState>,
    last_seen: usize,
}

impl CtrlC {
    /// Create a new Ctrl-C listener.
    pub fn new() -> io::Result<Self> {
        let state = ctrl_c_state()?.clone();
        let last_seen = state.counter.load(Ordering::Acquire);
        Ok(Self { state, last_seen })
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

impl Future for CtrlC {
    type Output = io::Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // SAFETY: CtrlC is not self-referential.
        let this = unsafe { self.get_unchecked_mut() };
        this.poll_recv(cx)
    }
}

/// Cross-platform Ctrl-C support.
///
/// Returns a future that resolves when Ctrl-C is received.
#[inline]
pub fn ctrl_c() -> io::Result<CtrlC> {
    CtrlC::new()
}

fn register_waker(state: &CtrlCState, waker: &Waker) {
    let mut wakers = state.wakers.lock().unwrap();
    if let Some(existing) = wakers.iter_mut().find(|existing| existing.will_wake(waker)) {
        *existing = waker.clone();
    } else {
        wakers.push(waker.clone());
    }
}

fn ctrl_c_state() -> io::Result<&'static Arc<CtrlCState>> {
    CTRL_C_STATE.get_or_try_init(|| {
        let state = Arc::new(CtrlCState {
            counter: AtomicUsize::new(0),
            wakers: Mutex::new(Vec::new()),
        });

        let ok = unsafe { SetConsoleCtrlHandler(Some(ctrl_c_handler), 1) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(state)
    })
}

unsafe extern "system" fn ctrl_c_handler(ctrl_type: u32) -> i32 {
    if ctrl_type == CTRL_C_EVENT {
        if let Some(state) = CTRL_C_STATE.get() {
            state.counter.fetch_add(1, Ordering::Release);
            let wakers = {
                let mut wakers = state.wakers.lock().unwrap();
                std::mem::take(&mut *wakers)
            };
            for waker in wakers {
                waker.wake();
            }
        }
        return 1;
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::driver::AnyDriver;
    use std::time::Duration;

    #[cfg(feature = "time")]
    async fn await_ctrl_c_with_timeout(
        fut: impl Future<Output = io::Result<()>>,
    ) -> io::Result<()> {
        crate::time::timeout(Duration::from_secs(1), fut)
            .await
            .map_err(|_| io::Error::new(io::ErrorKind::TimedOut, "ctrl-c timeout"))?
    }

    #[cfg(not(feature = "time"))]
    async fn await_ctrl_c_with_timeout(
        fut: impl Future<Output = io::Result<()>>,
    ) -> io::Result<()> {
        fut.await
    }

    #[test]
    fn ctrl_c_unblocks_on_handler() {
        let rt = crate::executor::Runtime::new(AnyDriver::new_mock());
        let result = rt.block_on(async {
            let ctrlc = ctrl_c()?;
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(10));
                unsafe {
                    let _ = ctrl_c_handler(CTRL_C_EVENT);
                }
            });
            await_ctrl_c_with_timeout(ctrlc).await
        });
        assert!(result.is_ok());
    }
}
