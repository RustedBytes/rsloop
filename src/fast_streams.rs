use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyByteArray, PyBytes, PyDict, PyTuple};

use crate::python_api::PyLoop;
use crate::python_names;
use crate::stream_transport::task_locals_for_loop;

const DEFAULT_STREAM_LIMIT: usize = 65_536;

struct ReadBuffer {
    bytes: Vec<u8>,
    start: usize,
}

impl ReadBuffer {
    fn with_capacity(capacity: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(capacity),
            start: 0,
        }
    }

    fn len(&self) -> usize {
        self.bytes.len().saturating_sub(self.start)
    }

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    fn unread(&self) -> &[u8] {
        &self.bytes[self.start..]
    }

    fn extend(&mut self, data: &[u8]) {
        if data.is_empty() {
            return;
        }
        self.compact_if_needed();
        self.bytes.reserve(data.len());
        self.bytes.extend_from_slice(data);
    }

    fn consume(&mut self, n: usize) {
        self.start = (self.start + n).min(self.bytes.len());
        self.compact_if_needed();
    }

    fn consume_all(&mut self) {
        self.start = self.bytes.len();
        self.compact_if_needed();
    }

    fn replace(&mut self, data: &[u8]) {
        self.bytes.clear();
        self.bytes.extend_from_slice(data);
        self.start = 0;
    }

    fn compact_if_needed(&mut self) {
        if self.start == 0 {
            return;
        }
        if self.start == self.bytes.len() {
            self.bytes.clear();
            self.start = 0;
            return;
        }
        if self.start >= 4096 && self.start * 2 >= self.bytes.len() {
            self.bytes.copy_within(self.start.., 0);
            self.bytes.truncate(self.bytes.len() - self.start);
            self.start = 0;
        }
    }
}

#[derive(Clone)]
enum ReadWaitKind {
    Any(usize),
    Exact(usize),
    All,
}

struct ReadWaiter {
    future: Py<PyAny>,
    kind: ReadWaitKind,
}

#[pyclass(module = "rsloop._loop")]
pub struct PyFastStreamReader {
    loop_obj: Py<PyAny>,
    limit: usize,
    buffer: ReadBuffer,
    waiter: Option<ReadWaiter>,
    transport: Py<PyAny>,
    paused: bool,
    eof: bool,
    exception: Option<Py<PyAny>>,
}

impl PyFastStreamReader {
    fn set_future_result_or_ignore_cancelled(
        py: Python<'_>,
        future: &Py<PyAny>,
        value: Py<PyAny>,
    ) -> PyResult<()> {
        let future = future.bind(py);
        match python_names::call_method1(py, future, python_names::set_result(py), value.bind(py)) {
            Ok(_) => Ok(()),
            Err(err) => {
                if python_names::call_method0(py, future, python_names::cancelled(py))?
                    .bind(py)
                    .extract::<bool>()?
                {
                    Ok(())
                } else {
                    Err(err)
                }
            }
        }
    }

    fn set_future_exception_or_ignore_cancelled(
        py: Python<'_>,
        future: &Py<PyAny>,
        exc: Py<PyAny>,
    ) -> PyResult<()> {
        let future = future.bind(py);
        match python_names::call_method1(py, future, python_names::set_exception(py), exc.bind(py))
        {
            Ok(_) => Ok(()),
            Err(err) => {
                if python_names::call_method0(py, future, python_names::cancelled(py))?
                    .bind(py)
                    .extract::<bool>()?
                {
                    Ok(())
                } else {
                    Err(err)
                }
            }
        }
    }

    fn new_with_loop(py: Python<'_>, loop_obj: Py<PyAny>, limit: usize) -> PyResult<Self> {
        if limit == 0 {
            return Err(PyValueError::new_err("Limit cannot be <= 0"));
        }

        Ok(Self {
            loop_obj,
            limit,
            buffer: ReadBuffer::with_capacity(limit.max(4096)),
            waiter: None,
            transport: py.None(),
            paused: false,
            eof: false,
            exception: None,
        })
    }

    fn create_future(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        python_names::call_method0(py, self.loop_obj.bind(py), python_names::create_future(py))
    }

