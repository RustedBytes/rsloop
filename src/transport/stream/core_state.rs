//! Transport state that `PyStreamTransport` exposes to Python.
//!
//! These are the accessors behind `get_protocol`, `get_extra_info`,
//! `pause_reading`, `set_write_buffer_limits`, and friends. Reader workers
//! block in `wait_until_readable` on the same condvar that `resume_reading`
//! signals, so a paused connection resumes immediately instead of sleeping
//! through a poll interval.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;

use super::core_write::{WriteBufferSignal, reconcile_write_buffer_limits};
use super::io_targets::TaskedDirectWriter;
use super::protocol::build_protocol_callbacks;
use super::tuning::DEFAULT_WRITE_BUFFER_HIGH_WATER;
use super::{StreamTransportCore, make_stream_extra};
use crate::fd_ops;

/// Resolve asyncio's optional write-buffer limits without overflowing when a
/// caller supplies a very large low-water mark.
fn normalize_write_buffer_limits(
    high: Option<usize>,
    low: Option<usize>,
) -> Option<(usize, usize)> {
    let high = match (high, low) {
        (Some(high), _) => high,
        (None, Some(low)) => low.checked_mul(4)?,
        (None, None) => DEFAULT_WRITE_BUFFER_HIGH_WATER,
    };
    let low = low.unwrap_or(high / 4);
    (high >= low).then_some((low, high))
}

impl StreamTransportCore {
    pub(crate) fn uses_native_stream_reader(&self) -> bool {
        matches!(
            self.state
                .lock()
                .expect("poisoned transport state")
                .callbacks
                .stream_reader_fast_path
                .as_ref(),
            Some(super::protocol::StreamReaderFastPath::Native { .. })
        )
    }

    pub(super) fn set_protocol(&self, py: Python<'_>, protocol: Py<PyAny>) -> PyResult<()> {
        let callbacks = build_protocol_callbacks(py, &protocol)?;
        let mut state = self.state.lock().expect("poisoned transport state");
        state.protocol = protocol;
        state.callbacks = callbacks;
        Ok(())
    }

    pub(super) fn get_protocol(&self, py: Python<'_>) -> Py<PyAny> {
        self.state
            .lock()
            .expect("poisoned transport state")
            .protocol
            .clone_ref(py)
    }

    pub(super) fn get_extra(&self, py: Python<'_>, name: &str) -> Option<Py<PyAny>> {
        let (cached, lazy_socket_family, transport_closed) = {
            let state = self.state.lock().expect("poisoned transport state");
            (
                state.extra.get(name).map(|value| value.clone_ref(py)),
                state.lazy_socket_family,
                state.closing || state.lost_called,
            )
        };
        if let Some(value) = cached {
            if name == "socket" && transport_closed {
                let _ = value.bind(py).call_method0("close");
            }
            return Some(value);
        }
        if lazy_socket_family.is_none() || !matches!(name, "socket" | "sockname" | "peername") {
            return None;
        }

        let family = lazy_socket_family?;
        let fd = self.direct_writer.as_ref().and_then(|writer| {
            writer
                .lock()
                .expect("poisoned direct tasked writer")
                .as_ref()
                .map(TaskedDirectWriter::fd)
        })?;
        let extra = make_stream_extra(py, fd, family).ok()?;
        if transport_closed && let Some(socket) = extra.get("socket") {
            let _ = socket.bind(py).call_method0("close");
        }
        let mut state = self.state.lock().expect("poisoned transport state");
        state.extra.extend(extra);
        state.lazy_socket_family = None;
        state.extra.get(name).map(|value| value.clone_ref(py))
    }

    #[inline]
    pub(super) fn set_closing(&self) {
        self.state.lock().expect("poisoned transport state").closing = true;
        self.state_cv.notify_all();
        self.read_state_notify.notify_all();
        self.read_buffer_pool.close();
    }

    pub(super) fn runtime_socket_fd(&self) -> Option<fd_ops::RawFd> {
        let state = self.state.lock().expect("poisoned transport state");
        if state.runtime_socket_io {
            state.io_fd
        } else {
            None
        }
    }

    pub(super) fn detach_underlying_stream(&self, py: Python<'_>) {
        self.detached.store(true, Ordering::Release);
        self.close_extra_socket_with_py(py);
        let mut state = self.state.lock().expect("poisoned transport state");
        state.closing = true;
        state.reading = false;
        state.writable = false;
        drop(state);
        self.reading.store(false, Ordering::Release);
        self.state_cv.notify_all();
        self.read_state_notify.notify_all();
        self.read_buffer_pool.close();
    }

    pub(super) fn release_direct_writer(&self) {
        if let Some(writer) = &self.direct_writer {
            writer.lock().expect("poisoned direct tasked writer").take();
        }
    }

    pub(super) fn is_closing_or_lost(&self) -> bool {
        let state = self.state.lock().expect("poisoned transport state");
        state.closing || state.lost_called
    }

    pub(super) fn mark_write_eof(&self) {
        self.state
            .lock()
            .expect("poisoned transport state")
            .write_eof_requested = true;
    }

    pub(super) fn is_closing(&self) -> bool {
        self.state.lock().expect("poisoned transport state").closing
    }

    pub(super) fn can_write_eof(&self) -> bool {
        self.state
            .lock()
            .expect("poisoned transport state")
            .can_write_eof
    }

    pub(super) fn pause_reading(&self) {
        let mut state = self.state.lock().expect("poisoned transport state");
        state.read_paused = true;
        state.reading = false;
        self.reading.store(false, Ordering::Release);
    }

