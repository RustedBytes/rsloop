//! Windows signal handling implementation for `vibeio`.
//!
//! This module provides Ctrl-C support for Windows systems using
//! `SetConsoleCtrlHandler`.
//!
//! # Implementation details
//! - Ctrl-C events are handled via the Windows console control handler.
//! - The handler updates a counter and wakes registered wakers.
//! - Only Ctrl-C is supported on Windows (no arbitrary signals).

#![deny(unsafe_op_in_unsafe_fn)]
#![warn(clippy::undocumented_unsafe_blocks)]

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, Waker};

use once_cell::sync::OnceCell;
#[cfg(windows)]
use windows_sys::Win32::System::Console::{CTRL_C_EVENT, SetConsoleCtrlHandler};

struct CtrlCState {
    counter: AtomicUsize,
    wakers: Mutex<slab::Slab<Option<Waker>>>,
}

#[cfg(windows)]
static CTRL_C_STATE: OnceCell<Arc<CtrlCState>> = OnceCell::new();
#[cfg(windows)]
static CTRL_C_HANDLER_INSTALLED: OnceCell<()> = OnceCell::new();

/// Cross-platform Ctrl-C future (Windows implementation).
///
/// This type provides a future that resolves when Ctrl-C is received.
/// On Windows, this is implemented via `SetConsoleCtrlHandler`.
pub struct CtrlC {
    state: Arc<CtrlCState>,
    last_seen: usize,
    waker_slot: usize,
}

impl CtrlC {
    /// Create a new Ctrl-C listener.
    #[cfg(windows)]
    pub fn new() -> io::Result<Self> {
        let state = ctrl_c_state()?.clone();
        let last_seen = state.counter.load(Ordering::Acquire);
        let waker_slot = state.wakers.lock().unwrap().insert(None);
        Ok(Self {
            state,
            last_seen,
            waker_slot,
        })
    }

    fn poll_recv(&mut self, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        let mut replacement = None;
        loop {
            // Serialize the notification check and registration with dispatch.
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
            // RawWaker clone callbacks may reenter notification handling.
            // The next iteration observes signals arriving during cloning.
            drop(wakers);
            replacement = Some(cx.waker().clone());
        }
    }
}

impl Drop for CtrlC {
    fn drop(&mut self) {
        // Synchronously retire this slot under the dispatcher lock. Deferring
        // removal could retain a cancelled task's waker indefinitely. User
        // callbacks run after releasing the guard, including the retired drop.
        // qualirs:ignore Q0082
        let retired = self.state.wakers.lock().unwrap().remove(self.waker_slot);
        drop(retired);
    }
}

impl Future for CtrlC {
    type Output = io::Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        self.get_mut().poll_recv(cx)
    }
}

/// Cross-platform Ctrl-C support.
///
/// Returns a future that resolves when Ctrl-C is received.
#[inline]
#[cfg(windows)]
pub fn ctrl_c() -> io::Result<CtrlC> {
    CtrlC::new()
}

fn dispatch_ctrl_c(state: &CtrlCState) {
    let wakers = {
        let mut wakers = state.wakers.lock().unwrap();
        state.counter.fetch_add(1, Ordering::Release);
        wakers
            .iter_mut()
            .filter_map(|(_, slot)| slot.take())
            .collect::<Vec<_>>()
    };
    for waker in wakers {
        waker.wake();
    }
}

#[cfg(windows)]
fn ctrl_c_state() -> io::Result<&'static Arc<CtrlCState>> {
    initialize_ctrl_c_state(&CTRL_C_STATE, &CTRL_C_HANDLER_INSTALLED, || {
        // SAFETY: the process-lifetime callback has the required system ABI;
        // shared state is synchronized and retained in CTRL_C_STATE.
        let ok = unsafe { SetConsoleCtrlHandler(Some(ctrl_c_handler), 1) };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }

        Ok(())
    })
}

fn initialize_ctrl_c_state<'a>(
    state_cell: &'a OnceCell<Arc<CtrlCState>>,
    installed: &OnceCell<()>,
    install: impl FnOnce() -> io::Result<()>,
) -> io::Result<&'a Arc<CtrlCState>> {
    // Publish before installation can expose the callback on another thread.
    // A failed installation leaves reusable state but does not mark success;
    // a later constructor retries installation, serialized by the second cell.
    let state = state_cell.get_or_init(|| {
        Arc::new(CtrlCState {
            counter: AtomicUsize::new(0),
            wakers: Mutex::new(slab::Slab::new()),
        })
    });
    installed.get_or_try_init(install)?;
    Ok(state)
}

