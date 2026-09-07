//! `PyO3` event-loop bindings and adapters into the Rust engine.
//!
//! [`PyLoop`] is a thin Python-facing shell: all scheduling and lifecycle state
//! lives in [`LoopCore`], and each group of loop methods lives in its own
//! submodule. [`methods`] holds the one `#[pymethods]` block that names them all.

use std::sync::Arc;

use pyo3::exceptions::{PyNotImplementedError, PyRuntimeError};
use pyo3::prelude::*;
use pyo3::types::PyTuple;
use pyo3_async_runtimes::TaskLocals;

mod asyncgens;
mod asyncio_cache;
mod connections;
mod executor;
mod fast_callbacks;
mod ffi_helpers;
mod lifecycle;
mod methods;
mod pipes;
mod pre_exec;
mod process_handles;
mod process_spawn;
mod process_stdio;
mod servers;
mod signals;
mod sock_ops;
mod socket_connect;
mod socket_operation;
mod sockets;
mod spawn_env;
mod tasks;
mod tls_params;
mod watchers;

pub use asyncgens::{asyncgen_finalizer_hook, asyncgen_firstiter_hook};
pub use lifecycle::future_done_stop;
pub use signals::signal_bridge;

pub(crate) use fast_callbacks::install_fast_callbacks;
pub(crate) use tasks::{try_fast_create_future, try_fast_create_task};

use crate::engine::{CallbackKind, LoopCore, LoopCoreError};

/// Upper bound (~1 century) for timer delays, so math.inf and other oversized
/// values clamp to a far-future deadline instead of panicking the conversion
/// to `Duration`/`Instant` (issue #48).
const MAX_TIMER_DELAY_SECS: f64 = 100.0 * 365.0 * 24.0 * 60.0 * 60.0;

#[pyclass(subclass, module = "rsloop._loop", weakref)]
/// Python-visible event loop; scheduling and lifecycle state live in `LoopCore`.
pub struct PyLoop {
    /// Shared scheduling, lifecycle, and runtime state for this loop.
    pub core: Arc<LoopCore>,
}

impl PyLoop {
    #[inline]
    fn as_py_any(py: Python<'_>, slf: &Py<Self>) -> Py<PyAny> {
        slf.clone_ref(py).into_any()
    }

    #[inline]
    fn task_locals(py: Python<'_>, slf: &Py<Self>) -> PyResult<TaskLocals> {
        TaskLocals::new(Self::as_py_any(py, slf).into_bound(py)).copy_context(py)
    }

    fn schedule_now(
        &self,
        py: Python<'_>,
        kind: CallbackKind,
        callback: Py<PyAny>,
        args: Py<PyTuple>,
        context: Option<Py<PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let handle = self
            .core
            .schedule_callback(py, kind, callback, args, context)?;
        Ok(handle.into_any())
    }

    #[allow(dead_code)]
    fn not_implemented(feature: &str) -> PyErr {
        PyNotImplementedError::new_err(format!("{feature} is not implemented in rust-impl yet"))
    }

    fn map_loop_error(err: LoopCoreError) -> PyErr {
        PyRuntimeError::new_err(err.to_string())
    }
}

#[pyfunction]
/// Creates a new Python-visible rsloop event loop.
///
/// The returned loop is not installed as the current event loop and does not
/// start running until Python calls `run_forever()` or `run_until_complete()`.
pub fn new_event_loop(py: Python<'_>) -> PyResult<Py<PyLoop>> {
    Py::new(py, PyLoop::new())
}
