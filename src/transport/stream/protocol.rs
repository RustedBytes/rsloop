//! Binding a Python protocol object to a transport, and the stream fast paths.
//!
//! `build_protocol_callbacks` resolves the protocol's methods once at
//! construction so the per-event path is a call on a cached `Py<PyAny>` rather
//! than an attribute lookup.
//!
//! `StreamReaderFastPath` is the bigger win: when the protocol is one of the
//! shapes we recognise — the native `PyFastStreamProtocol`, an object carrying
//! `_rsloop_fast_reader`, or `asyncio.streams.StreamReaderProtocol` — incoming
//! data is pushed straight into the reader's buffer, skipping the round trip
//! through `data_received`.

use pyo3::{
    exceptions::PyRuntimeError,
    prelude::*,
    sync::critical_section::with_critical_section,
    types::{PyByteArray, PyByteArrayMethods},
};

use super::{
    PyStreamTransport,
    buffers::ReadBufferPool,
    fast::{PyFastStreamProtocol, PyFastStreamReader},
};
use crate::python_names;

pub(super) struct ProtocolCallbacks {
    pub(super) connection_made: Py<PyAny>,
    pub(super) data_received: Option<Py<PyAny>>,
    pub(super) eof_received: Option<Py<PyAny>>,
    pub(super) connection_lost: Py<PyAny>,
    pub(super) pause_writing: Py<PyAny>,
    pub(super) resume_writing: Py<PyAny>,
    pub(super) get_buffer: Option<Py<PyAny>>,
    pub(super) buffer_updated: Option<Py<PyAny>>,
    pub(super) stream_reader_fast_path: Option<StreamReaderFastPath>,
}

impl ProtocolCallbacks {
    /// No protocol callbacks may run after connection_lost has been delivered.
    /// Release cached bound methods and fast readers as well as the protocol.
    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "ProtocolCallbacks"))]
    pub(super) fn cleared(py: Python<'_>) -> Self {
        Self {
            connection_made: py.None(),
            data_received: None,
            eof_received: None,
            connection_lost: py.None(),
            pause_writing: py.None(),
            resume_writing: py.None(),
            get_buffer: None,
            buffer_updated: None,
            stream_reader_fast_path: None,
        }
    }
}

pub(super) enum StreamReaderFastPath {
    Native {
        protocol: Py<PyFastStreamProtocol>,
        reader: Py<PyFastStreamReader>,
    },
    Generic {
        protocol: Option<Py<PyAny>>,
        reader: Py<PyAny>,
        buffer: Py<PyAny>,
        limit: usize,
    },
}