    fn ready_result_future(&self, py: Python<'_>, value: Py<PyAny>) -> PyResult<Py<PyAny>> {
        let future = self.create_future(py)?;
        python_names::call_method1(
            py,
            future.bind(py),
            python_names::set_result(py),
            value.bind(py),
        )?;
        Ok(future)
    }

    fn ready_exception_future(&self, py: Python<'_>, exc: Py<PyAny>) -> PyResult<Py<PyAny>> {
        let future = self.create_future(py)?;
        python_names::call_method1(
            py,
            future.bind(py),
            python_names::set_exception(py),
            exc.bind(py),
        )?;
        Ok(future)
    }

    fn bytes_object(py: Python<'_>, data: &[u8]) -> Py<PyAny> {
        PyBytes::new(py, data).unbind().into_any()
    }

    fn unread_bytes_object(&mut self, py: Python<'_>, n: usize) -> Py<PyAny> {
        let len = self.buffer.len().min(n);
        let value = Self::bytes_object(py, &self.buffer.unread()[..len]);
        self.buffer.consume(len);
        value
    }

    fn unread_all_bytes_object(&mut self, py: Python<'_>) -> Py<PyAny> {
        let value = Self::bytes_object(py, self.buffer.unread());
        self.buffer.consume_all();
        value
    }

    fn incomplete_read_error(
        py: Python<'_>,
        partial: &[u8],
        expected: usize,
    ) -> PyResult<Py<PyAny>> {
        let asyncio = py.import("asyncio")?;
        Ok(asyncio
            .getattr("IncompleteReadError")?
            .call1((PyBytes::new(py, partial), expected))?
            .unbind())
    }

    fn maybe_resume_transport(&mut self, py: Python<'_>) -> PyResult<()> {
        if self.paused && self.buffer.len() <= self.limit && !self.transport.bind(py).is_none() {
            self.paused = false;
            python_names::call_method0(
                py,
                self.transport.bind(py),
                python_names::resume_reading(py),
            )?;
        }
        Ok(())
    }

    fn maybe_pause_transport(&mut self, py: Python<'_>) -> PyResult<()> {
        if !self.transport.bind(py).is_none() && !self.paused && self.buffer.len() > 2 * self.limit
        {
            match python_names::call_method0(
                py,
                self.transport.bind(py),
                python_names::pause_reading(py),
            ) {
                Ok(_) => {
                    self.paused = true;
                }
                Err(err) if err.is_instance_of::<pyo3::exceptions::PyNotImplementedError>(py) => {
                    self.transport = py.None();
                }
                Err(err) => return Err(err),
            }
        }
        Ok(())
    }

    fn maybe_complete_waiter(&mut self, py: Python<'_>) -> PyResult<()> {
        let Some((future, kind)) = self
            .waiter
            .as_ref()
            .map(|waiter| (waiter.future.clone_ref(py), waiter.kind.clone()))
        else {
            return Ok(());
        };

        if let Some(exc) = self.exception.as_ref() {
            self.waiter = None;
            Self::set_future_exception_or_ignore_cancelled(py, &future, exc.clone_ref(py))?;
            return Ok(());
        }

        match kind {
            ReadWaitKind::Any(n) => {
                if self.buffer.is_empty() && !self.eof {
                    return Ok(());
                }
                self.waiter = None;
                let data = self.unread_bytes_object(py, n);
                self.maybe_resume_transport(py)?;
                Self::set_future_result_or_ignore_cancelled(py, &future, data)?;
            }
            ReadWaitKind::Exact(n) => {
                if self.buffer.len() >= n {
                    self.waiter = None;
                    let data = self.unread_bytes_object(py, n);
                    self.maybe_resume_transport(py)?;
                    Self::set_future_result_or_ignore_cancelled(py, &future, data)?;
                    return Ok(());
                }
                if !self.eof {
                    return Ok(());
                }
                let err = Self::incomplete_read_error(py, self.buffer.unread(), n)?;
                self.buffer.consume_all();
                self.waiter = None;
                Self::set_future_exception_or_ignore_cancelled(py, &future, err)?;
            }
            ReadWaitKind::All => {
                if !self.eof {
                    return Ok(());
                }
                self.waiter = None;
                let data = self.unread_all_bytes_object(py);
                Self::set_future_result_or_ignore_cancelled(py, &future, data)?;
            }
        }

        Ok(())
    }

