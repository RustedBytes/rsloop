//! `connect_read_pipe` / `connect_write_pipe`.

use pyo3::prelude::*;

use super::{
    PyLoop,
    spawn_env::{LoopSpawnEnv, transport_protocol_pair},
};
use crate::transport::stream::{spawn_read_pipe_transport, spawn_write_pipe_transport};

#[cfg_attr(feature = "profile", hotpath::measure)]
pub(super) fn connect_read_pipe<'py>(
    slf: Py<PyLoop>,
    py: Python<'py>,
    protocol_factory: Py<PyAny>,
    pipe: Py<PyAny>,
) -> PyResult<Bound<'py, PyAny>> {
    let locals = PyLoop::task_locals(py, &slf)?;
    let env = LoopSpawnEnv::capture(py, &slf)?;

    pyo3_async_runtimes::async_std::future_into_py_with_locals(py, locals, async move {
        let protocol = Python::attach(|py| env.call_protocol_factory(py, &protocol_factory))?;
        let transport = Python::attach(|py| {
            let _ = pipe.call_method1(py, "setblocking", (false,));
            spawn_read_pipe_transport(py, env.spawn_context(py, &protocol), pipe.clone_ref(py))
        })?;
        Python::attach(|py| transport_protocol_pair(py, transport.into_any(), &protocol))
    })
}

#[cfg_attr(feature = "profile", hotpath::measure)]
pub(super) fn connect_write_pipe<'py>(
    slf: Py<PyLoop>,
    py: Python<'py>,
    protocol_factory: Py<PyAny>,
    pipe: Py<PyAny>,
) -> PyResult<Bound<'py, PyAny>> {
    let locals = PyLoop::task_locals(py, &slf)?;
    let env = LoopSpawnEnv::capture(py, &slf)?;

    pyo3_async_runtimes::async_std::future_into_py_with_locals(py, locals, async move {
        let protocol = Python::attach(|py| env.call_protocol_factory(py, &protocol_factory))?;
        let transport = Python::attach(|py| {
            let _ = pipe.call_method1(py, "setblocking", (false,));
            spawn_write_pipe_transport(
                py,
                env.spawn_context(py, &protocol),
                pipe.clone_ref(py),
                None,
            )
        })?;
        Python::attach(|py| transport_protocol_pair(py, transport.into_any(), &protocol))
    })
}
