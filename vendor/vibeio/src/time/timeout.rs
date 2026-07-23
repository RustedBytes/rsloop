//! Timeout future that races an inner future against a duration.

use std::{
    fmt,
    future::Future,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use super::sleep::Sleep;

/// Error returned when a `timeout` expires.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimeoutError;

impl fmt::Display for TimeoutError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "operation timed out")
    }
}

impl std::error::Error for TimeoutError {}

/// Timeout future that races the provided future against a timeout duration.
///
/// If the inner future completes first, `Timeout` yields `Ok(T)`.
/// If the timeout elapses first, `Timeout` yields `Err(TimeoutError)` and the
/// inner future is dropped.
pub struct Timeout<F> {
    future: F,
    sleep: Sleep,
    /// If true, the timeout has already fired (and future should be treated as timed out).
    timed_out: bool,
}

impl<F> Timeout<F> {
    /// Create a new `Timeout` future.
    #[inline]
    pub fn new(future: F, duration: Duration) -> Self {
        Self {
            future,
            sleep: Sleep::new(duration),
            timed_out: false,
        }
    }
}

impl<F> Future for Timeout<F>
where
    F: Future,
{
    type Output = Result<F::Output, TimeoutError>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Safety: we don't move the fields out; we only create pinned projections.
        let this = unsafe { self.get_unchecked_mut() };

        if this.timed_out {
            return Poll::Ready(Err(TimeoutError));
        }

        // First, poll the inner future. If it completes, return the value and
        // allow `sleep` to be dropped (which cancels the timer).
        // We need a pinned projection of `future`.
        let mut future_pin = unsafe { Pin::new_unchecked(&mut this.future) };
        match Future::poll(future_pin.as_mut(), cx) {
            Poll::Ready(output) => return Poll::Ready(Ok(output)),
            Poll::Pending => {}
        }

        // Next, poll the timeout sleep. If it is ready, mark timed out and
        // return Err. Otherwise remain Pending.
        let mut sleep_pin = unsafe { Pin::new_unchecked(&mut this.sleep) };
        match sleep_pin.as_mut().poll(cx) {
            Poll::Ready(()) => {
                this.timed_out = true;
                // Dropping `future` happens naturally when this Timeout is dropped
                // by the executor or after we return. Return Err now.
                Poll::Ready(Err(TimeoutError))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Convenience async function that awaits `future` but returns an error if it
/// does not complete within `duration`.
///
/// Example:
/// ```ignore
/// let res = timeout(Duration::from_secs(1), some_future).await;
/// match res {
///     Ok(v) => { /* completed in time */ }
///     Err(_) => { /* timed out */ }
/// }
/// ```
#[inline]
pub async fn timeout<T>(
    duration: Duration,
    future: impl Future<Output = T>,
) -> Result<T, TimeoutError> {
    Timeout::new(future, duration).await
}