    fn start_waiter(
        &mut self,
        py: Python<'_>,
        func_name: &str,
        kind: ReadWaitKind,
    ) -> PyResult<Py<PyAny>> {
        if self.waiter.is_some() {
            return Err(PyValueError::new_err(format!(
                "{func_name}() called while another coroutine is already waiting for incoming data"
            )));
        }
        if self.paused && !self.transport.bind(py).is_none() {
            self.paused = false;
            python_names::call_method0(
                py,
                self.transport.bind(py),
                python_names::resume_reading(py),
            )?;
        }
        let future = self.create_future(py)?;
        self.waiter = Some(ReadWaiter {
            future: future.clone_ref(py),
            kind,
        });
        Ok(future)
    }

    pub(crate) fn set_transport_obj(
        &mut self,
        py: Python<'_>,
        transport: Py<PyAny>,
    ) -> PyResult<()> {
        self.transport = transport;
        self.maybe_resume_transport(py)
    }

    pub(crate) fn feed_data_internal(&mut self, py: Python<'_>, data: &[u8]) -> PyResult<()> {
        if self.eof {
            return Err(PyValueError::new_err("feed_data after feed_eof"));
        }
        self.buffer.extend(data);
        self.maybe_complete_waiter(py)?;
        self.maybe_pause_transport(py)
    }

    pub(crate) fn feed_eof_internal(&mut self, py: Python<'_>) -> PyResult<()> {
        self.eof = true;
        self.maybe_complete_waiter(py)
    }

    pub(crate) fn set_exception_internal(
        &mut self,
        py: Python<'_>,
        exc: Py<PyAny>,
    ) -> PyResult<()> {
        self.exception = Some(exc);
        self.maybe_complete_waiter(py)
    }

    fn build_read_future(&mut self, py: Python<'_>, n: isize) -> PyResult<Py<PyAny>> {
        if let Some(exc) = self.exception.as_ref() {
            return self.ready_exception_future(py, exc.clone_ref(py));
        }
        if n == 0 {
            return self.ready_result_future(py, Self::bytes_object(py, &[]));
        }
        if n < 0 {
            if self.eof {
                let data = self.unread_all_bytes_object(py);
                return self.ready_result_future(py, data);
            }
            return self.start_waiter(py, "read", ReadWaitKind::All);
        }
        if !self.buffer.is_empty() || self.eof {
            let data = self.unread_bytes_object(py, n as usize);
            self.maybe_resume_transport(py)?;
            return self.ready_result_future(py, data);
        }
        self.start_waiter(py, "read", ReadWaitKind::Any(n as usize))
    }

    fn build_readexactly_future(&mut self, py: Python<'_>, n: usize) -> PyResult<Py<PyAny>> {
        if let Some(exc) = self.exception.as_ref() {
            return self.ready_exception_future(py, exc.clone_ref(py));
        }
        if n == 0 {
            return self.ready_result_future(py, Self::bytes_object(py, &[]));
        }
        if self.buffer.len() >= n {
            let data = self.unread_bytes_object(py, n);
            self.maybe_resume_transport(py)?;
            return self.ready_result_future(py, data);
        }
        if self.eof {
            let err = Self::incomplete_read_error(py, self.buffer.unread(), n)?;
            self.buffer.consume_all();
            return self.ready_exception_future(py, err);
        }
        self.start_waiter(py, "readexactly", ReadWaitKind::Exact(n))
    }
}

#[pymethods]
impl PyFastStreamReader {
    #[new]
    #[pyo3(signature = (limit=DEFAULT_STREAM_LIMIT, loop_obj=None))]
    fn py_new(py: Python<'_>, limit: usize, loop_obj: Option<Py<PyAny>>) -> PyResult<Self> {
        let loop_obj = match loop_obj {
            Some(loop_obj) => loop_obj,
            None => py
                .import("asyncio.events")?
                .call_method0("get_event_loop")?
                .unbind(),
        };
        Self::new_with_loop(py, loop_obj, limit)
    }

    #[getter(_rsloop_fast_reader)]
    fn get_rsloop_fast_reader(&self, py: Python<'_>) -> Py<PyAny> {
        py.None()
    }

    #[getter(_loop)]
    fn get_loop_obj(&self, py: Python<'_>) -> Py<PyAny> {
        self.loop_obj.clone_ref(py)
    }

