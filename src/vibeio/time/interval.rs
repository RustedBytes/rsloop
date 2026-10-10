//! Interval for periodic tick scheduling with drift compensation.

use std::time::{Duration, Instant};

use super::sleep::Sleep;

#[cfg_attr(feature = "profile", hotpath::measure)]
#[inline]
fn duration_remainder(duration: Duration, divisor: Duration) -> Duration {
    let remainder = duration.as_nanos() % divisor.as_nanos();
    Duration::new(
        (remainder / 1_000_000_000) as u64,
        (remainder % 1_000_000_000) as u32,
    )
}

/// How to handle missed ticks for `Interval`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MissedTickBehavior {
    /// Skip missed ticks and schedule the next tick at the next future
    /// multiple.
    Skip,
    /// Return the number of missed ticks so the caller can run catch-up loop.
    CatchUp,
}

/// Interval provides a simple API to await periodic ticks while maintaining
/// a steady cadence (accounting for drift): each tick tries to occur at an
/// integer multiple of the original period relative to the interval start.
///
/// Requires a timer-enabled runtime. See `tools/vibeio-check/EXAMPLES.md`
/// for an executable finite-loop example.
/// Deadline arithmetic saturates at the platform's last representable Instant.
pub struct Interval {
    period: Duration,
    /// The next absolute deadline for the interval. If `None`, the next tick
    /// will be scheduled relative to the current time.
    pub next_deadline: Option<Instant>,
    /// Behavior when ticks are missed.
    missed_tick_behavior: MissedTickBehavior,
}

impl Interval {
    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "Interval"))]
    #[inline]
    pub fn new(period: Duration) -> Self {
        Self {
            period,
            next_deadline: None,
            missed_tick_behavior: MissedTickBehavior::Skip,
        }
    }

    /// Configure how missed ticks are handled.
    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "Interval"))]
    #[inline]
    pub fn set_missed_tick_behavior(&mut self, behavior: MissedTickBehavior) {
        self.missed_tick_behavior = behavior;
    }

    /// Reset the interval schedule so the next tick is computed relative to
    /// the time when `tick()` is next called (useful when you want to restart
    /// the cadence).
    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "Interval"))]
    #[inline]
    pub fn reset(&mut self) {
        self.next_deadline = None;
    }

    /// Await the next tick. Returns the number of ticks that should be
    /// processed:
    /// - For `MissedTickBehavior::Skip` this will be `1`.
    /// - For `MissedTickBehavior::CatchUp` this may be `> 1` if several periods
    ///   were missed.
    /// A zero period yields once and returns one tick in either mode.
    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "Interval", future = true)
    )]
    pub async fn tick(&mut self) -> u64 {
        self.tick_at(Instant::now()).await
    }

    // Keep the scheduling clock explicit so catch-up boundaries can be tested
    // without scheduler latency changing the expected number of missed ticks.
    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "Interval", future = true)
    )]
    async fn tick_at(&mut self, now: Instant) -> u64 {
        let (ticks, next_deadline) = match plan_tick(
            self.period,
            self.next_deadline,
            self.missed_tick_behavior,
            now,
        ) {
            TickPlan::WaitUntil(target) => {
                Sleep::sleep_until_with_zero_behavior(target, super::sleep::ZeroBehavior::Yield)
                    .await;
                (1, super::deadline_after(target, self.period))
            }
            TickPlan::CatchUp {
                ticks,
                next_deadline,
            } => (ticks, next_deadline),
            TickPlan::YieldAndReset => {
                Sleep::new_with_zero_behavior(Duration::ZERO, super::sleep::ZeroBehavior::Yield)
                    .await;
                (1, Instant::now())
            }
        };
        // Commit only after the wait completes. Dropping a pending tick leaves
        // the previous schedule intact, including zero-period catch-up ticks.
        self.next_deadline = Some(next_deadline);
        ticks
    }
}

/// Scheduling decisions contain no timer registration, clock read or mutation.
#[derive(Debug, PartialEq, Eq)]
enum TickPlan {
    WaitUntil(Instant),
    CatchUp { ticks: u64, next_deadline: Instant },
    YieldAndReset,
}

