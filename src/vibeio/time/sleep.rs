//! Sleep future and zero-duration behavior configuration.

use std::{
    cell::Cell,
    future::Future,
    pin::Pin,
    rc::Rc,
    task::{Context, Poll},
    time::{Duration, Instant},
};

use crate::vibeio::executor::current_timer;
use crate::vibeio::timer::{Timer, TimerHandle};

/// Behavior for zero-duration sleeps (duration < 1 ms).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ZeroBehavior {
    /// Complete immediately (default).
    Immediate,
    /// Yield to the scheduler once, then complete on the following poll.
    Yield,
}

/// Sleep is a future that completes after the given `Duration`.
///
/// Polling requires a runtime built with `enable_timer(true)`.
/// See `tools/vibeio-check/EXAMPLES.md` for an executable sleep example.
pub struct Sleep {
    /// The timer handle returned by the timer driver when the timer was scheduled.
    /// `None` means we haven't scheduled yet.
    handle: Option<TimerHandle>,
    /// Whether the timer has already fired / completed.
    fired: Cell<bool>,
    /// For zero-duration sleeps we may schedule a one-shot yield; track whether
    /// that yield has been scheduled so the subsequent poll completes.
    yield_scheduled: Cell<bool>,
    /// How to behave for durations below the timer resolution (less than 1ms).
    zero_behavior: ZeroBehavior,
    /// The absolute deadline when this sleep should complete.
    deadline: Instant,
    /// The timer used to schedule the sleep.
    timer: Option<Rc<Timer>>,
}

impl Sleep {
    /// Create a new Sleep instance for the provided `duration`.
    /// Deadlines beyond the platform's Instant range saturate at its upper limit.
    #[inline]
    pub fn new(duration: Duration) -> Self {
        Self::sleep_until(super::deadline_after(Instant::now(), duration))
    }

    /// Create a Sleep with custom behavior for zero-length waits.
    #[inline]
    pub fn new_with_zero_behavior(duration: Duration, zero_behavior: ZeroBehavior) -> Self {
        Self::sleep_until_with_zero_behavior(
            super::deadline_after(Instant::now(), duration),
            zero_behavior,
        )
    }

    /// Create a Sleep that completes at the specified absolute `deadline`.
    ///
    /// Preserves the absolute deadline without converting through a duration.
    #[inline]
    pub fn sleep_until(deadline: Instant) -> Self {
        Self::sleep_until_with_zero_behavior(deadline, ZeroBehavior::Immediate)
    }

    #[inline]
    pub(crate) fn sleep_until_with_zero_behavior(
        deadline: Instant,
        zero_behavior: ZeroBehavior,
    ) -> Self {
        Self {
            handle: None,
            fired: Cell::new(false),
            yield_scheduled: Cell::new(false),
            zero_behavior,
            deadline,
            timer: None,
        }
    }

    /// Reset the sleep to a new absolute `deadline` (`Instant`).
    ///
    /// If a timer was previously scheduled, cancel it. The timer will be
    /// rescheduled on the next `poll`. This allows reusing a `Sleep` value
    /// to implement steady intervals or dynamic timeout adjustments using
    /// absolute deadlines instead of relative durations.
    #[inline]
    pub fn reset(&mut self, deadline: Instant) {
        // Compute relative duration until deadline and update state.
        self.deadline = deadline;
        self.fired.set(false);
        self.yield_scheduled.set(false);

        // If there was an outstanding handle, cancel it so the timer won't
        // keep the old waker alive.
        if let Some(handle) = self.handle.take() {
            if let Some(timer_rc) = self.timer.as_ref() {
                timer_rc.cancel(handle);
            }
        }
    }
}

impl Future for Sleep {
    type Output = ();

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        if this.fired.get() {
            return Poll::Ready(());
        }