    #[getter(_limit)]
    fn get_limit(&self) -> usize {
        self.limit
    }

    #[getter(_buffer)]
    fn get_buffer(&self, py: Python<'_>) -> Py<PyAny> {
        PyByteArray::new(py, self.buffer.unread())
            .unbind()
            .into_any()
    }

    #[setter(_buffer)]
    fn set_buffer(&mut self, value: &Bound<'_, PyAny>) -> PyResult<()> {
        let data: Vec<u8> = value.extract()?;
        self.buffer.replace(&data);
        Ok(())
    }

    #[getter(_waiter)]
    fn get_waiter(&self, py: Python<'_>) -> Py<PyAny> {
        self.waiter
            .as_ref()
            .map(|waiter| waiter.future.clone_ref(py))
            .unwrap_or_else(|| py.None())
    }

    #[getter(_transport)]
    fn get_transport(&self, py: Python<'_>) -> Py<PyAny> {
        self.transport.clone_ref(py)
    }

    #[getter(_paused)]
    fn get_paused(&self) -> bool {
        self.paused
    }

    #[getter(_eof)]
    fn get_eof(&self) -> bool {
        self.eof
    }

    #[getter(_exception)]
    fn get_exception_obj(&self, py: Python<'_>) -> Py<PyAny> {
        self.exception
            .as_ref()
            .map(|exc| exc.clone_ref(py))
            .unwrap_or_else(|| py.None())
    }

    fn exception(&self, py: Python<'_>) -> Py<PyAny> {
        self.get_exception_obj(py)
    }

    fn set_exception(&mut self, py: Python<'_>, exc: Py<PyAny>) -> PyResult<()> {
        self.set_exception_internal(py, exc)
    }

    fn set_transport_public(&mut self, py: Python<'_>, transport: Py<PyAny>) -> PyResult<()> {
        self.set_transport_obj(py, transport)
    }

    fn feed_data(&mut self, py: Python<'_>, data: &[u8]) -> PyResult<()> {
        self.feed_data_internal(py, data)
    }

    fn feed_eof(&mut self, py: Python<'_>) -> PyResult<()> {
        self.feed_eof_internal(py)
    }

    fn at_eof(&self) -> bool {
        self.eof && self.buffer.is_empty()
    }

    #[pyo3(signature = (n=-1))]
    fn read(&mut self, py: Python<'_>, n: isize) -> PyResult<Py<PyAny>> {
        self.build_read_future(py, n)
    }

    fn readexactly(&mut self, py: Python<'_>, n: usize) -> PyResult<Py<PyAny>> {
        self.build_readexactly_future(py, n)
    }
}

#[pyclass(module = "rsloop._loop")]
pub struct PyFastStreamProtocol {
    loop_obj: Py<PyAny>,
    reader: Py<PyFastStreamReader>,
    client_connected_cb: Py<PyAny>,
    transport: Py<PyAny>,
    task: Py<PyAny>,
    closed: Py<PyAny>,
    ready_none: Py<PyAny>,
    paused: bool,
    drain_waiters: Vec<Py<PyAny>>,
    connection_lost: bool,
}

impl PyFastStreamProtocol {
    fn new_with_loop(
        py: Python<'_>,
        loop_obj: Py<PyAny>,
        reader: Py<PyFastStreamReader>,
        client_connected_cb: Py<PyAny>,
    ) -> PyResult<Self> {
        let closed =
            python_names::call_method0(py, loop_obj.bind(py), python_names::create_future(py))?;
        let ready_none =
            python_names::call_method0(py, loop_obj.bind(py), python_names::create_future(py))?;
        python_names::call_method1(
            py,
            ready_none.bind(py),
            python_names::set_result(py),
            py.None().bind(py),
        )?;
        Ok(Self {
            closed,
            ready_none,
            loop_obj,
            reader,
            client_connected_cb,
            transport: py.None(),
            task: py.None(),
            paused: false,
            drain_waiters: Vec::new(),
            connection_lost: false,
        })
    }

    fn has_client_connected_cb(&self, py: Python<'_>) -> bool {
        !self.client_connected_cb.bind(py).is_none()
    }

    pub(crate) fn reader_ref(&self, py: Python<'_>) -> Py<PyFastStreamReader> {
        self.reader.clone_ref(py)
    }

