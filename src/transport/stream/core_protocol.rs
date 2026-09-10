//! Delivering connection lifecycle and data events to the Python protocol.
//!
//! Every callback runs inside the transport's `contextvars.Context` when the
//! protocol needs one, and the fast paths short-circuit that: a recognised
//! stream reader is fed directly, and a buffered protocol goes through
//! `get_buffer`/`buffer_updated` instead of `data_received`. Errors raised by
//! protocol code are reported to the loop's exception handler rather than
//! propagated into the I/O worker that triggered them.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyDict};

use super::buffers::PendingReadBuffer;
use super::protocol::StreamReaderFastPath;
use super::tuning::{PENDING_READ_HIGH_WATER, PENDING_READ_LOW_WATER};
use super::{PendingReadEvent, PyStreamTransport, StreamTransportCore};
use crate::context::{run_in_context_noargs, run_in_context_onearg};

/// Copy raw bytes into a writable C-contiguous export, regardless of its
/// element format. The export is released before calling buffer_updated, which
/// is allowed to resize or replace the protocol's buffer.
fn copy_to_protocol_buffer(buffer: &Bound<'_, PyAny>, source: &[u8]) -> PyResult<usize> {
    pyo3::sync::critical_section::with_critical_section(buffer, || {
        copy_to_protocol_buffer_locked(buffer, source)
    })
}

fn copy_to_protocol_buffer_locked(buffer: &Bound<'_, PyAny>, source: &[u8]) -> PyResult<usize> {
    let py = buffer.py();
    // Exporters can make Py_buffer self-referential; keep its address stable.
    let mut view = Box::<pyo3::ffi::Py_buffer>::new_uninit();
    // SAFETY: buffer is live and attached; view has stable writable storage.
    let result = unsafe {
        pyo3::ffi::PyObject_GetBuffer(
            buffer.as_ptr(),
            view.as_mut_ptr(),
            pyo3::ffi::PyBUF_WRITABLE | pyo3::ffi::PyBUF_C_CONTIGUOUS,
        )
    };
    if result < 0 {
        return Err(PyErr::fetch(py));
    }
    // SAFETY: successful GetBuffer initialized the allocation in place.
    let mut view = unsafe { view.assume_init() };
    let result = if view.len <= 0 {
        Err(PyRuntimeError::new_err(
            "get_buffer() returned an empty buffer",
        ))
    } else {
        let count = source.len().min(view.len as usize);
        // Use the buffer-protocol C API rather than forming Rust references to
        // potentially shared Python memory. No Python callback can see an
        // intermediate bytes object, and non-byte element formats need no cast.
        // SAFETY: GetBuffer granted a writable contiguous export of at least
        // count bytes. source lives throughout the call. The C API does not
        // modify source (the mutable pointer supports Python 3.10's signature).
        let copied = unsafe {
            pyo3::ffi::PyBuffer_FromContiguous(
                &mut *view,
                source.as_ptr().cast_mut().cast(),
                count as pyo3::ffi::Py_ssize_t,
                b'C' as std::ffi::c_char,
            )
        };
        if copied < 0 {
            Err(PyErr::fetch(py))
        } else {
            Ok(count)
        }
    };
    // SAFETY: exactly one release of the successfully acquired live export,
    // still at the same address; all fallible paths above converge here.
    unsafe { pyo3::ffi::PyBuffer_Release(&mut *view) };
    result
}

#[cfg(test)]
mod buffer_tests {
    use super::copy_to_protocol_buffer;
    use pyo3::prelude::*;

    #[test]
    fn copies_prefix_and_releases_export_before_resize() {
        crate::initialize_python_for_tests();
        Python::attach(|py| {
            let buffer = py.eval(c"bytearray(b'________')", None, None).unwrap();
            assert_eq!(copy_to_protocol_buffer(&buffer, b"abc").unwrap(), 3);
            assert_eq!(buffer.extract::<Vec<u8>>().unwrap(), b"abc_____");
            buffer
                .call_method1("extend", (b"more".as_slice(),))
                .unwrap();
            assert_eq!(buffer.len().unwrap(), 12);
        });
    }

    #[test]
    fn supports_non_byte_formats_and_rejects_invalid_exports() {
        crate::initialize_python_for_tests();
        Python::attach(|py| {
            let buffer = py
                .eval(c"__import__('array').array('I', [0, 0])", None, None)
                .unwrap();
            assert_eq!(copy_to_protocol_buffer(&buffer, &[1, 2, 3]).unwrap(), 3);
            let bytes = buffer.call_method0("tobytes").unwrap();
            assert_eq!(&bytes.extract::<Vec<u8>>().unwrap()[..3], &[1, 2, 3]);
            for expr in [
                c"b'readonly'",
                c"bytearray()",
                c"memoryview(bytearray(8))[::2]",
            ] {
                let buffer = py.eval(expr, None, None).unwrap();
                assert!(copy_to_protocol_buffer(&buffer, b"abc").is_err());
            }
            let buffer = py.eval(c"bytearray()", None, None).unwrap();
            assert!(copy_to_protocol_buffer(&buffer, b"abc").is_err());
            buffer.call_method1("extend", (b"ok".as_slice(),)).unwrap();
            assert_eq!(copy_to_protocol_buffer(&buffer, b"long input").unwrap(), 2);
        });
    }
}