impl StreamReaderFastPath {
    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "StreamReaderFastPath")
    )]
    pub(super) fn clone_ref(&self, py: Python<'_>) -> Self {
        match self {
            Self::Native { protocol, reader } => Self::Native {
                protocol: protocol.clone_ref(py),
                reader: reader.clone_ref(py),
            },
            Self::Generic {
                protocol,
                reader,
                buffer,
                limit,
            } => Self::Generic {
                protocol: protocol.as_ref().map(|value| value.clone_ref(py)),
                reader: reader.clone_ref(py),
                buffer: buffer.clone_ref(py),
                limit: *limit,
            },
        }
    }

    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "StreamReaderFastPath")
    )]
    pub(super) fn connection_made(
        &self,
        py: Python<'_>,
        transport: Py<PyStreamTransport>,
    ) -> PyResult<bool> {
        match self {
            Self::Native { protocol, .. } => {
                PyFastStreamProtocol::handle_connection_made(
                    protocol.clone_ref(py),
                    py,
                    transport.into_any(),
                )?;
                Ok(true)
            }
            Self::Generic {
                protocol, reader, ..
            } => {
                let has_client_connected_cb = protocol.as_ref().is_some_and(|protocol| {
                    protocol
                        .bind(py)
                        .getattr("_client_connected_cb")
                        .map(|value| !value.is_none())
                        .unwrap_or(true)
                });
                if has_client_connected_cb {
                    return Ok(false);
                }

                reader
                    .bind(py)
                    .setattr("_transport", transport.clone_ref(py).into_any())?;
                if let Some(protocol) = protocol.as_ref() {
                    protocol
                        .bind(py)
                        .setattr("_transport", transport.into_any())?;
                }
                Ok(true)
            }
        }
    }

    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "StreamReaderFastPath")
    )]
    pub(super) fn feed_data(&self, py: Python<'_>, data: &[u8]) -> PyResult<()> {
        match self {
            Self::Native { reader, .. } => reader.borrow_mut(py).feed_data_internal(py, data),
            Self::Generic {
                reader,
                buffer,
                limit,
                ..
            } => {
                if data.is_empty() {
                    return Ok(());
                }

                let reader = reader.bind(py);
                let buffer = buffer.bind(py).cast::<PyByteArray>()?;
                if reader.getattr("_eof")?.extract::<bool>()? {
                    return Err(PyRuntimeError::new_err("feed_data after feed_eof"));
                }

                // The bytearray is a Python-visible object, so the size read,
                // the resize, and the copy have to be one
                // atomic step: on a free-threaded interpreter
                // another thread holding the same reader
                // could otherwise resize between them and leave `as_bytes_mut`
                // pointing into a freed allocation. CPython locks the same
                // per-object mutex for `bytearray`'s own mutations, so taking
                // the critical section here serializes against
                // them. It compiles away to a direct call on
                // GIL-enabled builds.
                let end = with_critical_section(buffer.as_any(), || -> PyResult<usize> {
                    let start = buffer.len();
                    let end = start + data.len();
                    buffer.resize(end)?;
                    // SAFETY: The bytearray was just resized to `end`, so
                    // `start..end` is in bounds, and the
                    // critical section keeps any other thread
                    // from resizing it for the duration of this copy.
                    unsafe {
                        buffer.as_bytes_mut()[start..end].copy_from_slice(data);
                    }
                    Ok(end)
                })?;

                let waiter = reader.getattr("_waiter")?;
                if !waiter.is_none() {
                    reader.setattr("_waiter", py.None())?;
                    if !waiter.call_method0("cancelled")?.extract::<bool>()? {
                        waiter.call_method1("set_result", (py.None(),))?;
                    }
                }

                let transport = reader.getattr("_transport")?;
                let paused = reader.getattr("_paused")?.extract::<bool>()?;
                if !transport.is_none() && !paused && end > 2 * limit {
                    match transport.call_method0(python_names::pause_reading(py)) {
                        Ok(_) => {
                            reader.setattr("_paused", true)?;
                        }
                        Err(err)
                            if err
                                .is_instance_of::<pyo3::exceptions::PyNotImplementedError>(py) =>
                        {
                            reader.setattr("_transport", py.None())?;
                        }
                        Err(err) => return Err(err),
                    }
                }

                Ok(())
            }
        }
    }

    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "StreamReaderFastPath")
    )]
    pub(super) fn feed_owned_data(
        &self,
        py: Python<'_>,
        data: Vec<u8>,
        pool: &std::sync::Arc<ReadBufferPool>,
    ) -> PyResult<()> {
        match self {
            Self::Native { reader, .. } => reader
                .borrow_mut(py)
                .feed_owned_data_internal(py, data, pool),
            Self::Generic { .. } => {
                let result = self.feed_data(py, &data);
                pool.release(data);
                result
            }
        }
    }

    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "StreamReaderFastPath")
    )]
    pub(super) fn feed_eof(&self, py: Python<'_>) -> PyResult<()> {
        match self {
            Self::Native { reader, .. } => reader.borrow_mut(py).feed_eof_internal(py),
            Self::Generic { reader, .. } => {
                let reader = reader.bind(py);
                reader.setattr("_eof", true)?;
                let waiter = reader.getattr("_waiter")?;
                if !waiter.is_none() {
                    reader.setattr("_waiter", py.None())?;
                    if !waiter.call_method0("cancelled")?.extract::<bool>()? {
                        waiter.call_method1("set_result", (py.None(),))?;
                    }
                }
                Ok(())
            }
        }
    }

    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "StreamReaderFastPath")
    )]
    pub(super) fn connection_lost(&self, py: Python<'_>, exc: Option<PyErr>) -> PyResult<()> {
        match self {
            Self::Native { protocol, .. } => protocol.borrow_mut(py).handle_connection_lost(
                py,
                exc.map(|err| err.value(py).clone().unbind().into_any()),
            ),
            Self::Generic {
                protocol, reader, ..
            } => {
                let Some(protocol) = protocol.as_ref() else {
                    return Ok(());
                };

                let protocol = protocol.bind(py);
                protocol.setattr("_connection_lost", true)?;

                match exc {
                    Some(err) => {
                        let err_value = err.value(py).clone().unbind().into_any();
                        reader
                            .bind(py)
                            .setattr("_exception", err_value.clone_ref(py))?;
                        let waiter = reader.bind(py).getattr("_waiter")?;
                        if !waiter.is_none() {
                            reader.bind(py).setattr("_waiter", py.None())?;
                            if !waiter.call_method0("cancelled")?.extract::<bool>()? {
                                waiter.call_method1("set_exception", (err_value.clone_ref(py),))?;
                            }
                        }
                        let closed = protocol.getattr("_closed")?;
                        if !closed.call_method0("done")?.extract::<bool>()? {
                            closed.call_method1("set_exception", (err_value.clone_ref(py),))?;
                        }
                        if protocol.getattr("_paused")?.extract::<bool>()? {
                            let waiters = protocol.getattr("_drain_waiters")?;
                            for waiter in waiters.try_iter()? {
                                let waiter = waiter?;
                                if !waiter.call_method0("done")?.extract::<bool>()? {
                                    waiter.call_method1(
                                        "set_exception",
                                        (err_value.clone_ref(py),),
                                    )?;
                                }
                            }
                        }
                    }
                    None => {
                        self.feed_eof(py)?;
                        let closed = protocol.getattr("_closed")?;
                        if !closed.call_method0("done")?.extract::<bool>()? {
                            closed.call_method1("set_result", (py.None(),))?;
                        }
                        if protocol.getattr("_paused")?.extract::<bool>()? {
                            let waiters = protocol.getattr("_drain_waiters")?;
                            for waiter in waiters.try_iter()? {
                                let waiter = waiter?;
                                if !waiter.call_method0("done")?.extract::<bool>()? {
                                    waiter.call_method1("set_result", (py.None(),))?;
                                }
                            }
                        }
                    }
                }

                protocol.setattr("_transport", py.None())?;
                protocol.setattr("_task", py.None())?;
                Ok(())
            }
        }
    }

    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "StreamReaderFastPath")
    )]
    pub(super) fn eof_received(&self, py: Python<'_>) -> PyResult<bool> {
        match self {
            Self::Native { protocol, .. } => protocol.borrow_mut(py).eof_received(py),
            Self::Generic { .. } => {
                self.feed_eof(py)?;
                Ok(true)
            }
        }
    }
}