    fn ready_none_future(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        Ok(self.ready_none.clone_ref(py))
    }

    fn ready_exception_future(&self, py: Python<'_>, exc: Py<PyAny>) -> PyResult<Py<PyAny>> {
        let future = python_names::call_method0(
            py,
            self.loop_obj.bind(py),
            python_names::create_future(py),
        )?;
        python_names::call_method1(
            py,
            future.bind(py),
            python_names::set_exception(py),
            exc.bind(py),
        )?;
        Ok(future)
    }

    fn push_drain_waiter(&mut self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let future = python_names::call_method0(
            py,
            self.loop_obj.bind(py),
            python_names::create_future(py),
        )?;
        self.drain_waiters.push(future.clone_ref(py));
        Ok(future)
    }

    fn resolve_drain_waiters(&mut self, py: Python<'_>, exc: Option<Py<PyAny>>) -> PyResult<()> {
        for future in self.drain_waiters.drain(..) {
            let future = future.bind(py);
            if python_names::call_method0(py, future, python_names::done(py))?
                .bind(py)
                .extract::<bool>()?
            {
                continue;
            }
            match exc.as_ref() {
                Some(exc) => {
                    python_names::call_method1(
                        py,
                        future,
                        python_names::set_exception(py),
                        exc.bind(py),
                    )?;
                }
                None => {
                    python_names::call_method1(
                        py,
                        future,
                        python_names::set_result(py),
                        py.None().bind(py),
                    )?;
                }
            }
        }
        Ok(())
    }

    fn build_drain_future(
        &mut self,
        py: Python<'_>,
        reader_exception: Option<Py<PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        if let Some(exc) = reader_exception {
            return self.ready_exception_future(py, exc);
        }
        if self.connection_lost {
            let builtins = py.import("builtins")?;
            let exc = builtins
                .getattr("ConnectionResetError")?
                .call1(("Connection lost",))?
                .unbind();
            return self.ready_exception_future(py, exc);
        }
        if !self.paused {
            return self.ready_none_future(py);
        }
        self.push_drain_waiter(py)
    }

    pub(crate) fn handle_connection_made(
        slf: Py<Self>,
        py: Python<'_>,
        transport: Py<PyAny>,
    ) -> PyResult<()> {
        {
            let mut protocol = slf.borrow_mut(py);
            protocol.transport = transport.clone_ref(py);
            protocol
                .reader
                .borrow_mut(py)
                .set_transport_obj(py, transport.clone_ref(py))?;
            if !protocol.has_client_connected_cb(py) {
                return Ok(());
            }
        }

        let (loop_obj, callback, reader) = {
            let protocol = slf.borrow(py);
            (
                protocol.loop_obj.clone_ref(py),
                protocol.client_connected_cb.clone_ref(py),
                protocol.reader.clone_ref(py),
            )
        };
        let writer = Py::new(
            py,
            PyFastStreamWriter {
                transport: transport.clone_ref(py),
                protocol: slf.clone_ref(py),
                reader: reader.clone_ref(py),
            },
        )?;
        let result = callback.call1(py, (reader.clone_ref(py), writer))?;
        let asyncio = py.import("asyncio")?;
        if !asyncio
            .call_method1("iscoroutine", (result.clone_ref(py),))?
            .extract::<bool>()?
        {
            return Ok(());
        }

        let task = loop_obj.call_method1(py, "create_task", (result,))?;
        slf.borrow_mut(py).task = task.clone_ref(py);
        let done_cb = Py::new(
            py,
            PyFastClientDoneCallback {
                loop_obj,
                transport,
            },
        )?;
        task.call_method1(py, "add_done_callback", (done_cb,))?;
        Ok(())
    }

