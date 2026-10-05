//! The Python objects standing in for the subprocess pipes.
//!
//! `PyProcessPipeTransport` is a stub for stdout and stderr: those are read by
//! worker threads, so the object exists only to satisfy
//! `get_pipe_transport(fd)` and carry `close()`/`is_closing()`.
//!
//! stdin is a real stream transport, and `PyProcessStdinProtocol` is the
//! protocol attached to it — it forwards the pipe's `connection_lost` into the
//! subprocess core so a closed stdin counts toward the open-pipe bookkeeping.

use std::sync::atomic::Ordering;

use pyo3::prelude::*;

use super::{PyProcessPipeTransport, PyProcessStdinProtocol};

#[pymethods]
impl PyProcessPipeTransport {
    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "PyProcessPipeTransport")
    )]
    fn close(&self) {
        self.core.closing.store(true, Ordering::SeqCst);
    }

    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "PyProcessPipeTransport")
    )]
    fn is_closing(&self) -> bool {
        self.core.closing.load(Ordering::SeqCst)
    }

    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "PyProcessPipeTransport")
    )]
    fn get_extra_info(&self, py: Python<'_>, _name: &str, default: Option<Py<PyAny>>) -> Py<PyAny> {
        default.unwrap_or_else(|| py.None())
    }

    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "PyProcessPipeTransport")
    )]
    fn pause_reading(&self) {}

    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "PyProcessPipeTransport")
    )]
    fn resume_reading(&self) {}

    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "PyProcessPipeTransport")
    )]
    fn __repr__(&self) -> String {
        format!(
            "<ProcessPipeTransport fd={} closing={}>",
            self.core.fd,
            self.is_closing()
        )
    }
}

#[pymethods]
impl PyProcessStdinProtocol {
    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "PyProcessStdinProtocol")
    )]
    fn connection_made(&self, _transport: Py<PyAny>) {}

    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "PyProcessStdinProtocol")
    )]
    fn pause_writing(&self) {}

    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "PyProcessStdinProtocol")
    )]
    fn resume_writing(&self) {}

    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "PyProcessStdinProtocol")
    )]
    #[pyo3(signature=(_exc=None))]
    fn connection_lost(&self, _exc: Option<Py<PyAny>>) -> PyResult<()> {
        if !self.core.has_open_pipe(0) {
            return Ok(());
        }
        self.core.pipe_connection_lost_message(0, None)
    }
}
