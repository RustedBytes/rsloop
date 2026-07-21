//! A small `time` utility module for `vibeio`.
//!
//! This module provides:
//! - `Sleep`: a Future that completes after a duration.
//! - `Interval`: a convenience type with a `tick().await` method to await periodic ticks.
//! - `timeout`: a function / `Timeout` future that races a future against a timeout.
//!
//! Implementation notes:
//! - The runtime's `Timer` driver (in `crate::timer`) is accessed through
//!   `crate::executor::current_timer()` which returns an `Rc<Timer>` when called
//!   from inside a runtime. Calling these time utilities outside a runtime will
//!   panic (matching the library's general behavior for runtime-only APIs).
//! - The `Timer` driver accepts a `Waker` and returns an optional `TimerHandle`.
//!   We store the `TimerHandle` and cancel it if the Sleep is dropped before firing.

mod interval;
mod sleep;
mod timeout;

// Re-export public types and functions
pub use interval::{Interval, MissedTickBehavior};
pub use sleep::{Sleep, ZeroBehavior};
pub use timeout::{timeout, Timeout, TimeoutError};

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
#[inline]
pub async fn timeout_at<T>(
    deadline: std::time::Instant,
    future: impl std::future::Future<Output = T>,
) -> Result<T, TimeoutError> {
    let dur = deadline.saturating_duration_since(std::time::Instant::now());
    timeout(dur, future).await
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
    use crate::driver::AnyDriver;
    use std::time::{Duration, Instant};

    #[test]
    fn sleep_completes() {
        let rt = crate::executor::Runtime::new(AnyDriver::new_mock());
        rt.block_on(async {
            Sleep::new(Duration::from_millis(1)).await;
        });
    }

    #[test]
    fn timeout_expires() {
        let rt = crate::executor::Runtime::new(AnyDriver::new_mock());
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
        let rt = crate::executor::Runtime::new(AnyDriver::new_mock());
        let res = rt.block_on(async { timeout(Duration::from_secs(1), async { 123usize }).await });
        assert_eq!(res, Ok(123usize));
    }

    #[test]
    fn interval_ticks_skip() {
        let rt = crate::executor::Runtime::new(AnyDriver::new_mock());
        rt.block_on(async {
            let mut interval = Interval::new(Duration::from_millis(1));
            // two ticks should complete quickly
            let _ = interval.tick().await;
            let _ = interval.tick().await;
        });
    }

    #[test]
    fn interval_catchup_returns_multiple() {
        let rt = crate::executor::Runtime::new(AnyDriver::new_mock());
        rt.block_on(async {
            let mut interval = Interval::new(Duration::from_millis(1));
            interval.set_missed_tick_behavior(MissedTickBehavior::CatchUp);
            // Initialise baseline
            let _ = interval.tick().await;
            // Artificially push next_deadline into the past to simulate missed ticks
            interval.next_deadline = Some(Instant::now() - Duration::from_millis(10));
            let missed = interval.tick().await;
            assert!(missed >= 1);
        });
    }
}