    pub(crate) fn handle_connection_lost(
        &mut self,
        py: Python<'_>,
        exc: Option<Py<PyAny>>,
    ) -> PyResult<()> {
        self.connection_lost = true;
        match exc {
            Some(exc) => {
                self.reader
                    .borrow_mut(py)
                    .set_exception_internal(py, exc.clone_ref(py))?;
                if !python_names::call_method0(py, self.closed.bind(py), python_names::done(py))?
                    .bind(py)
                    .extract::<bool>()?
                {
                    python_names::call_method1(
                        py,
                        self.closed.bind(py),
                        python_names::set_exception(py),
                        exc.bind(py),
                    )?;
                }
                self.resolve_drain_waiters(py, Some(exc))?;
            }
            None => {
                self.reader.borrow_mut(py).feed_eof_internal(py)?;
                if !python_names::call_method0(py, self.closed.bind(py), python_names::done(py))?
                    .bind(py)
                    .extract::<bool>()?
                {
                    python_names::call_method1(
                        py,
                        self.closed.bind(py),
                        python_names::set_result(py),
                        py.None().bind(py),
                    )?;
                }
                self.resolve_drain_waiters(py, None)?;
            }
        }
        self.transport = py.None();
        self.task = py.None();
        Ok(())
    }
}

#[pymethods]
impl PyFastStreamProtocol {
    #[new]
    #[pyo3(signature = (reader, client_connected_cb=None, loop_obj=None))]
    fn py_new(
        py: Python<'_>,
        reader: Py<PyFastStreamReader>,
        client_connected_cb: Option<Py<PyAny>>,
        loop_obj: Option<Py<PyAny>>,
    ) -> PyResult<Self> {
        let loop_obj = match loop_obj {
            Some(loop_obj) => loop_obj,
            None => py
                .import("asyncio.events")?
                .call_method0("get_event_loop")?
                .unbind(),
        };
        Self::new_with_loop(
            py,
            loop_obj,
            reader,
            client_connected_cb.unwrap_or_else(|| py.None()),
        )
    }

    #[getter(_rsloop_fast_reader)]
    fn get_rsloop_fast_reader(&self, py: Python<'_>) -> Py<PyAny> {
        self.reader.clone_ref(py).into_any()
    }

    fn connection_made(slf: Py<Self>, py: Python<'_>, transport: Py<PyAny>) -> PyResult<()> {
        Self::handle_connection_made(slf, py, transport)
    }

    fn pause_writing(&mut self) {
        self.paused = true;
    }

    fn resume_writing(&mut self, py: Python<'_>) -> PyResult<()> {
        self.paused = false;
        self.resolve_drain_waiters(py, None)
    }

    fn _drain_helper(&mut self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        self.build_drain_future(py, None)
    }

    fn data_received(&mut self, py: Python<'_>, data: &[u8]) -> PyResult<()> {
        self.reader.borrow_mut(py).feed_data_internal(py, data)
    }

    fn eof_received(&mut self, py: Python<'_>) -> PyResult<bool> {
        self.reader.borrow_mut(py).feed_eof_internal(py)?;
        Ok(true)
    }

    fn connection_lost(&mut self, py: Python<'_>, exc: Option<Py<PyAny>>) -> PyResult<()> {
        self.handle_connection_lost(py, exc)
    }
}

#[pyclass(module = "rsloop._loop")]
pub struct PyFastStreamWriter {
    transport: Py<PyAny>,
    protocol: Py<PyFastStreamProtocol>,
    reader: Py<PyFastStreamReader>,
}

#[pymethods]
impl PyFastStreamWriter {
    #[getter]
    fn transport(&self, py: Python<'_>) -> Py<PyAny> {
        self.transport.clone_ref(py)
    }

    fn write(&self, py: Python<'_>, data: &Bound<'_, PyAny>) -> PyResult<()> {
        self.transport.call_method1(py, "write", (data,))?;
        Ok(())
    }

    fn writelines(&self, py: Python<'_>, data: &Bound<'_, PyAny>) -> PyResult<()> {
        self.transport.call_method1(py, "writelines", (data,))?;
        Ok(())
    }

    fn write_eof(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        self.transport.call_method0(py, "write_eof")
    }

    fn can_write_eof(&self, py: Python<'_>) -> PyResult<bool> {
        self.transport
            .call_method0(py, "can_write_eof")?
            .extract(py)
    }

    fn close(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        self.transport.call_method0(py, "close")
    }

    fn is_closing(&self, py: Python<'_>) -> PyResult<bool> {
        self.transport.call_method0(py, "is_closing")?.extract(py)
    }

    #[pyo3(signature = (name, default=None))]
    fn get_extra_info(
        &self,
        py: Python<'_>,
        name: &str,
        default: Option<Py<PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        self.transport.call_method1(
            py,
            "get_extra_info",
            (name, default.unwrap_or_else(|| py.None())),
        )
    }

