use std::future::Future;

use pyo3::prelude::*;

pub use pyo3_async_runtimes::{into_future_with_locals, TaskLocals};

/// Capture the current Python event loop and contextvars so a Rust future can
/// be attached to the active `rsloop` task.
#[inline]
pub fn get_current_locals(py: Python<'_>) -> PyResult<TaskLocals> {
    pyo3_async_runtimes::async_std::get_current_locals(py)
}

/// Convert a `Send` Rust future into a Python awaitable bound to the currently
/// running Python loop.
pub fn future_into_py<F, T>(py: Python<'_>, fut: F) -> PyResult<Bound<'_, PyAny>>
where
    F: Future<Output = PyResult<T>> + Send + 'static,
    T: for<'py> IntoPyObject<'py> + Send + 'static,
{
    future_into_py_with_locals(py, get_current_locals(py)?, fut)
}

/// Convert a `Send` Rust future into a Python awaitable using explicit task
/// locals captured earlier.
pub fn future_into_py_with_locals<F, T>(
    py: Python<'_>,
    locals: TaskLocals,
    fut: F,
) -> PyResult<Bound<'_, PyAny>>
where
    F: Future<Output = PyResult<T>> + Send + 'static,
    T: for<'py> IntoPyObject<'py> + Send + 'static,
{
    pyo3_async_runtimes::async_std::future_into_py_with_locals(py, locals, fut)
}

/// Convert a `!Send` Rust future into a Python awaitable bound to the current
/// Python loop.
pub fn local_future_into_py<F, T>(py: Python<'_>, fut: F) -> PyResult<Bound<'_, PyAny>>
where
    F: Future<Output = PyResult<T>> + 'static,
    T: for<'py> IntoPyObject<'py>,
{
    local_future_into_py_with_locals(py, get_current_locals(py)?, fut)
}

/// Convert a `!Send` Rust future into a Python awaitable using explicit task
/// locals captured earlier.
#[allow(deprecated)]
pub fn local_future_into_py_with_locals<F, T>(
    py: Python<'_>,
    locals: TaskLocals,
    fut: F,
) -> PyResult<Bound<'_, PyAny>>
where
    F: Future<Output = PyResult<T>> + 'static,
    T: for<'py> IntoPyObject<'py>,
{
    pyo3_async_runtimes::async_std::local_future_into_py_with_locals(py, locals, fut)
}
