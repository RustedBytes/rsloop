//! Interval for periodic tick scheduling with drift compensation.

use std::time::{Duration, Instant};

use super::sleep::Sleep;

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
/// Example:
/// ```ignore
/// let mut interval = Interval::new(Duration::from_millis(100));
/// loop {
///     let ticks = interval.tick().await;
///     for _ in 0..ticks { /* handle tick */ }
/// }
/// ```
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
    pub async fn tick(&mut self) -> u64 {
        let now = Instant::now();

        // Determine base next (the previous next_deadline or now+period)
        let base_next = self.next_deadline.unwrap_or_else(|| now + self.period);

        match self.missed_tick_behavior {
            MissedTickBehavior::Skip => {
                // Advance target forward until it's in the future.
                let mut target = base_next;
                if target <= now {
                    if self.period.as_nanos() == 0 {
                        target = now;
                    } else {
                        // Advance by whole periods until target > now.
                        // Use a loop; the number of iterations is typically small.
                        let mut cnt: u64 = 0;
                        while target <= now {
                            target += self.period;
                            cnt = cnt.saturating_add(1);
                            // Safety: in pathological cases we break after huge iterations,
                            // but such large loops are unlikely in practice.
                            if cnt == u64::MAX {
                                break;
                            }
                        }
                    }
                }

                let remaining = target.saturating_duration_since(Instant::now());
                if remaining.as_millis() == 0 {
                    // For immediate remaining, yield once to avoid busy-looping.
                    Sleep::new_with_zero_behavior(remaining, super::sleep::ZeroBehavior::Yield)
                        .await;
                } else {
                    Sleep::new(remaining).await;
                }

                // Schedule next deadline for subsequent tick
                self.next_deadline = Some(target + self.period);
                1
            }
            MissedTickBehavior::CatchUp => {
                if base_next > now {
                    // Not missed yet: sleep until base_next and return 1
                    let remaining = base_next.saturating_duration_since(Instant::now());
                    if remaining.as_millis() == 0 {
                        Sleep::new_with_zero_behavior(remaining, super::sleep::ZeroBehavior::Yield)
                            .await;
                    } else {
                        Sleep::new(remaining).await;
                    }
                    self.next_deadline = Some(base_next + self.period);
                    1
                } else {
                    // We missed one or more ticks. Compute how many.
                    if self.period.as_nanos() == 0 {
                        // If period is zero, avoid infinite loops; treat as a single tick.
                        self.next_deadline = Some(Instant::now() + self.period);
                        return 1;
                    }

                    let elapsed = now.duration_since(base_next);
                    let missed = (elapsed.as_nanos() / self.period.as_nanos()) as u64 + 1;

                    // Advance base_next by `missed` periods to compute the next deadline.
                    let mut new_next = base_next;
                    // loop to add period missed times
                    for _ in 0..missed {
                        new_next += self.period;
                    }

                    // Set next_deadline to the instant after the catch-up window.
                    self.next_deadline = Some(new_next + self.period);

                    // Return the number of missed ticks so caller can catch up.
                    missed
                }
            }
        }
    }
}