#[cfg_attr(feature = "profile", hotpath::measure)]
pub(super) fn build_protocol_callbacks(
    py: Python<'_>,
    protocol: &Py<PyAny>,
) -> PyResult<ProtocolCallbacks> {
    let bound = protocol.bind(py);
    let stream_reader_fast_path = stream_reader_fast_path(py, bound)?;
    if matches!(
        &stream_reader_fast_path,
        Some(StreamReaderFastPath::Native { .. })
    ) {
        // Native streams handle these lifecycle and read callbacks directly
        // in Rust. Only bind callbacks that remain reachable.
        return Ok(ProtocolCallbacks {
            connection_made: protocol.clone_ref(py),
            data_received: None,
            eof_received: None,
            connection_lost: protocol.clone_ref(py),
            pause_writing: bound.getattr(python_names::pause_writing(py))?.unbind(),
            resume_writing: bound.getattr(python_names::resume_writing(py))?.unbind(),
            get_buffer: None,
            buffer_updated: None,
            stream_reader_fast_path,
        });
    }
    let data_received = match bound.getattr("data_received") {
        Ok(callback) => Some(callback.unbind()),
        Err(_) => None,
    };
    let eof_received = match bound.getattr("eof_received") {
        Ok(callback) => Some(callback.unbind()),
        Err(_) => None,
    };
    let get_buffer = match bound.getattr("get_buffer") {
        Ok(callback) => Some(callback.unbind()),
        Err(_) => None,
    };
    let buffer_updated = match bound.getattr("buffer_updated") {
        Ok(callback) => Some(callback.unbind()),
        Err(_) => None,
    };
    Ok(ProtocolCallbacks {
        connection_made: bound.getattr("connection_made")?.unbind(),
        data_received,
        eof_received,
        connection_lost: bound.getattr("connection_lost")?.unbind(),
        pause_writing: bound.getattr(python_names::pause_writing(py))?.unbind(),
        resume_writing: bound.getattr(python_names::resume_writing(py))?.unbind(),
        get_buffer,
        buffer_updated,
        stream_reader_fast_path,
    })
}

