//! The `StreamTransport` object Python holds.
//!
//! `PyStreamTransport` is a thin handle over `StreamTransportCore`; this module
//! carries the asyncio `Transport` surface (`write`, `writelines`, `close`,
//! `abort`, the `get_extra_info` accessors) plus `write_data`, the shared entry
//! point that accepts any buffer Python hands us.

use std::net::Shutdown;

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::{PyBytes, PyString};

#[cfg(unix)]
use super::io_targets::shutdown_unix_stream;
use super::io_targets::{TaskedDirectWriter, shutdown_tcp_stream};
use super::{PyStreamTransport, WriterCommand, stop_socket_reader_nowait};

impl PyStreamTransport {
    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "PyStreamTransport")
    )]
    pub(crate) fn write_data(&self, py: Python<'_>, data: &Bound<'_, PyAny>) -> PyResult<()> {
        if self.core.is_closing() {
            return Ok(());
        }
        if !self.core.is_writable() {
            return Err(PyRuntimeError::new_err("transport is not writable"));
        }

        let borrowed_bytes;
        let converted = if self.core.has_text_encoding
            && let Some(encoding) = self.core.get_extra(py, "text_encoding")
        {
            if data.is_instance_of::<PyString>() {
                let errors = self
                    .core
                    .get_extra(py, "text_errors")
                    .unwrap_or_else(|| PyString::new(py, "strict").unbind().into_any());
                data.call_method1("encode", (encoding, errors))?
            } else {
                py.import("builtins")?.getattr("bytes")?.call1((data,))?
            }
        } else if let Ok(bytes) = data.cast::<PyBytes>() {
            borrowed_bytes = bytes;
            self.core
                .try_write_bytes(borrowed_bytes.as_bytes())
                .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
            return Ok(());
        } else {
            py.import("builtins")?.getattr("bytes")?.call1((data,))?
        };
        let bytes = converted.cast::<PyBytes>()?;
        self.core
            .try_write_bytes(bytes.as_bytes())
            .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
        Ok(())
    }
}