#[cfg(windows)]
extern "system" fn ctrl_c_handler(ctrl_type: u32) -> i32 {
    if ctrl_type == CTRL_C_EVENT {
        if let Some(state) = CTRL_C_STATE.get() {
            dispatch_ctrl_c(state);
        }
        return 1;
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initialization_publishes_state_before_callback_and_retries_failure() {
        let state_cell = OnceCell::new();
        let installed = OnceCell::new();
        let failed = initialize_ctrl_c_state(&state_cell, &installed, || {
            assert!(state_cell.get().is_some());
            Err(io::Error::from(io::ErrorKind::PermissionDenied))
        });
        assert!(matches!(failed, Err(error) if error.kind() == io::ErrorKind::PermissionDenied));
        assert!(installed.get().is_none());
        let published = state_cell.get().unwrap().clone();
        let state = initialize_ctrl_c_state(&state_cell, &installed, || {
            // Model the newly registered callback arriving before installation
            // returns, on the actual published state rather than a test copy.
            dispatch_ctrl_c(state_cell.get().expect("callback state not published"));
            Ok(())
        })
        .unwrap();
        assert!(Arc::ptr_eq(state, &published));
        assert_eq!(state.counter.load(Ordering::Acquire), 1);
        assert!(installed.get().is_some());
        let reused = initialize_ctrl_c_state(&state_cell, &installed, || {
            panic!("successful installation must not be repeated")
        })
        .unwrap();
        assert!(Arc::ptr_eq(state, reused));
    }

    fn listener(state: &Arc<CtrlCState>) -> CtrlC {
        CtrlC {
            state: state.clone(),
            last_seen: state.counter.load(Ordering::Acquire),
            waker_slot: state.wakers.lock().unwrap().insert(None),
        }
    }

    #[test]
    fn listener_slots_replace_release_and_broadcast_wakers() {
        struct WakeCount(AtomicUsize);
        impl std::task::Wake for WakeCount {
            fn wake(self: Arc<Self>) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
        let state = Arc::new(CtrlCState {
            counter: AtomicUsize::new(0),
            wakers: Mutex::new(slab::Slab::new()),
        });
        let mut first = listener(&state);
        let mut second = listener(&state);
        let old = Arc::new(WakeCount(AtomicUsize::new(0)));
        let old_waker = Waker::from(old.clone());
        assert!(
            first
                .poll_recv(&mut Context::from_waker(&old_waker))
                .is_pending()
        );
        let current = Arc::new(WakeCount(AtomicUsize::new(0)));
        let waker = Waker::from(current.clone());
        assert!(
            first
                .poll_recv(&mut Context::from_waker(&waker))
                .is_pending()
        );
        assert_eq!(
            Arc::strong_count(&old),
            2,
            "replacement must release old capture"
        );
        assert!(
            second
                .poll_recv(&mut Context::from_waker(&waker))
                .is_pending()
        );
        assert_eq!(state.wakers.lock().unwrap().len(), 2);
        drop(first);
        assert_eq!(state.wakers.lock().unwrap().len(), 1);
        dispatch_ctrl_c(&state);
        assert_eq!(old.0.load(Ordering::Relaxed), 0);
        assert_eq!(current.0.load(Ordering::Relaxed), 1);
        assert!(matches!(
            second.poll_recv(&mut Context::from_waker(&waker)),
            Poll::Ready(Ok(()))
        ));
        assert!(
            second
                .poll_recv(&mut Context::from_waker(&waker))
                .is_pending()
        );
        let mut third = listener(&state);
        assert!(
            third
                .poll_recv(&mut Context::from_waker(&waker))
                .is_pending()
        );
        dispatch_ctrl_c(&state);
        assert_eq!(current.0.load(Ordering::Relaxed), 3);
        drop(second);
        drop(third);
        assert!(state.wakers.lock().unwrap().is_empty());
        assert_eq!(Arc::strong_count(&current), 2);
    }

    #[test]
    fn notification_during_clone_is_observed_without_locked_callbacks() {
        use std::task::{RawWaker, RawWakerVTable};
        unsafe fn clone(data: *const ()) -> RawWaker {
            // SAFETY: every raw waker owns one Arc reference to this state.
            let state = unsafe { &*data.cast::<CtrlCState>() };
            // Fail without deadlocking or poisoning a guard on the old path.
            let unlocked = state.wakers.try_lock().is_ok();
            if unlocked {
                dispatch_ctrl_c(state);
            }
            // SAFETY: the source waker keeps the Arc alive during cloning.
            unsafe { Arc::increment_strong_count(data.cast::<CtrlCState>()) };
            RawWaker::new(data, &VTABLE)
        }
        unsafe fn release(data: *const ()) {
            // SAFETY: consume exactly the Arc reference owned by this waker.
            let state = unsafe { Arc::from_raw(data.cast::<CtrlCState>()) };
            assert!(state.wakers.try_lock().is_ok());
        }
        unsafe fn wake_by_ref(_: *const ()) {}
        static VTABLE: RawWakerVTable = RawWakerVTable::new(clone, release, wake_by_ref, release);
        let state = Arc::new(CtrlCState {
            counter: AtomicUsize::new(0),
            wakers: Mutex::new(slab::Slab::new()),
        });
        let mut ctrl_c = listener(&state);
        // SAFETY: callbacks retain/release Arc ownership and access synchronized
        // state. Neither the state nor its callbacks have thread affinity.
        let waker =
            unsafe { Waker::from_raw(RawWaker::new(Arc::into_raw(state.clone()).cast(), &VTABLE)) };
        assert!(matches!(
            ctrl_c.poll_recv(&mut Context::from_waker(&waker)),
            Poll::Ready(Ok(()))
        ));
        assert!(state.wakers.lock().unwrap()[ctrl_c.waker_slot].is_none());
        drop(ctrl_c);
        drop(waker);
        assert_eq!(Arc::strong_count(&state), 1);
    }
    #[cfg(windows)]
    use crate::vibeio::driver::AnyDriver;
    #[test]
    #[cfg(windows)]
    fn ctrl_c_unblocks_on_handler() {
        let rt = crate::vibeio::executor::Runtime::new(AnyDriver::new_mock());
        let result = rt.block_on(async {
            let ctrlc = ctrl_c()?;
            crate::vibeio::test_support::notify_after_pending(ctrlc, move || {
                let _ = ctrl_c_handler(CTRL_C_EVENT);
            })
            .await
        });
        assert!(result.is_ok());
    }
}
