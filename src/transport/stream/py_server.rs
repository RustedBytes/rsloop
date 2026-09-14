//! The `Server` object Python holds.
//!
//! Mirrors `asyncio.Server`: `close`, `wait_closed`, `start_serving`, and the
//! `sockets` / `is_serving` accessors. The waiting methods return awaitables
//! bound to the server's own loop so they can be awaited from any task.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use pyo3::prelude::*;
use pyo3::types::PyTuple;

use super::PyServer;

#[pymethods]
impl PyServer {
    fn close(&self) {
        self.core.close();
    }

    fn is_serving(&self) -> bool {
        self.core.is_serving()
    }

    fn get_loop(&self, py: Python<'_>) -> Py<PyAny> {
        self.core.loop_obj.clone_ref(py)
    }

    fn start_serving<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let locals = self.core.locals(py)?;
        let core = Arc::clone(&self.core);
        pyo3_async_runtimes::async_std::future_into_py_with_locals(py, locals, async move {
            core.spawn_accept_tasks();
            Ok(Python::attach(|py| py.None()))
        })
    }

    fn wait_closed<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let locals = self.core.locals(py)?;
        let core = Arc::clone(&self.core);
        pyo3_async_runtimes::async_std::future_into_py_with_locals(py, locals, async move {
            loop {
                if core.is_closed()
                    && core.active_accept_tasks.load(Ordering::Acquire) == 0
                    && core.active_connections.load(Ordering::SeqCst) == 0
                    && core.pending_tls_handshakes.load(Ordering::Acquire) == 0
                {
                    return Ok(Python::attach(|py| py.None()));
                }
                let wait = core.closed_notify.listen();
                if core.is_closed()
                    && core.active_accept_tasks.load(Ordering::Acquire) == 0
                    && core.active_connections.load(Ordering::SeqCst) == 0
                    && core.pending_tls_handshakes.load(Ordering::Acquire) == 0
                {
                    return Ok(Python::attach(|py| py.None()));
                }
                let _ = wait.await;
            }
        })
    }

    fn serve_forever<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let locals = self.core.locals(py)?;
        let core = Arc::clone(&self.core);
        pyo3_async_runtimes::async_std::future_into_py_with_locals(py, locals, async move {
            core.spawn_accept_tasks();
            loop {
                if core.is_closed() {
                    return Ok(Python::attach(|py| py.None()));
                }
                let wait = core.closed_notify.listen();
                if core.is_closed() {
                    return Ok(Python::attach(|py| py.None()));
                }
                let _ = wait.await;
            }
        })
    }

    #[getter]
    fn sockets(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let tuple = PyTuple::new(
            py,
            self.core
                .sockets
                .iter()
                .map(|socket| socket.clone_ref(py))
                .collect::<Vec<_>>(),
        )?;
        Ok(tuple.unbind().into_any())
    }

    fn __repr__(&self) -> String {
        format!(
            "<Server serving={} closed={}>",
            self.core.is_serving(),
            self.core.is_closed()
        )
    }
}