impl StreamTransportCore {
    pub(super) fn apply_pending_read_backpressure(&self) {
        if self.pending_read_bytes.load(Ordering::Acquire) < PENDING_READ_HIGH_WATER {
            return;
        }
        let mut state = self.state.lock().expect("poisoned transport state");
        if state.read_backpressured || state.closing {
            return;
        }
        state.read_backpressured = true;
        state.reading = false;
        self.reading.store(false, Ordering::Release);
    }

    pub(super) fn record_pending_read_drained(&self, len: usize) {
        let remaining = self
            .pending_read_bytes
            .fetch_sub(len, Ordering::AcqRel)
            .saturating_sub(len);
        if remaining > PENDING_READ_LOW_WATER {
            return;
        }
        let mut state = self.state.lock().expect("poisoned transport state");
        if !state.read_backpressured {
            return;
        }
        state.read_backpressured = false;
        state.reading = !state.read_paused && !state.closing;
        let reading = state.reading;
        drop(state);
        self.reading.store(reading, Ordering::Release);
        if reading {
            self.state_cv.notify_all();
            self.read_state_notify.notify_all();
        }
    }

    #[inline]
    pub(super) fn call_protocol_method0(
        &self,
        py: Python<'_>,
        callback: &Py<PyAny>,
        context: &Py<PyAny>,
        context_needs_run: bool,
    ) -> PyResult<Py<PyAny>> {
        run_in_context_noargs(py, context, context_needs_run, callback)
    }

    #[inline]
    pub(super) fn call_protocol_method1(
        &self,
        py: Python<'_>,
        callback: &Py<PyAny>,
        context: &Py<PyAny>,
        context_needs_run: bool,
        arg: Py<PyAny>,
    ) -> PyResult<Py<PyAny>> {
        run_in_context_onearg(py, context, context_needs_run, callback, arg.bind(py))
    }

    pub(super) fn flush_pending_data_with_py(
        &self,
        py: Python<'_>,
        pending_data: &mut Option<PendingReadBuffer<'_>>,
        fast_path: Option<&StreamReaderFastPath>,
    ) -> PyResult<()> {
        let Some(data) = pending_data.take() else {
            return Ok(());
        };

        if self.is_closing_or_lost() {
            Ok(())
        } else if let Some(fast_path) = fast_path {
            fast_path.feed_data(py, data.as_slice())
        } else {
            self.data_received_slow_path(py, data.as_slice())
        }
    }

    pub(super) fn report_error_with_py(
        &self,
        py: Python<'_>,
        err: PyErr,
        message: &str,
    ) -> PyResult<()> {
        let context = PyDict::new(py);
        context.set_item("message", message)?;
        context.set_item("exception", err.value(py))?;
        self.loop_core
            .call_exception_handler(py, Some(&self.loop_obj), context.unbind().into_any())
    }

    pub(super) fn report_error(&self, err: PyErr, message: &str) {
        let _ = Python::try_attach(|py| self.report_error_with_py(py, err, message));
    }

    pub fn connection_made(&self, transport: Py<PyStreamTransport>) -> PyResult<()> {
        self.call_in_loop_context(|py| {
            let (callback, fast_path, context, context_needs_run) = {
                let state = self.state.lock().expect("poisoned transport state");
                (
                    state.callbacks.connection_made.clone_ref(py),
                    state
                        .callbacks
                        .stream_reader_fast_path
                        .as_ref()
                        .map(|value| value.clone_ref(py)),
                    state.context.clone_ref(py),
                    state.context_needs_run,
                )
            };
            if let Some(fast_path) = fast_path.as_ref()
                && fast_path.connection_made(py, transport.clone_ref(py))?
            {
                return Ok(());
            }
            self.call_protocol_method1(
                py,
                &callback,
                &context,
                context_needs_run,
                transport.into_any(),
            )?;
            Ok(())
        })
    }

    pub fn data_received(&self, data: &[u8]) -> PyResult<()> {
        self.call_in_loop_context(|py| self.data_received_with_py(py, data))
    }

    pub fn eof_received(&self) -> PyResult<bool> {
        self.call_in_loop_context(|py| self.eof_received_with_py(py))
    }

    pub fn connection_lost(self: &Arc<Self>, exc: Option<PyErr>) -> PyResult<()> {
        // Always serialize loss with pending read data, even when the caller
        // is already on the loop thread. A direct callback here can overtake
        // data that a reader worker has queued but the loop has not drained
        // yet, exposing EOF before those bytes to StreamReader.
        self.enqueue_pending_read_event(PendingReadEvent::ConnectionLost(
            exc.map(|err| Python::attach(|py| err.value(py).to_string())),
        ));
        Ok(())
    }

