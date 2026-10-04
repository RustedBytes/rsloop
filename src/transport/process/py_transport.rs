//! The `ProcessTransport` object Python holds.
//!
//! Mirrors `asyncio.SubprocessTransport`. The signalling methods do not act on
//! the child directly — they post a `ProcessCommand` to the waiter thread,
//! which already owns the `Child` and is the only place allowed to reap it.
//! That keeps `kill()` from racing the `try_wait()` that observes the exit.

use pyo3::exceptions::PyProcessLookupError;
use pyo3::prelude::*;

use super::{ProcessCommand, PyProcessTransport};

#[pymethods]
impl PyProcessTransport {
    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "PyProcessTransport")
    )]
    fn get_pid(&self) -> u32 {
        self.core.state.lock().expect("poisoned process state").pid
    }

    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "PyProcessTransport")
    )]
    #[inline]
    fn get_returncode(&self) -> Option<i32> {
        self.core.get_returncode()
    }

    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "PyProcessTransport")
    )]
    fn is_closing(&self) -> bool {
        self.core.is_closing()
    }

    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "PyProcessTransport")
    )]
    fn get_pipe_transport(&self, py: Python<'_>, fd: i32) -> Option<Py<PyAny>> {
        self.core.pipe_transport(py, fd)
    }

    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "PyProcessTransport")
    )]
    fn send_signal(&self, sig: i32) -> PyResult<()> {
        if self.core.get_returncode().is_some() {
            return Err(PyProcessLookupError::new_err("process is not running"));
        }
        self.core
            .control_tx
            .send(ProcessCommand::SendSignal(sig))
            .map_err(|_| PyProcessLookupError::new_err("process is not running"))
    }

    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "PyProcessTransport")
    )]
    fn terminate(&self) -> PyResult<()> {
        if self.core.get_returncode().is_some() {
            return Err(PyProcessLookupError::new_err("process is not running"));
        }
        self.core
            .control_tx
            .send(ProcessCommand::Terminate)
            .map_err(|_| PyProcessLookupError::new_err("process is not running"))
    }

    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "PyProcessTransport")
    )]
    fn kill(&self) -> PyResult<()> {
        if self.core.get_returncode().is_some() {
            return Err(PyProcessLookupError::new_err("process is not running"));
        }
        self.core
            .control_tx
            .send(ProcessCommand::Kill)
            .map_err(|_| PyProcessLookupError::new_err("process is not running"))
    }

    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "PyProcessTransport")
    )]
    fn close(&self, py: Python<'_>) -> PyResult<()> {
        {
            let mut state = self.core.state.lock().expect("poisoned process state");
            state.closing = true;
        }
        if let Some(stdin) = self.core.pipe_transport(py, 0) {
            let _ = stdin.call_method0(py, "close");
        }
        let _ = self.core.control_tx.send(ProcessCommand::Close);
        Ok(())
    }

    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "PyProcessTransport")
    )]
    fn __repr__(&self) -> String {
        format!(
            "<ProcessTransport pid={} returncode={:?} closing={}>",
            self.get_pid(),
            self.get_returncode(),
            self.is_closing()
        )
    }

    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "PyProcessTransport")
    )]
    fn _wait<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let locals = crate::transport::stream::task_locals_for_loop(py, &self.core.loop_obj)?;
        let core = self.core.clone();
        pyo3_async_runtimes::async_std::future_into_py_with_locals(py, locals, async move {
            loop {
                if let Some(returncode) = core.get_returncode() {
                    return Python::attach(|py| -> PyResult<Py<PyAny>> {
                        Ok(returncode.into_pyobject(py)?.unbind().into_any())
                    });
                }
                let wait = core.exit_notify.listen();
                if let Some(returncode) = core.get_returncode() {
                    return Python::attach(|py| -> PyResult<Py<PyAny>> {
                        Ok(returncode.into_pyobject(py)?.unbind().into_any())
                    });
                }
                let _ = wait.await;
            }
        })
    }
}
