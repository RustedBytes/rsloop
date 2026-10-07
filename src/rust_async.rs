//! Public adapters for turning Rust futures into Python awaitables.

mod local;
mod runtime;

use std::future::Future;

use pyo3::prelude::*;
pub use pyo3_async_runtimes::{TaskLocals, into_future_with_locals};
use runtime::SmolRuntime;

/// Capture the current Python event loop and contextvars so a Rust future can
/// be attached to the active `rsloop` task.
#[cfg_attr(feature = "profile", hotpath::measure)]
#[inline]
pub fn get_current_locals(py: Python<'_>) -> PyResult<TaskLocals> {
    pyo3_async_runtimes::generic::get_current_locals::<SmolRuntime>(py)
}

/// Convert a `Send` Rust future into a Python awaitable bound to the currently
/// running Python loop.
#[cfg_attr(feature = "profile", hotpath::measure)]
pub fn future_into_py<F, T>(py: Python<'_>, fut: F) -> PyResult<Bound<'_, PyAny>>
where
    F: Future<Output = PyResult<T>> + Send + 'static,
    T: for<'py> IntoPyObject<'py> + Send + 'static,
{
    future_into_py_with_locals(py, get_current_locals(py)?, fut)
}

/// Convert a `Send` Rust future into a Python awaitable using explicit task
/// locals captured earlier.
#[cfg_attr(feature = "profile", hotpath::measure)]
pub fn future_into_py_with_locals<F, T>(
    py: Python<'_>,
    locals: TaskLocals,
    fut: F,
) -> PyResult<Bound<'_, PyAny>>
where
    F: Future<Output = PyResult<T>> + Send + 'static,
    T: for<'py> IntoPyObject<'py> + Send + 'static,
{
    pyo3_async_runtimes::generic::future_into_py_with_locals::<SmolRuntime, _, _>(py, locals, fut)
}

/// Convert a `!Send` Rust future into a Python awaitable bound to the current
/// Python loop.
///
/// Call on the thread that runs the captured loop; the future is polled there.
#[cfg_attr(feature = "profile", hotpath::measure)]
pub fn local_future_into_py<F, T>(py: Python<'_>, fut: F) -> PyResult<Bound<'_, PyAny>>
where
    F: Future<Output = PyResult<T>> + 'static,
    T: for<'py> IntoPyObject<'py>,
{
    local_future_into_py_with_locals(py, get_current_locals(py)?, fut)
}

/// Convert a `!Send` Rust future into a Python awaitable using explicit task
/// locals captured earlier.
///
/// Call on the thread that will run the captured loop. It may be stopped when
/// this helper is called, but must run to drive completion or cancellation.
#[cfg_attr(feature = "profile", hotpath::measure)]
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
    runtime::with_locals(locals.clone(), || {
        pyo3_async_runtimes::generic::local_future_into_py_with_locals::<SmolRuntime, _, _>(
            py, locals, fut,
        )
    })
}

#[cfg_attr(feature = "profile", hotpath::measure(future = true))]
/// Race an operation against the executor-independent async-io timer.
pub(crate) async fn timeout<F: Future>(
    duration: std::time::Duration,
    future: F,
) -> Result<F::Output, ()> {
    smol::future::or(async { Ok(future.await) }, async {
        async_io::Timer::after(duration).await;
        Err(())
    })
    .await
}

#[cfg(test)]
mod tests;