#[pymethods]
impl PyStreamTransport {
    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "PyStreamTransport")
    )]
    fn write(&self, py: Python<'_>, data: &Bound<'_, PyAny>) -> PyResult<()> {
        self.write_data(py, data)
    }

    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "PyStreamTransport")
    )]
    pub(super) fn writelines(&self, py: Python<'_>, seq: &Bound<'_, PyAny>) -> PyResult<()> {
        if self.core.has_text_encoding {
            for item in seq.try_iter()? {
                self.write_data(py, &item?)?;
            }
            return Ok(());
        }
        if self.core.is_closing() {
            return Ok(());
        }
        if !self.core.is_writable() {
            return Err(PyRuntimeError::new_err("transport is not writable"));
        }

        // Match asyncio's single-write writelines behavior. Besides reducing
        // syscalls for framed protocols, validating and joining here lets the
        // direct path account for backpressure once for the complete batch.
        // Immutable byte segments need neither a Python conversion nor a
        // builtins lookup (the common framed-protocol case).
        let mut bytes_type = None;
        let mut joined = self.core.new_pooled_write_buffer(0);
        for item in seq.try_iter()? {
            let item = item?;
            if let Ok(bytes) = item.cast::<PyBytes>() {
                joined.extend_from_slice(bytes.as_bytes());
            } else {
                let converter = match &bytes_type {
                    Some(converter) => converter,
                    None => bytes_type.insert(py.import("builtins")?.getattr("bytes")?),
                };
                let converted = converter.call1((item,))?;
                joined.extend_from_slice(converted.cast::<PyBytes>()?.as_bytes());
            }
        }
        self.core
            .try_write_buffer(joined)
            .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
        Ok(())
    }

    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "PyStreamTransport")
    )]
    fn close(&self) -> PyResult<()> {
        self.core.flush_pending_direct_write();
        #[cfg(windows)]
        self.core.queue_pending_direct_write();
        self.core.set_closing();
        if let Some(fd) = self.core.runtime_socket_fd() {
            let _ = stop_socket_reader_nowait(&self.core, fd);
        }
        if self.core.direct_writer.is_none() {
            let _ = self.core.writer_tx.send(WriterCommand::Close);
            return Ok(());
        }
        if self.core.writer_is_still_lazy() {
            if let Some(writer) = &self.core.direct_writer {
                let writer = writer.lock().expect("poisoned direct tasked writer");
                if let Some(writer) = writer.as_ref() {
                    // `close()` is graceful: stop producing bytes, then let
                    // the kernel deliver everything already accepted into its
                    // send buffer. `shutdown(Both)` can discard that tail on
                    // some platforms and is reserved for `abort()`.
                    let _ = writer.shutdown_write();
                }
            }
            let _ = self.core.writer_tx.send(WriterCommand::Stop);
            let _ = self.core.connection_lost(None);
            return Ok(());
        }

        let _ = self.core.writer_tx.send(WriterCommand::Close);
        Ok(())
    }

    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "PyStreamTransport")
    )]
    fn abort(&self) -> PyResult<()> {
        self.core.discard_pending_direct_write();
        self.core.set_closing();
        if let Some(fd) = self.core.runtime_socket_fd() {
            let _ = stop_socket_reader_nowait(&self.core, fd);
        }
        if self.core.direct_writer.is_none() {
            let _ = self.core.writer_tx.send(WriterCommand::Abort);
            return Ok(());
        }
        if let Some(writer) = &self.core.direct_writer {
            let writer = writer.lock().expect("poisoned direct tasked writer");
            if let Some(writer) = writer.as_ref() {
                let _ = writer.shutdown_close();
            }
        }
        let _ = self.core.writer_tx.send(WriterCommand::Abort);
        let _ = self.core.connection_lost(None);
        Ok(())
    }

    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "PyStreamTransport")
    )]
    fn is_closing(&self) -> bool {
        self.core.is_closing()
    }

    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "PyStreamTransport")
    )]
    fn can_write_eof(&self) -> bool {
        self.core.can_write_eof()
    }

    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "PyStreamTransport")
    )]
    fn write_eof(&self) -> PyResult<()> {
        if !self.core.can_write_eof() {
            return Err(PyRuntimeError::new_err(
                "transport does not support write_eof",
            ));
        }
        self.core.flush_pending_direct_write();
        #[cfg(windows)]
        self.core.queue_pending_direct_write();
        self.core.mark_write_eof();
        if self.core.direct_writer.is_some() && self.core.writer_is_still_lazy() {
            if let Some(writer) = &self.core.direct_writer {
                let writer = writer.lock().expect("poisoned direct tasked writer");
                match writer.as_ref() {
                    Some(TaskedDirectWriter::Tcp(stream)) => {
                        let _ = shutdown_tcp_stream(stream, Shutdown::Write);
                    }
                    #[cfg(unix)]
                    Some(TaskedDirectWriter::Unix(stream)) => {
                        let _ = shutdown_unix_stream(stream, Shutdown::Write);
                    }
                    None => {}
                }
            }
            if self.core.close_on_write_eof() {
                let _ = self.core.connection_lost(None);
            }
            return Ok(());
        }
        let _ = self.core.writer_tx.send(WriterCommand::WriteEof);
        Ok(())
    }

    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "PyStreamTransport")
    )]
    #[pyo3(signature=(name, default=None))]
    fn get_extra_info(&self, py: Python<'_>, name: &str, default: Option<Py<PyAny>>) -> Py<PyAny> {
        self.core
            .get_extra(py, name)
            .unwrap_or_else(|| default.unwrap_or_else(|| py.None()))
    }

    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "PyStreamTransport")
    )]
    fn get_protocol(&self, py: Python<'_>) -> Py<PyAny> {
        self.core.get_protocol(py)
    }

    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "PyStreamTransport")
    )]
    fn set_protocol(&self, py: Python<'_>, protocol: Py<PyAny>) {
        self.core
            .set_protocol(py, protocol)
            .expect("failed to update transport protocol");
    }

    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "PyStreamTransport")
    )]
    fn pause_reading(&self) {
        self.core.pause_reading();
    }

    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "PyStreamTransport")
    )]
    fn resume_reading(&self) {
        self.core.resume_reading();
    }

    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "PyStreamTransport")
    )]
    fn is_reading(&self) -> bool {
        self.core.is_reading()
    }

    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "PyStreamTransport")
    )]
    fn get_write_buffer_size(&self) -> usize {
        self.core.get_write_buffer_size()
    }

    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "PyStreamTransport")
    )]
    fn get_write_buffer_limits(&self) -> (usize, usize) {
        self.core.get_write_buffer_limits()
    }

    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "PyStreamTransport")
    )]
    #[pyo3(signature=(high=None, low=None))]
    fn set_write_buffer_limits(&self, high: Option<usize>, low: Option<usize>) -> PyResult<()> {
        self.core.set_write_buffer_limits(high, low)
    }

    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "PyStreamTransport")
    )]
    fn __repr__(&self) -> String {
        format!("<StreamTransport closing={}>", self.is_closing())
    }
}
