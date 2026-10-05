//! Starting, stopping, and closing the loop.

use pyo3::{
    exceptions::PyRuntimeError,
    prelude::*,
    types::{PyDict, PyModule},
};

use super::{PyLoop, asyncgens::AsyncgenHooksGuard};
use crate::engine::LoopCoreError;

#[cfg_attr(feature = "profile", hotpath::measure)]
pub(super) fn close(loop_ref: &PyLoop, py: Python<'_>) -> PyResult<()> {
    let executor = {
        let mut state = loop_ref.core.state.lock().expect("poisoned loop state");
        if state.running {
            return Err(PyLoop::map_loop_error(LoopCoreError::Running));
        }
        if state.closed {
            return Ok(());
        }
        state.executor_shutdown_called = true;
        state.active_asyncgens = None;
        state.default_executor.take()
    };

    loop_ref.core.close().map_err(PyLoop::map_loop_error)?;

    if let Some(executor) = executor {
        executor.call_method1(py, "shutdown", (false,))?;
    }

    Ok(())
}

#[cfg_attr(feature = "profile", hotpath::measure)]
pub(super) fn run_forever(slf: Py<PyLoop>, py: Python<'_>) -> PyResult<()> {
    let loop_obj = PyLoop::as_py_any(py, &slf);
    let core = slf.borrow(py).core.clone();
    let _asyncgen_hooks = AsyncgenHooksGuard::install(py, &loop_obj, &core)?;
    core.run_forever(py, loop_obj)
}

#[cfg_attr(feature = "profile", hotpath::measure)]
pub(super) fn run_until_complete(
    slf: Py<PyLoop>,
    py: Python<'_>,
    future: Py<PyAny>,
) -> PyResult<Py<PyAny>> {
    let core = slf.borrow(py).core.clone();
    let loop_obj = PyLoop::as_py_any(py, &slf);
    let _asyncgen_hooks = AsyncgenHooksGuard::install(py, &loop_obj, &core)?;
    let asyncio = py.import("asyncio")?;
    let new_task = !asyncio
        .getattr("isfuture")?
        .call1((future.clone_ref(py),))?
        .extract::<bool>()?;
    let kwargs = PyDict::new(py);
    kwargs.set_item("loop", loop_obj.clone_ref(py))?;
    let wrapped = asyncio
        .getattr("ensure_future")?
        .call((future,), Some(&kwargs))?;

    let helper_mod = PyModule::import(py, "rsloop._loop")?;
    let functools = py.import("functools")?;
    let stopper = functools.getattr("partial")?.call1((
        helper_mod.getattr("future_done_stop")?,
        loop_obj.clone_ref(py),
    ))?;

    wrapped.call_method1("add_done_callback", (stopper.clone(),))?;
    let result = core.run_forever(py, loop_obj);
    let _ = wrapped.call_method1("remove_done_callback", (stopper,));
    if let Err(err) = result {
        if wrapped.call_method0("done")?.extract::<bool>()?
            && !wrapped.call_method0("cancelled")?.extract::<bool>()?
        {
            let _ = wrapped.call_method0("result");
            if new_task {
                let _ = wrapped.call_method0("exception");
            }
        }
        return Err(err);
    }

    if !wrapped.call_method0("done")?.extract::<bool>()? {
        return Err(PyRuntimeError::new_err(
            "Event loop stopped before Future completed.",
        ));
    }

    Ok(wrapped.call_method0("result")?.unbind())
}

/// `add_done_callback` target installed by `run_until_complete`: stop the loop
/// once the awaited future finishes, but let `SystemExit` and
/// `KeyboardInterrupt` propagate out of `run_forever` instead.
#[cfg_attr(feature = "profile", hotpath::measure)]
#[pyfunction]
pub fn future_done_stop(loop_obj: &Bound<'_, PyAny>, future: &Bound<'_, PyAny>) -> PyResult<()> {
    if !future.call_method0("cancelled")?.extract::<bool>()? {
        let exc = future.call_method0("exception")?;
        if !exc.is_none()
            && (exc.is_instance_of::<pyo3::exceptions::PySystemExit>()
                || exc.is_instance_of::<pyo3::exceptions::PyKeyboardInterrupt>())
        {
            return Ok(());
        }
    }

    loop_obj.call_method0("stop")?;
    Ok(())
}