    pub(super) fn resume_reading(&self) {
        let mut state = self.state.lock().expect("poisoned transport state");
        state.read_paused = false;
        state.reading = !state.read_backpressured && !state.closing;
        let reading = state.reading;
        drop(state);
        self.reading.store(reading, Ordering::Release);
        if reading {
            self.state_cv.notify_all();
            self.read_state_notify.notify_all();
        }
    }

    pub(super) fn is_reading(&self) -> bool {
        self.reading.load(Ordering::Acquire)
    }

    pub(super) fn wait_until_readable(&self) {
        let mut state = self.state.lock().expect("poisoned transport state");
        while (state.read_paused || state.read_backpressured) && !state.closing {
            // The timeout is only a backstop against missed notifications;
            // resume_reading()/set_closing() wake the worker immediately.
            let (guard, _) = self
                .state_cv
                .wait_timeout(state, Duration::from_millis(50))
                .expect("poisoned transport state");
            state = guard;
        }
    }

    pub(super) async fn wait_until_async_readable(&self) {
        loop {
            if self.is_closing() || self.is_reading() {
                return;
            }
            let wait = self.read_state_notify.listen();
            if self.is_closing() || self.is_reading() {
                return;
            }
            let _ = wait.await;
        }
    }

    pub(super) fn acquire_read_buffer_blocking(
        &self,
        capacity: usize,
        stop: Option<&std::sync::atomic::AtomicBool>,
    ) -> Option<Vec<u8>> {
        loop {
            if self.is_closing() || stop.is_some_and(|stop| stop.load(Ordering::Acquire)) {
                return None;
            }
            if let Some(buffer) = self.read_buffer_pool.try_acquire(capacity) {
                return Some(buffer);
            }
            self.read_buffer_pool
                .wait_timeout(Duration::from_millis(50));
        }
    }

    pub(super) async fn acquire_read_buffer_async(&self, capacity: usize) -> Option<Vec<u8>> {
        loop {
            if self.is_closing() {
                return None;
            }
            if let Some(buffer) = self.read_buffer_pool.try_acquire(capacity) {
                return Some(buffer);
            }
            self.read_buffer_pool.wait_async().await;
        }
    }

    pub(super) fn is_writable(&self) -> bool {
        self.state
            .lock()
            .expect("poisoned transport state")
            .writable
    }

    pub(super) fn get_write_buffer_size(&self) -> usize {
        self.state
            .lock()
            .expect("poisoned transport state")
            .write_buffer
            .size
    }

    pub(super) fn get_write_buffer_limits(&self) -> (usize, usize) {
        let state = self.state.lock().expect("poisoned transport state");
        (state.write_buffer.low_water, state.write_buffer.high_water)
    }

    pub(super) fn set_write_buffer_limits(
        self: &Arc<Self>,
        high: Option<usize>,
        low: Option<usize>,
    ) -> PyResult<()> {
        let (should_pause, should_resume) = {
            let mut state = self.state.lock().expect("poisoned transport state");
            let Some((low, high)) = normalize_write_buffer_limits(high, low) else {
                return Err(PyValueError::new_err(format!(
                    "high ({high:?}) must be >= low ({low:?}) and derived limits must fit usize"
                )));
            };

            match reconcile_write_buffer_limits(&mut state.write_buffer, low, high) {
                WriteBufferSignal::Pause => (true, false),
                WriteBufferSignal::Resume => (false, true),
                WriteBufferSignal::None => (false, false),
            }
        };

        if should_pause {
            self.notify_pause_writing();
        } else if should_resume {
            self.notify_resume_writing();
        }

        Ok(())
    }
}

#[cfg(test)]
mod write_buffer_limit_tests {
    use super::normalize_write_buffer_limits;

    #[test]
    fn derived_high_water_rejects_overflow() {
        assert_eq!(normalize_write_buffer_limits(None, Some(usize::MAX)), None);
    }
}

#[cfg(kani)]
mod verification {
    use super::{DEFAULT_WRITE_BUFFER_HIGH_WATER, normalize_write_buffer_limits};

    #[kani::proof]
    fn merge_write_buffer_limits_are_ordered_and_overflow_free() {
        let high: Option<usize> = kani::any();
        let low: Option<usize> = kani::any();
        let normalized = normalize_write_buffer_limits(high, low);

        match normalized {
            Some((resolved_low, resolved_high)) => {
                assert!(resolved_low <= resolved_high);
                match (high, low) {
                    (Some(given_high), Some(given_low)) => {
                        assert_eq!(resolved_high, given_high);
                        assert_eq!(resolved_low, given_low);
                    }
                    (Some(given_high), None) => {
                        assert_eq!(resolved_high, given_high);
                        assert_eq!(resolved_low, given_high / 4);
                    }
                    (None, Some(given_low)) => {
                        assert_eq!(resolved_high, given_low * 4);
                        assert_eq!(resolved_low, given_low);
                    }
                    (None, None) => {
                        assert_eq!(resolved_high, DEFAULT_WRITE_BUFFER_HIGH_WATER);
                        assert_eq!(resolved_low, DEFAULT_WRITE_BUFFER_HIGH_WATER / 4);
                    }
                }
            }
            None => match (high, low) {
                (Some(given_high), Some(given_low)) => assert!(given_high < given_low),
                (None, Some(given_low)) => assert!(given_low.checked_mul(4).is_none()),
                _ => unreachable!("defaulted limits are always valid"),
            },
        }
    }
}
