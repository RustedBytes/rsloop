//! Timeout future that races an inner future against a duration.

use std::{
    fmt,
    future::Future,
    pin::Pin,
    task::{Context, Poll},
    time::{Duration, Instant},
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

pin_project_lite::pin_project! {
    /// Timeout future that races the provided future against a timeout duration.
    ///
    /// If the inner future completes first, `Timeout` yields `Ok(T)`.
    /// If the timeout elapses first, `Timeout` yields `Err(TimeoutError)` and the
    /// inner future is dropped.
    /// The inner future is polled first, so a ready result wins even at an expired
    /// deadline. Timeout cancellation occurs when the wrapper is polled.
    pub struct Timeout<F> {
        #[pin]
        future: Option<F>,
        sleep: Option<Sleep>,
        // If true, the timeout has already fired (and future should be treated as timed out).
        timed_out: bool,
    }
}

impl<F> Timeout<F> {
    /// Create a new `Timeout` future.
    #[inline]
    pub fn new(future: F, duration: Duration) -> Self {
        Self::new_at(future, super::deadline_after(Instant::now(), duration))
    }

    /// Create a timeout with an absolute deadline, without rebasing it on now.
    #[inline]
    pub fn new_at(future: F, deadline: Instant) -> Self {
        Self {
            future: Some(future),
            sleep: Some(Sleep::sleep_until(deadline)),
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
        let this = self.project();

        if *this.timed_out {
            return Poll::Ready(Err(TimeoutError));
        }

        let mut future_pin = this.future;
        match future_pin
            .as_mut()
            .as_pin_mut()
            .expect("Timeout polled after completion")
            .poll(cx)
        {
            Poll::Ready(output) => {
                *this.sleep = None;
                future_pin.set(None);
                return Poll::Ready(Ok(output));
            }
            Poll::Pending => {}
        }

        // Next, poll the timeout sleep. If it is ready, mark timed out and
        // return Err. Otherwise remain Pending.
        match Pin::new(this.sleep.as_mut().expect("Timeout missing timer")).poll(cx) {
            Poll::Ready(()) => {
                *this.timed_out = true;
                *this.sleep = None;
                // Cancel even if the caller retains the completed Timeout.
                future_pin.set(None);
                Poll::Ready(Err(TimeoutError))
            }
            Poll::Pending => Poll::Pending,
        }
    }
}

/// Convenience async function that awaits `future` but returns an error if it
/// does not complete within `duration`.
///
/// Pending futures require a timer-enabled runtime. See
/// `tools/vibeio-check/EXAMPLES.md` for executable success and timeout examples.
#[inline]
pub async fn timeout<T>(
    duration: Duration,
    future: impl Future<Output = T>,
) -> Result<T, TimeoutError> {
    Timeout::new(future, duration).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, marker::PhantomPinned, rc::Rc, task::Waker};

    #[test]
    fn absolute_expiration_drops_pending_future_but_ready_result_wins() {
        crate::vibeio::executor::Runtime::new(crate::vibeio::driver::AnyDriver::new_mock())
            .block_on(async {
                let deadline = Instant::now() - Duration::from_secs(1);
                let dropped = Rc::new(Cell::new(false));
                assert_eq!(
                    super::super::timeout_at(
                        deadline,
                        future(&Rc::new(Cell::new(false)), &dropped)
                    )
                    .await,
                    Err(TimeoutError)
                );
                assert!(dropped.get());
                assert_eq!(
                    super::super::timeout_at(deadline, async { 42 }).await,
                    Ok(42)
                );
            });
    }

    #[test]
    fn cancelling_absolute_timeout_releases_registered_timer_and_future() {
        crate::vibeio::executor::Runtime::new(crate::vibeio::driver::AnyDriver::new_mock())
            .block_on(async {
                let dropped = Rc::new(Cell::new(false));
                let mut timeout = Box::pin(Timeout::new_at(
                    future(&Rc::new(Cell::new(false)), &dropped),
                    crate::vibeio::time::deadline_after(Instant::now(), Duration::MAX),
                ));
                let mut cx = Context::from_waker(Waker::noop());
                assert!(timeout.as_mut().poll(&mut cx).is_pending());
                let timer = crate::vibeio::executor::current_timer().unwrap();
                assert!(timer.spin_and_get_deadline().0.is_some());
                drop(timeout);
                assert!(dropped.get());
                assert!(timer.spin_and_get_deadline().0.is_none());
            });
    }

    struct PinnedFuture {
        ready: Rc<Cell<bool>>,
        dropped: Rc<Cell<bool>>,
        address: Cell<*const Self>,
        _pinned: PhantomPinned,
    }

    impl Future for PinnedFuture {
        type Output = u8;

        fn poll(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<u8> {
            let this = self.as_ref().get_ref();
            let previous = this.address.replace(this);
            assert!(previous.is_null() || std::ptr::eq(previous, this));
            if this.ready.get() {
                Poll::Ready(42)
            } else {
                Poll::Pending
            }
        }
    }

    impl Drop for PinnedFuture {
        fn drop(&mut self) {
            let address = self.address.get();
            assert!(address.is_null() || std::ptr::eq(address, self));
            assert!(!self.dropped.replace(true), "future dropped twice");
        }
    }

    fn future(ready: &Rc<Cell<bool>>, dropped: &Rc<Cell<bool>>) -> PinnedFuture {
        PinnedFuture {
            ready: ready.clone(),
            dropped: dropped.clone(),
            address: Cell::new(std::ptr::null()),
            _pinned: PhantomPinned,
        }
    }

    #[test]
    fn timeout_drops_pinned_future_before_returning_error() {
        let runtime =
            crate::vibeio::executor::Runtime::new(crate::vibeio::driver::AnyDriver::new_mock());
        runtime.block_on(async {
            let ready = Rc::new(Cell::new(false));
            let dropped = Rc::new(Cell::new(false));
            let mut timeout = Box::pin(Timeout::new(future(&ready, &dropped), Duration::ZERO));
            let mut cx = Context::from_waker(Waker::noop());
            assert!(matches!(
                timeout.as_mut().poll(&mut cx),
                Poll::Ready(Err(_))
            ));
            assert!(dropped.get());
            assert!(timeout.sleep.is_none());
            assert!(matches!(
                timeout.as_mut().poll(&mut cx),
                Poll::Ready(Err(_))
            ));
        });
    }

    #[test]
    fn timeout_success_releases_future_and_registered_timer() {
        let runtime =
            crate::vibeio::executor::Runtime::new(crate::vibeio::driver::AnyDriver::new_mock());
        runtime.block_on(async {
            let ready = Rc::new(Cell::new(false));
            let dropped = Rc::new(Cell::new(false));
            let mut timeout = Box::pin(Timeout::new(future(&ready, &dropped), Duration::MAX));
            let mut cx = Context::from_waker(Waker::noop());
            assert!(timeout.as_mut().poll(&mut cx).is_pending());
            let timer = crate::vibeio::executor::current_timer().unwrap();
            assert!(timer.spin_and_get_deadline().0.is_some());
            ready.set(true);
            assert!(matches!(
                timeout.as_mut().poll(&mut cx),
                Poll::Ready(Ok(42))
            ));
            assert!(dropped.get());
            assert!(timeout.sleep.is_none());
            assert!(timer.spin_and_get_deadline().0.is_none());
        });
    }

    #[test]
    fn dropping_pending_timeout_cancels_timer_and_pinned_future() {
        let runtime =
            crate::vibeio::executor::Runtime::new(crate::vibeio::driver::AnyDriver::new_mock());
        runtime.block_on(async {
            let dropped = Rc::new(Cell::new(false));
            let mut timeout = Box::pin(Timeout::new(
                future(&Rc::new(Cell::new(false)), &dropped),
                Duration::MAX,
            ));
            let mut cx = Context::from_waker(Waker::noop());
            assert!(timeout.as_mut().poll(&mut cx).is_pending());
            drop(timeout);
            assert!(dropped.get());
            assert!(
                crate::vibeio::executor::current_timer()
                    .unwrap()
                    .spin_and_get_deadline()
                    .0
                    .is_none()
            );
        });
    }
}