#[cfg_attr(feature = "profile", hotpath::measure)]
pub(super) fn stream_reader_fast_path(
    py: Python<'_>,
    protocol: &Bound<'_, PyAny>,
) -> PyResult<Option<StreamReaderFastPath>> {
    if let Some(native) = native_stream_reader_fast_path(py, protocol)? {
        return Ok(Some(native));
    }
    if let Some(generic) = generic_stream_reader_fast_path(protocol)? {
        return Ok(Some(generic));
    }
    asyncio_stream_reader_fast_path(py, protocol)
}

#[cfg_attr(feature = "profile", hotpath::measure)]
pub(super) fn native_stream_reader_fast_path(
    py: Python<'_>,
    protocol: &Bound<'_, PyAny>,
) -> PyResult<Option<StreamReaderFastPath>> {
    let Ok(native_protocol) = protocol.extract::<Py<PyFastStreamProtocol>>() else {
        return Ok(None);
    };
    let reader = native_protocol.borrow(py).reader_ref(py);
    Ok(Some(StreamReaderFastPath::Native {
        protocol: native_protocol,
        reader,
    }))
}

#[cfg_attr(feature = "profile", hotpath::measure)]
pub(super) fn generic_stream_reader_fast_path(
    protocol: &Bound<'_, PyAny>,
) -> PyResult<Option<StreamReaderFastPath>> {
    let Ok(reader) = protocol.getattr("_rsloop_fast_reader") else {
        return Ok(None);
    };
    if reader.is_none() {
        return Ok(None);
    }
    stream_reader_fast_path_from_reader(Some(protocol.clone().unbind()), reader)
}

#[cfg_attr(feature = "profile", hotpath::measure)]
pub(super) fn asyncio_stream_reader_fast_path(
    py: Python<'_>,
    protocol: &Bound<'_, PyAny>,
) -> PyResult<Option<StreamReaderFastPath>> {
    let asyncio_streams = py.import("asyncio.streams")?;
    let stream_reader_protocol_cls = asyncio_streams.getattr("StreamReaderProtocol")?;
    if !protocol.is_instance(&stream_reader_protocol_cls)? {
        return Ok(None);
    }

    let reader = protocol.getattr("_stream_reader")?;
    if reader.is_none() {
        return Ok(None);
    }
    stream_reader_fast_path_from_reader(Some(protocol.clone().unbind()), reader)
}

#[cfg_attr(feature = "profile", hotpath::measure)]
pub(super) fn stream_reader_fast_path_from_reader(
    protocol: Option<Py<PyAny>>,
    reader: Bound<'_, PyAny>,
) -> PyResult<Option<StreamReaderFastPath>> {
    let buffer = reader.getattr("_buffer")?;
    let limit = reader.getattr("_limit")?.extract::<usize>()?;

    Ok(Some(StreamReaderFastPath::Generic {
        protocol,
        reader: reader.unbind(),
        buffer: buffer.unbind(),
        limit,
    }))
}