#[cfg_attr(feature = "profile", hotpath::measure)]
#[inline]
fn plan_tick(
    period: Duration,
    next_deadline: Option<Instant>,
    behavior: MissedTickBehavior,
    now: Instant,
) -> TickPlan {
    let base_next = next_deadline.unwrap_or_else(|| super::deadline_after(now, period));
    if base_next > now {
        return TickPlan::WaitUntil(base_next);
    }
    if period.is_zero() {
        return match behavior {
            MissedTickBehavior::Skip => TickPlan::WaitUntil(now),
            MissedTickBehavior::CatchUp => TickPlan::YieldAndReset,
        };
    }

    let elapsed = now.duration_since(base_next);
    // Jump to the first cadence boundary after now, independent of the number
    // of missed periods. The remainder preserves sub-millisecond precision.
    let next = super::deadline_after(now, period - duration_remainder(elapsed, period));
    match behavior {
        MissedTickBehavior::Skip => TickPlan::WaitUntil(next),
        MissedTickBehavior::CatchUp => TickPlan::CatchUp {
            ticks: (elapsed.as_nanos() / period.as_nanos())
                .saturating_add(1)
                .min(u64::MAX as u128) as u64,
            next_deadline: next,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn duration_remainders_satisfy_euclidean_invariants_at_numeric_limits() {
        let values = [
            Duration::ZERO,
            Duration::from_nanos(1),
            Duration::new(0, 999_999_999),
            Duration::from_secs(1),
            Duration::new(1, 1),
            Duration::new(u64::MAX / 2, 999_999_999),
            Duration::from_secs(u64::MAX),
            Duration::MAX,
        ];
        for dividend in values {
            for divisor in values.into_iter().filter(|value| !value.is_zero()) {
                let remainder = duration_remainder(dividend, divisor);
                // These two conditions uniquely determine Euclidean remainder;
                // check the reconstructed Duration, not a duplicate conversion.
                assert!(remainder < divisor);
                assert!(remainder <= dividend);
                assert_eq!((dividend - remainder).as_nanos() % divisor.as_nanos(), 0);
                if dividend < divisor {
                    assert_eq!(remainder, dividend);
                }
            }
        }
        // Translation by any whole number of periods preserves phase.
        for period in 1..=64 {
            for nanos in 0..=128 {
                let divisor = Duration::from_nanos(period);
                let dividend = Duration::from_nanos(nanos);
                assert_eq!(
                    duration_remainder(dividend + divisor * 17, divisor),
                    duration_remainder(dividend, divisor)
                );
            }
        }
    }

    #[test]
    fn tick_plan_preserves_cadence_at_and_between_boundaries() {
        let base = Instant::now();
        for period_ns in [1, 3, 100_000_000] {
            let period = Duration::from_nanos(period_ns);
            for elapsed_ns in [0, 1, period_ns - 1, period_ns, 17 * period_ns + 1] {
                let now = base + Duration::from_nanos(elapsed_ns);
                // An intentionally simple bounded reference: advance one tick
                // at a time rather than reuse the production remainder formula.
                let mut next = base;
                let mut ticks = 0;
                while next <= now {
                    next += period;
                    ticks += 1;
                }
                assert_eq!(
                    plan_tick(period, Some(base), MissedTickBehavior::Skip, now),
                    TickPlan::WaitUntil(next)
                );
                assert_eq!(
                    plan_tick(period, Some(base), MissedTickBehavior::CatchUp, now),
                    TickPlan::CatchUp {
                        ticks,
                        next_deadline: next
                    }
                );
            }
        }
    }

    #[test]
    fn tick_plan_distinguishes_initial_future_and_zero_period_ticks() {
        let now = Instant::now();
        let period = Duration::from_secs(1);
        let future = now + period;
        for behavior in [MissedTickBehavior::Skip, MissedTickBehavior::CatchUp] {
            assert_eq!(
                plan_tick(period, None, behavior, now),
                TickPlan::WaitUntil(future)
            );
            assert_eq!(
                plan_tick(period, Some(future), behavior, now),
                TickPlan::WaitUntil(future)
            );
            // An explicit future first tick must still wait with a zero period.
            assert_eq!(
                plan_tick(Duration::ZERO, Some(future), behavior, now),
                TickPlan::WaitUntil(future)
            );
        }
        assert_eq!(
            plan_tick(Duration::ZERO, None, MissedTickBehavior::Skip, now),
            TickPlan::WaitUntil(now)
        );
        assert_eq!(
            plan_tick(Duration::ZERO, None, MissedTickBehavior::CatchUp, now),
            TickPlan::YieldAndReset
        );
    }

    #[test]
    fn cancelling_future_tick_preserves_explicit_and_initial_schedules() {
        Runtime::new(AnyDriver::new_mock()).block_on(async {
            let now = Instant::now();
            for behavior in [MissedTickBehavior::Skip, MissedTickBehavior::CatchUp] {
                for original in [None, Some(now + Duration::from_secs(60))] {
                    let mut interval = Interval::new(Duration::from_secs(60));
                    interval.set_missed_tick_behavior(behavior);
                    interval.next_deadline = original;
                    {
                        let mut tick = std::pin::pin!(interval.tick_at(now));
                        assert!(
                            tick.as_mut()
                                .poll(&mut Context::from_waker(Waker::noop()))
                                .is_pending()
                        );
                    }
                    assert_eq!(interval.next_deadline, original);
                }
            }
        });
    }

    #[test]
    fn overdue_large_period_saturates_its_next_deadline() {
        Runtime::new(AnyDriver::new_mock()).block_on(async {
            let now = Instant::now();
            let mut interval = Interval::new(Duration::MAX);
            interval.set_missed_tick_behavior(MissedTickBehavior::CatchUp);
            interval.next_deadline = Some(now);
            assert_eq!(interval.tick_at(now).await, 1);
            assert_eq!(
                interval.next_deadline,
                Some(super::super::deadline_after(now, Duration::MAX))
            );
        });
    }

    #[test]
    fn catchup_counts_exact_boundaries_without_clock_races() {
        Runtime::new(AnyDriver::new_mock()).block_on(async {
            let base = Instant::now();
            for (elapsed_ns, missed, next_ms) in [
                (0, 1, 100),
                (99_999_999, 1, 100),
                (100_000_000, 2, 200),
                (1_000_000_000, 11, 1100),
                (1_050_000_000, 11, 1100),
            ] {
                let mut interval = Interval::new(Duration::from_millis(100));
                interval.set_missed_tick_behavior(MissedTickBehavior::CatchUp);
                interval.next_deadline = Some(base);
                assert_eq!(
                    interval
                        .tick_at(base + Duration::from_nanos(elapsed_ns))
                        .await,
                    missed
                );
                assert_eq!(
                    interval.next_deadline,
                    Some(base + Duration::from_millis(next_ms))
                );
            }
        });
    }
    use std::{
        future::Future,
        task::{Context, Poll, Waker},
    };

    use crate::vibeio::{driver::AnyDriver, executor::Runtime};

    #[test]
    fn zero_period_yields_on_every_tick_in_both_modes() {
        Runtime::new(AnyDriver::new_mock()).block_on(async {
            for behavior in [MissedTickBehavior::Skip, MissedTickBehavior::CatchUp] {
                let mut interval = Interval::new(Duration::ZERO);
                interval.set_missed_tick_behavior(behavior);
                let mut cx = Context::from_waker(Waker::noop());
                for _ in 0..3 {
                    let mut tick = std::pin::pin!(interval.tick());
                    assert!(tick.as_mut().poll(&mut cx).is_pending());
                    assert!(matches!(tick.as_mut().poll(&mut cx), Poll::Ready(1)));
                }
            }
        });
    }

    #[test]
    fn cancelling_zero_period_tick_preserves_schedule() {
        Runtime::new(AnyDriver::new_mock()).block_on(async {
            for behavior in [MissedTickBehavior::Skip, MissedTickBehavior::CatchUp] {
                let mut interval = Interval::new(Duration::ZERO);
                interval.set_missed_tick_behavior(behavior);
                let original = Instant::now();
                interval.next_deadline = Some(original);
                let mut cx = Context::from_waker(Waker::noop());
                {
                    let mut tick = std::pin::pin!(interval.tick());
                    assert!(tick.as_mut().poll(&mut cx).is_pending());
                }
                assert_eq!(interval.next_deadline, Some(original));
                assert_eq!(interval.tick().await, 1);
            }
        });
    }
}