    pub(super) fn report_connection_lost_result(&self, result: PyResult<()>) {
        if let Err(err) = result {
            self.report_error(err, "stream connection_lost callback failed");
        }
    }

    pub(super) fn data_received_with_py(&self, py: Python<'_>, data: &[u8]) -> PyResult<()> {
        let fast_path = {
            let state = self.state.lock().expect("poisoned transport state");
            state
                .callbacks
                .stream_reader_fast_path
                .as_ref()
                .map(|value| value.clone_ref(py))
        };

        if let Some(fast_path) = fast_path.as_ref() {
            return fast_path.feed_data(py, data);
        }

        self.data_received_slow_path(py, data)
    }

    pub(super) fn data_received_slow_path(&self, py: Python<'_>, data: &[u8]) -> PyResult<()> {
        let (data_received, get_buffer, buffer_updated, context, context_needs_run) = {
            let state = self.state.lock().expect("poisoned transport state");
            (
                state
                    .callbacks
                    .data_received
                    .as_ref()
                    .map(|value| value.clone_ref(py)),
                state
                    .callbacks
                    .get_buffer
                    .as_ref()
                    .map(|value| value.clone_ref(py)),
                state
                    .callbacks
                    .buffer_updated
                    .as_ref()
                    .map(|value| value.clone_ref(py)),
                state.context.clone_ref(py),
                state.context_needs_run,
            )
        };

        if let (Some(get_buffer), Some(buffer_updated)) =
            (get_buffer.as_ref(), buffer_updated.as_ref())
        {
            let mut offset = 0;
            while offset < data.len() {
                let remaining = data.len() - offset;
                let buffer_obj = run_in_context_onearg(
                    py,
                    &context,
                    context_needs_run,
                    get_buffer,
                    &remaining.into_pyobject(py)?.into_any(),
                )?;
                let chunk_len = copy_to_protocol_buffer(buffer_obj.bind(py), &data[offset..])?;
                run_in_context_onearg(
                    py,
                    &context,
                    context_needs_run,
                    buffer_updated,
                    &chunk_len.into_pyobject(py)?.into_any(),
                )?;
                offset += chunk_len;
            }
            return Ok(());
        }

        if let Some(data_received) = data_received.as_ref() {
            self.call_protocol_method1(
                py,
                data_received,
                &context,
                context_needs_run,
                PyBytes::new(py, data).unbind().into_any(),
            )?;
        }
        Ok(())
    }

    pub(super) fn eof_received_with_py(&self, py: Python<'_>) -> PyResult<bool> {
        let (callback, fast_path, context, context_needs_run) = {
            let state = self.state.lock().expect("poisoned transport state");
            (
                state
                    .callbacks
                    .eof_received
                    .as_ref()
                    .map(|value| value.clone_ref(py)),
                state
                    .callbacks
                    .stream_reader_fast_path
                    .as_ref()
                    .map(|value| value.clone_ref(py)),
                state.context.clone_ref(py),
                state.context_needs_run,
            )
        };
        if let Some(fast_path) = fast_path.as_ref() {
            return fast_path.eof_received(py);
        }
        let Some(callback) = callback else {
            return Ok(false);
        };
        let result = self.call_protocol_method0(py, &callback, &context, context_needs_run)?;
        result.bind(py).is_truthy()
    }

    pub(super) fn connection_lost_with_py(
        &self,
        py: Python<'_>,
        exc: Option<PyErr>,
    ) -> PyResult<()> {
        let (callback, fast_path, context, context_needs_run, server) = {
            let mut state = self.state.lock().expect("poisoned transport state");
            if self.detached.load(Ordering::Acquire) {
                state.lost_called = true;
                return Ok(());
            }
            if state.lost_called {
                return Ok(());
            }
            state.lost_called = true;
            state.closing = true;
            state.write_buffer.size = 0;
            state.write_buffer.protocol_paused = false;
            self.state_cv.notify_all();
            self.read_state_notify.notify_all();
            (
                state.callbacks.connection_lost.clone_ref(py),
                state
                    .callbacks
                    .stream_reader_fast_path
                    .as_ref()
                    .map(|value| value.clone_ref(py)),
                state.context.clone_ref(py),
                state.context_needs_run,
                state.server.as_ref().cloned(),
            )
        };

        if let Some(fast_path) = fast_path.as_ref() {
            fast_path.connection_lost(py, exc)?;
        } else {
            let arg = exc
                .map(|err| err.value(py).clone().unbind().into_any())
                .unwrap_or_else(|| py.None());
            self.call_protocol_method1(py, &callback, &context, context_needs_run, arg)?;
        }

        // Preserve asyncio's post-close get_extra_info("socket") behavior
        // without retaining the live shared owner through Python reference
        // cycles: cache a proxy first, then close its duplicate descriptor.
        let _ = self.get_extra(py, "socket");
        self.close_extra_socket_with_py(py);
        self.release_direct_writer();

        if let Some(server) = server.and_then(|weak| weak.upgrade()) {
            server.connection_lost();
        }
        Ok(())
    }
}
