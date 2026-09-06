//! Interval for periodic tick scheduling with drift compensation.

use std::time::{Duration, Instant};

use super::sleep::Sleep;

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
    /// Skip missed ticks and schedule the next tick at the next future multiple.
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
    #[inline]
    pub fn new(period: Duration) -> Self {
        Self {
            period,
            next_deadline: None,
            missed_tick_behavior: MissedTickBehavior::Skip,
        }
    }

    /// Configure how missed ticks are handled.
    #[inline]
    pub fn set_missed_tick_behavior(&mut self, behavior: MissedTickBehavior) {
        self.missed_tick_behavior = behavior;
    }

    /// Reset the interval schedule so the next tick is computed relative to
    /// the time when `tick()` is next called (useful when you want to restart
    /// the cadence).
    #[inline]
    pub fn reset(&mut self) {
        self.next_deadline = None;
    }

    /// Await the next tick. Returns the number of ticks that should be processed:
    /// - For `MissedTickBehavior::Skip` this will be `1`.
    /// - For `MissedTickBehavior::CatchUp` this may be `> 1` if several periods were missed.
    /// A zero period yields once and returns one tick in either mode.
    pub async fn tick(&mut self) -> u64 {
        self.tick_at(Instant::now()).await
    }

    // Keep the scheduling clock explicit so catch-up boundaries can be tested
    // without scheduler latency changing the expected number of missed ticks.
    async fn tick_at(&mut self, now: Instant) -> u64 {
        // Determine base next (the previous next_deadline or now+period)
        let base_next = self
            .next_deadline
            .unwrap_or_else(|| super::deadline_after(now, self.period));

        match self.missed_tick_behavior {
            MissedTickBehavior::Skip => {
                // Advance target forward until it's in the future.
                let mut target = base_next;
                if target <= now {
                    if self.period.as_nanos() == 0 {
                        target = now;
                    } else {
                        // Advance directly to the first cadence boundary after
                        // `now`; iterating once per missed period can otherwise
                        // make an old interval stall the executor.
                        let elapsed = now.duration_since(target);
                        target = super::deadline_after(
                            now,
                            self.period - duration_remainder(elapsed, self.period),
                        );
                    }
                }

                Sleep::sleep_until_with_zero_behavior(target, super::sleep::ZeroBehavior::Yield)
                    .await;

                // Schedule next deadline for subsequent tick
                self.next_deadline = Some(super::deadline_after(target, self.period));
                1
            }
            MissedTickBehavior::CatchUp => {
                if base_next > now {
                    // Not missed yet: sleep until base_next and return 1
                    Sleep::sleep_until_with_zero_behavior(
                        base_next,
                        super::sleep::ZeroBehavior::Yield,
                    )
                    .await;
                    self.next_deadline = Some(super::deadline_after(base_next, self.period));
                    1
                } else {
                    // We missed one or more ticks. Compute how many.
                    if self.period.as_nanos() == 0 {
                        // A zero-period catch-up loop must still let other
                        // tasks run, just as zero-period Skip mode does.
                        Sleep::new_with_zero_behavior(
                            Duration::ZERO,
                            super::sleep::ZeroBehavior::Yield,
                        )
                        .await;
                        self.next_deadline = Some(Instant::now());
                        return 1;
                    }

                    let elapsed = now.duration_since(base_next);
                    let missed = (elapsed.as_nanos() / self.period.as_nanos())
                        .saturating_add(1)
                        .min(u64::MAX as u128) as u64;

                    // The next deadline is the first cadence boundary after
                    // `now`. `missed` already accounts for the tick at
                    // `base_next`, so adding another period would skip a tick.
                    let new_next = super::deadline_after(
                        now,
                        self.period - duration_remainder(elapsed, self.period),
                    );
                    self.next_deadline = Some(new_next);

                    // Return the number of missed ticks so caller can catch up.
                    missed
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
    use crate::vibeio::{driver::AnyDriver, executor::Runtime};
    use std::{
        future::Future,
        task::{Context, Poll, Waker},
    };

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
