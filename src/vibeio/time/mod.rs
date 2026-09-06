//! A small `time` utility module for `vibeio`.
//!
//! This module provides:
//! - `Sleep`: a Future that completes after a duration.
//! - `Interval`: a convenience type with a `tick().await` method to await periodic ticks.
//! - `timeout`: a function / `Timeout` future that races a future against a timeout.
//!
//! Implementation notes:
//! - The runtime's `Timer` driver (in `crate::vibeio::timer`) is accessed through
//!   `crate::vibeio::executor::current_timer()` which returns an `Rc<Timer>` when called
//!   from inside a runtime. Calling these time utilities outside a runtime will
//!   panic (matching the library's general behavior for runtime-only APIs).
//! - The `Timer` driver accepts a `Waker` and returns an optional `TimerHandle`.
//!   We store the `TimerHandle` and cancel it if the Sleep is dropped before firing.

mod interval;
mod sleep;
mod timeout;

/// Add a duration, saturating at the platform's last representable Instant.
#[inline]
pub(crate) fn deadline_after(
    base: std::time::Instant,
    duration: std::time::Duration,
) -> std::time::Instant {
    if let Some(deadline) = base.checked_add(duration) {
        return deadline;
    }
    saturating_deadline(base, duration)
}

#[cold]
fn saturating_deadline(
    base: std::time::Instant,
    duration: std::time::Duration,
) -> std::time::Instant {
    // Instant has no portable MAX. Find the largest representable addition
    // only on overflow; ordinary timer creation stays a single checked add.
    let mut low = 0;
    let mut high = duration.as_nanos();
    let mut result = base;
    while low < high {
        let middle = low + (high - low).div_ceil(2);
        let candidate = std::time::Duration::new(
            (middle / 1_000_000_000) as u64,
            (middle % 1_000_000_000) as u32,
        );
        if let Some(deadline) = base.checked_add(candidate) {
            result = deadline;
            low = middle;
        } else {
            high = middle - 1;
        }
    }
    result
}

// Re-export public types and functions
// Public runtime API; not every embedding uses this re-export.
#[allow(unused_imports)]
pub use interval::{Interval, MissedTickBehavior};
pub use sleep::{Sleep, ZeroBehavior};
// Public runtime API; not every embedding uses this re-export.
#[allow(unused_imports)]
pub use timeout::{Timeout, TimeoutError, timeout};

/// Convenience builder: returns a `Sleep` future.
#[inline]
pub fn sleep(duration: std::time::Duration) -> Sleep {
    Sleep::new(duration)
}

/// Convenience builder allowing zero-behavior control for tiny durations.
#[inline]
pub fn sleep_with_zero_behavior(duration: std::time::Duration, behavior: ZeroBehavior) -> Sleep {
    Sleep::new_with_zero_behavior(duration, behavior)
}

/// Convenience builder: returns an `Interval`.
#[inline]
pub fn interval(period: std::time::Duration) -> Interval {
    Interval::new(period)
}

/// Convenience builder: returns a `Sleep` that completes at the provided absolute `Instant`.
#[inline]
pub fn sleep_until(deadline: std::time::Instant) -> Sleep {
    Sleep::sleep_until(deadline)
}

/// Convenience async function that awaits `future` but returns an error if it
/// does not complete before the absolute `deadline` Instant.
/// The inner future is polled first; an immediately ready result wins even if
/// the deadline has already elapsed.
#[inline]
pub async fn timeout_at<T>(
    deadline: std::time::Instant,
    future: impl std::future::Future<Output = T>,
) -> Result<T, TimeoutError> {
    Timeout::new_at(future, deadline).await
}

/// Convenience builder: returns an `Interval` with the first tick scheduled to
/// complete at `first_tick_instant` and subsequent ticks every `period`.
#[inline]
pub fn interval_at(
    first_tick_instant: std::time::Instant,
    period: std::time::Duration,
) -> Interval {
    let mut iv = Interval::new(period);
    iv.next_deadline = Some(first_tick_instant);
    iv
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vibeio::driver::AnyDriver;
    use std::time::{Duration, Instant};

    #[test]
    fn deadline_addition_is_exact_or_saturates_at_platform_limit() {
        let base = Instant::now();
        for duration in [
            Duration::ZERO,
            Duration::from_nanos(1),
            Duration::from_secs(3600),
        ] {
            assert_eq!(deadline_after(base, duration), base + duration);
        }
        let largest = deadline_after(base, Duration::MAX);
        assert!(largest > base);
        assert!(largest.checked_add(Duration::from_nanos(1)).is_none());
        assert_eq!(deadline_after(largest, Duration::MAX), largest);
    }

    #[test]
    fn maximum_duration_sleep_timeout_and_intervals_remain_pending() {
        use std::{future::Future, task::Context};
        crate::vibeio::executor::Runtime::new(AnyDriver::new_mock()).block_on(async {
            let mut cx = Context::from_waker(std::task::Waker::noop());
            let mut sleep = std::pin::pin!(sleep(Duration::MAX));
            assert!(sleep.as_mut().poll(&mut cx).is_pending());
            let mut timeout = std::pin::pin!(timeout(Duration::MAX, std::future::pending::<()>()));
            assert!(timeout.as_mut().poll(&mut cx).is_pending());
            for behavior in [MissedTickBehavior::Skip, MissedTickBehavior::CatchUp] {
                let mut interval = Interval::new(Duration::MAX);
                interval.set_missed_tick_behavior(behavior);
                let mut tick = std::pin::pin!(interval.tick());
                assert!(tick.as_mut().poll(&mut cx).is_pending());
            }
        });
    }

    #[test]
    fn sleep_completes() {
        let rt = crate::vibeio::executor::Runtime::new(AnyDriver::new_mock());
        rt.block_on(async {
            Sleep::new(Duration::from_millis(1)).await;
        });
    }

    #[test]
    fn timeout_expires() {
        let rt = crate::vibeio::executor::Runtime::new(AnyDriver::new_mock());
        let res = rt.block_on(async {
            let never = async {
                futures_util::future::pending::<()>().await;
            };
            timeout(Duration::from_millis(1), never).await
        });
        assert!(res.is_err());
    }

    #[test]
    fn timeout_succeeds_if_future_completes() {
        let rt = crate::vibeio::executor::Runtime::new(AnyDriver::new_mock());
        let res = rt.block_on(async { timeout(Duration::from_secs(1), async { 123usize }).await });
        assert_eq!(res, Ok(123usize));
    }

    #[test]
    fn interval_ticks_skip() {
        let rt = crate::vibeio::executor::Runtime::new(AnyDriver::new_mock());
        rt.block_on(async {
            let mut interval = Interval::new(Duration::from_millis(1));
            // two ticks should complete quickly
            let _ = interval.tick().await;
            let _ = interval.tick().await;
        });
    }
}