        if this.handle.is_none() {
            // Schedule with runtime timer.
            let timer_rc = this.timer.get_or_insert_with(|| {
                current_timer().expect("Sleep::poll called without a timer-enabled runtime")
            });

            // The timer driver expects a task `Waker`. We clone the task waker here.
            let waker = cx.waker().clone();
            match timer_rc.submit(this.deadline, waker) {
                Some(handle) => {
                    this.handle = Some(handle);
                    // Not fired yet.
                    Poll::Pending
                }
                None => {
                    // Timer driver woke us immediately (duration rounded to 0 or similar).
                    match this.zero_behavior {
                        ZeroBehavior::Immediate => {
                            this.fired.set(true);
                            Poll::Ready(())
                        }
                        ZeroBehavior::Yield => {
                            // If we haven't scheduled the one-shot yield yet, schedule it
                            // by waking ourselves and return Pending. On the subsequent
                            // poll we will observe `yield_scheduled` and complete.
                            if !this.yield_scheduled.replace(true) {
                                cx.waker().wake_by_ref();
                                Poll::Pending
                            } else {
                                this.fired.set(true);
                                Poll::Ready(())
                            }
                        }
                    }
                }
            }
        } else {
            // We were previously scheduled, and now we've been polled again.
            // The runtime's timer driver will wake the task by calling the task
            // waker when the timer expires.

            // Check if the deadline has actually been reached
            if Instant::now() >= this.deadline {
                this.fired.set(true);
                // A caller can poll after the deadline before the timer driver
                // drains its heap. Release that registration and its waker now.
                if let Some(handle) = this.handle.take()
                    && let Some(timer) = this.timer.as_ref()
                {
                    timer.cancel(handle);
                }
                Poll::Ready(())
            } else {
                // A spurious poll usually only needs a waiter update, not heap
                // removal/reinsertion. Unchanged wakers need no clone either.
                if let Some(handle) = this.handle
                    && let Some(timer) = this.timer.as_ref()
                    && timer.update_waker(handle, cx.waker())
                {
                    return Poll::Pending;
                }
                // A retired handle needs a fresh registration.
                if let Some(handle) = this.handle.take() {
                    if let Some(timer_rc) = this.timer.as_ref() {
                        timer_rc.cancel(handle);
                        let waker = cx.waker().clone();
                        match timer_rc.submit(this.deadline, waker) {
                            Some(handle) => {
                                this.handle = Some(handle);
                                // Not fired yet.
                                return Poll::Pending;
                            }
                            None => {
                                // Timer driver woke us immediately (duration rounded to 0 or similar).
                                this.fired.set(true);
                                return Poll::Ready(());
                            }
                        }
                    }
                }
                Poll::Pending
            }
        }
    }
}

impl Drop for Sleep {
    fn drop(&mut self) {
        // If we still have an outstanding timer handle, cancel it so the timer
        // won't hold onto our waker.
        if let Some(handle) = self.handle.take() {
            if let Some(timer_rc) = self.timer.take() {
                timer_rc.cancel(handle);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn absolute_deadline_constructor_preserves_expired_targets() {
        let deadline = Instant::now() - Duration::from_secs(1);
        let sleep = Sleep::sleep_until_with_zero_behavior(deadline, ZeroBehavior::Yield);
        assert_eq!(sleep.deadline, deadline);
        assert!(matches!(sleep.zero_behavior, ZeroBehavior::Yield));
    }
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::task::{Wake, Waker};

    struct WakeCounter(AtomicUsize);
    impl Wake for WakeCounter {
        fn wake(self: Arc<Self>) {
            self.0.fetch_add(1, Ordering::Relaxed);
        }
    }

    #[test]
    fn spurious_polls_preserve_registration_and_replace_waiter() {
        let timer = Rc::new(Timer::new());
        let mut sleep = Sleep::new(Duration::from_secs(60));
        sleep.timer = Some(timer);
        let first = Arc::new(WakeCounter(AtomicUsize::new(0)));
        let second = Arc::new(WakeCounter(AtomicUsize::new(0)));
        let first_waker = Waker::from(first.clone());
        let second_waker = Waker::from(second.clone());
        let mut cx = Context::from_waker(&first_waker);
        assert!(Pin::new(&mut sleep).poll(&mut cx).is_pending());
        let handle = sleep.handle;
        for _ in 0..10 {
            assert!(Pin::new(&mut sleep).poll(&mut cx).is_pending());
            assert_eq!(sleep.handle, handle);
        }
        assert!(
            Pin::new(&mut sleep)
                .poll(&mut Context::from_waker(&second_waker))
                .is_pending()
        );
        assert_eq!(sleep.handle, handle);
        assert_eq!(Arc::strong_count(&first), 2);
        assert_eq!(Arc::strong_count(&second), 3);
        drop(sleep);
        assert_eq!(Arc::strong_count(&second), 2);
    }

    #[test]
    fn completed_sleep_releases_registration_before_timer_is_drained() {
        let timer = Rc::new(Timer::new());
        let mut sleep = Sleep::new(Duration::from_secs(60));
        sleep.timer = Some(timer.clone());
        let owner = Arc::new(WakeCounter(AtomicUsize::new(0)));
        let waker = Waker::from(owner.clone());
        let mut cx = Context::from_waker(&waker);
        assert!(Pin::new(&mut sleep).poll(&mut cx).is_pending());
        assert_eq!(Arc::strong_count(&owner), 3);
        // Deterministically model deadline passage without draining the timer.
        sleep.deadline = Instant::now();
        assert!(Pin::new(&mut sleep).poll(&mut cx).is_ready());
        assert_eq!(Arc::strong_count(&owner), 2);
        assert!(timer.spin_and_get_deadline().0.is_none());
        assert!(Pin::new(&mut sleep).poll(&mut cx).is_ready());
        assert_eq!(owner.0.load(Ordering::Relaxed), 0);
    }
}