    fn wait_closed(&self, py: Python<'_>) -> Py<PyAny> {
        self.protocol.borrow(py).closed.clone_ref(py)
    }

    fn drain(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let reader_exception = self
            .reader
            .borrow(py)
            .exception
            .as_ref()
            .map(|exc| exc.clone_ref(py));
        self.protocol
            .borrow_mut(py)
            .build_drain_future(py, reader_exception)
    }
}

#[pyclass(module = "rsloop._loop")]
struct PyFastClientDoneCallback {
    loop_obj: Py<PyAny>,
    transport: Py<PyAny>,
}

#[pymethods]
impl PyFastClientDoneCallback {
    fn __call__(&self, py: Python<'_>, task: Py<PyAny>) -> PyResult<()> {
        if task.call_method0(py, "cancelled")?.extract::<bool>(py)? {
            self.transport.call_method0(py, "close")?;
            return Ok(());
        }

        let exc = task.call_method0(py, "exception")?;
        if exc.bind(py).is_none() {
            return Ok(());
        }

        let context = PyDict::new(py);
        context.set_item("message", "Unhandled exception in client_connected_cb")?;
        context.set_item("exception", exc.clone_ref(py))?;
        context.set_item("transport", self.transport.clone_ref(py))?;
        self.loop_obj
            .call_method1(py, "call_exception_handler", (context,))?;
        self.transport.call_method0(py, "close")?;
        Ok(())
    }
}

#[pyclass(module = "rsloop._loop")]
struct PyFastProtocolFactory {
    loop_obj: Py<PyAny>,
    limit: usize,
    client_connected_cb: Py<PyAny>,
}

#[pymethods]
impl PyFastProtocolFactory {
    fn __call__(&self, py: Python<'_>) -> PyResult<Py<PyFastStreamProtocol>> {
        let reader = Py::new(
            py,
            PyFastStreamReader::new_with_loop(py, self.loop_obj.clone_ref(py), self.limit)?,
        )?;
        Py::new(
            py,
            PyFastStreamProtocol::new_with_loop(
                py,
                self.loop_obj.clone_ref(py),
                reader,
                self.client_connected_cb.clone_ref(py),
            )?,
        )
    }
}

fn running_loop(py: Python<'_>) -> PyResult<Py<PyAny>> {
    Ok(py
        .import("asyncio.events")?
        .call_method0("get_running_loop")?
        .unbind())
}

fn call_asyncio_streams_function(
    py: Python<'_>,
    name: &str,
    args: &Bound<'_, PyTuple>,
    kwargs: Option<&Bound<'_, PyDict>>,
) -> PyResult<Py<PyAny>> {
    let module = py.import("asyncio.streams")?;
    Ok(module.getattr(name)?.call(args, kwargs)?.unbind())
}

fn kwargs_with_limit<'py>(
    py: Python<'py>,
    kwargs: Option<&Bound<'py, PyDict>>,
    limit: usize,
) -> PyResult<Bound<'py, PyDict>> {
    let dict = PyDict::new(py);
    if let Some(kwargs) = kwargs {
        for (key, value) in kwargs.iter() {
            dict.set_item(key, value)?;
        }
    }
    dict.set_item("limit", limit)?;
    Ok(dict)
}

fn native_stream_loop(
    py: Python<'_>,
    kwargs: Option<&Bound<'_, PyDict>>,
) -> PyResult<Option<Py<PyAny>>> {
    let loop_obj = running_loop(py)?;
    if !loop_obj.bind(py).is_instance_of::<PyLoop>() {
        return Ok(None);
    }
    if let Some(kwargs) = kwargs {
        if let Some(ssl) = kwargs.get_item("ssl")? {
            if !ssl.is_none() {
                return Ok(None);
            }
        }
    }
    Ok(Some(loop_obj))
}

#[pyfunction(signature = (host=None, port=None, *, limit=DEFAULT_STREAM_LIMIT, **kwargs))]
pub fn open_connection(
    py: Python<'_>,
    host: Option<Py<PyAny>>,
    port: Option<Py<PyAny>>,
    limit: usize,
    kwargs: Option<&Bound<'_, PyDict>>,
) -> PyResult<Py<PyAny>> {
    let host_obj = host
        .as_ref()
        .map(|value| value.clone_ref(py))
        .unwrap_or_else(|| py.None());
    let port_obj = port
        .as_ref()
        .map(|value| value.clone_ref(py))
        .unwrap_or_else(|| py.None());
    let args = PyTuple::new(py, [host_obj.clone_ref(py), port_obj.clone_ref(py)])?;

    let Some(loop_obj) = native_stream_loop(py, kwargs)? else {
        let kwargs = kwargs_with_limit(py, kwargs, limit)?;
        return call_asyncio_streams_function(py, "open_connection", &args, Some(&kwargs));
    };

    let locals = task_locals_for_loop(py, &loop_obj)?;
    let factory = Py::new(
        py,
        PyFastProtocolFactory {
            loop_obj: loop_obj.clone_ref(py),
            limit,
            client_connected_cb: py.None(),
        },
    )?;
    let kwargs = kwargs.map(|kwargs| {
        let copied = PyDict::new(py);
        for (key, value) in kwargs.iter() {
            let _ = copied.set_item(key, value);
        }
        copied
    });
    let create_args = PyTuple::new(py, [factory.into_any(), host_obj, port_obj])?;
    let awaitable = loop_obj.call_method(py, "create_connection", &create_args, kwargs.as_ref())?;

    Ok(pyo3_async_runtimes::async_std::future_into_py_with_locals(
        py,
        locals.clone(),
        async move {
            let created = Python::attach(|py| {
                pyo3_async_runtimes::into_future_with_locals(&locals, awaitable.bind(py).clone())
            })?
            .await?;

            Python::attach(|py| {
                let result = created.bind(py).cast::<PyTuple>()?;
                let transport = result.get_item(0)?.unbind();
                let protocol: Py<PyFastStreamProtocol> = result.get_item(1)?.extract()?;
                let reader = protocol.borrow(py).reader.clone_ref(py);
                let writer = Py::new(
                    py,
                    PyFastStreamWriter {
                        transport,
                        protocol,
                        reader: reader.clone_ref(py),
                    },
                )?;
                let output = PyTuple::new(py, [reader.into_any(), writer.into_any()])?;
                Ok(output.unbind().into_any())
            })
        },
    )?
    .unbind())
}

#[pyfunction(signature = (client_connected_cb, host=None, port=None, *, limit=DEFAULT_STREAM_LIMIT, **kwargs))]
pub fn start_server(
    py: Python<'_>,
    client_connected_cb: Py<PyAny>,
    host: Option<Py<PyAny>>,
    port: Option<Py<PyAny>>,
    limit: usize,
    kwargs: Option<&Bound<'_, PyDict>>,
) -> PyResult<Py<PyAny>> {
    let host_obj = host
        .as_ref()
        .map(|value| value.clone_ref(py))
        .unwrap_or_else(|| py.None());
    let port_obj = port
        .as_ref()
        .map(|value| value.clone_ref(py))
        .unwrap_or_else(|| py.None());
    let args = PyTuple::new(
        py,
        [
            client_connected_cb.clone_ref(py),
            host_obj.clone_ref(py),
            port_obj.clone_ref(py),
        ],
    )?;

    let Some(loop_obj) = native_stream_loop(py, kwargs)? else {
        let kwargs = kwargs_with_limit(py, kwargs, limit)?;
        return call_asyncio_streams_function(py, "start_server", &args, Some(&kwargs));
    };

    let locals = task_locals_for_loop(py, &loop_obj)?;
    let factory = Py::new(
        py,
        PyFastProtocolFactory {
            loop_obj: loop_obj.clone_ref(py),
            limit,
            client_connected_cb,
        },
    )?;
    let kwargs = kwargs.map(|kwargs| {
        let copied = PyDict::new(py);
        for (key, value) in kwargs.iter() {
            let _ = copied.set_item(key, value);
        }
        copied
    });
    let create_args = PyTuple::new(py, [factory.into_any(), host_obj, port_obj])?;
    let awaitable = loop_obj.call_method(py, "create_server", &create_args, kwargs.as_ref())?;

    Ok(pyo3_async_runtimes::async_std::future_into_py_with_locals(
        py,
        locals.clone(),
        async move {
            Python::attach(|py| {
                pyo3_async_runtimes::into_future_with_locals(&locals, awaitable.bind(py).clone())
            })?
            .await
        },
    )?
    .unbind())
}
