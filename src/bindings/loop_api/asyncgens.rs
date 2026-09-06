//! Async-generator tracking and shutdown.
//!
//! `sys.set_asyncgen_hooks` is process-wide, so a running loop installs its own
//! hooks for the duration of `run_forever` and restores the previous pair on the
//! way out — including when the loop exits with an error, which is why the
//! install returns a `Drop` guard. The hooks themselves are exposed to Python so
//! `functools.partial` can bind them to a specific loop.

use std::sync::Arc;

use pyo3::prelude::*;
use pyo3::types::{PyDict, PyModule, PySet, PyWeakrefMethods, PyWeakrefReference};

use super::PyLoop;
use crate::engine::LoopCore;

pub(super) struct AsyncgenHooksGuard {
    old_firstiter: Py<PyAny>,
    old_finalizer: Py<PyAny>,
}

impl AsyncgenHooksGuard {
    // Install loop-specific async-generator hooks temporarily; `Drop` restores
    // the process-wide hooks even when `run_forever` exits with an error.
    pub(super) fn install(
        py: Python<'_>,
        loop_obj: &Py<PyAny>,
        core: &Arc<LoopCore>,
    ) -> PyResult<Self> {
        let sys = py.import("sys")?;
        let hooks = sys.call_method0("get_asyncgen_hooks")?;
        let old_firstiter = hooks.getattr("firstiter")?.unbind();
        let old_finalizer = hooks.getattr("finalizer")?.unbind();
        let helper_mod = PyModule::import(py, "rsloop._loop")?;
        let functools = py.import("functools")?;
        let firstiter = functools.getattr("partial")?.call1((
            helper_mod.getattr("_asyncgen_firstiter_hook")?,
            loop_obj.clone_ref(py),
        ))?;
        let finalizer = functools.getattr("partial")?.call1((
            helper_mod.getattr("_asyncgen_finalizer_hook")?,
            loop_obj.clone_ref(py),
        ))?;
        let kwargs = PyDict::new(py);
        kwargs.set_item("firstiter", firstiter)?;
        kwargs.set_item("finalizer", finalizer)?;
        sys.call_method("set_asyncgen_hooks", (), Some(&kwargs))?;

        {
            let mut state = core.state.lock().expect("poisoned loop state");
            if state.active_asyncgens.is_none() {
                state.active_asyncgens = Some(new_asyncgens_set(py)?);
            }
        }

        Ok(Self {
            old_firstiter,
            old_finalizer,
        })
    }
}

impl Drop for AsyncgenHooksGuard {
    fn drop(&mut self) {
        Python::attach(|py| {
            let sys = match py.import("sys") {
                Ok(sys) => sys,
                Err(_) => return,
            };
            let kwargs = PyDict::new(py);
            let _ = kwargs.set_item("firstiter", self.old_firstiter.bind(py));
            let _ = kwargs.set_item("finalizer", self.old_finalizer.bind(py));
            let _ = sys.call_method("set_asyncgen_hooks", (), Some(&kwargs));
        });
    }
}

fn new_asyncgens_set(py: Python<'_>) -> PyResult<Py<PySet>> {
    // A strong set retains every completed async context manager until loop
    // shutdown and prevents abandoned generators from reaching their finalizer.
    // Store weak references in a native set, with set.discard as the callback,
    // avoiding a Python WeakSet.add frame for every async context manager.
    Ok(PySet::empty(py)?.unbind())
}

fn active_asyncgens_set(py: Python<'_>, core: &Arc<LoopCore>) -> PyResult<Py<PySet>> {
    let mut state = core.state.lock().expect("poisoned loop state");
    if let Some(active) = state.active_asyncgens.as_ref() {
        return Ok(active.clone_ref(py));
    }
    let active = new_asyncgens_set(py)?;
    state.active_asyncgens = Some(active.clone_ref(py));
    Ok(active)
}

pub(super) fn shutdown_asyncgens<'py>(
    slf: Py<PyLoop>,
    py: Python<'py>,
) -> PyResult<Bound<'py, PyAny>> {
    let core = slf.borrow(py).core.clone();
    let loop_obj = PyLoop::as_py_any(py, &slf);
    let active = active_asyncgens_set(py, &core)?;
    let mut closing_agens = Vec::with_capacity(active.bind(py).len());
    // Weakref callbacks can remove entries during collection, including from
    // another thread on free-threaded Python. Iterate a private snapshot.
    let snapshot = active.bind(py).call_method0("copy")?;
    for reference in snapshot.cast::<PySet>()?.iter() {
        if let Some(agen) = reference.cast::<PyWeakrefReference>()?.upgrade() {
            closing_agens.push(agen.unbind());
        }
    }
    active.bind(py).clear();
    core.state
        .lock()
        .expect("poisoned loop state")
        .asyncgens_shutdown_called = true;

    let locals = PyLoop::task_locals(py, &slf)?;
    let locals_for_await = locals.clone();
    pyo3_async_runtimes::async_std::future_into_py_with_locals(py, locals, async move {
        if closing_agens.is_empty() {
            return Ok(Python::attach(|py| py.None()));
        }

        for agen in &closing_agens {
            let aclose = Python::attach(|py| agen.call_method0(py, "aclose"))?;
            let result = Python::attach(|py| {
                pyo3_async_runtimes::into_future_with_locals(
                    &locals_for_await,
                    aclose.bind(py).clone(),
                )
            })?
            .await;

            if let Err(err) = result {
                Python::attach(|py| -> PyResult<()> {
                    let context = PyDict::new(py);
                    context.set_item(
                        "message",
                        format!(
                            "an error occurred during closing of asynchronous generator {:?}",
                            agen.bind(py)
                        ),
                    )?;
                    context.set_item("exception", err.value(py))?;
                    context.set_item("asyncgen", agen.bind(py))?;
                    loop_obj.call_method1(py, "call_exception_handler", (context,))?;
                    Ok(())
                })?;
            }
        }

        Ok(Python::attach(|py| py.None()))
    })
}

#[pyfunction]
/// Registers an asynchronous generator with its owning loop on first iteration.
///
/// This implements the first-iteration half of Python's asynchronous-generator
/// hooks and emits a `ResourceWarning` if iteration begins after
/// `loop.shutdown_asyncgens()`.
pub fn asyncgen_firstiter_hook(
    py: Python<'_>,
    loop_obj: &Bound<'_, PyAny>,
    agen: &Bound<'_, PyAny>,
) -> PyResult<()> {
    let loop_ref = loop_obj.extract::<PyRef<'_, PyLoop>>()?;
    let core = loop_ref.core.clone();
    drop(loop_ref);

    let shutdown_called = {
        let state = core.state.lock().expect("poisoned loop state");
        state.asyncgens_shutdown_called
    };
    if shutdown_called {
        let warnings = py.import("warnings")?;
        let builtins = py.import("builtins")?;
        let kwargs = PyDict::new(py);
        kwargs.set_item("source", loop_obj)?;
        warnings.call_method(
            "warn",
            (
                format!(
                    "asynchronous generator {:?} was scheduled after loop.shutdown_asyncgens() call",
                    agen
                ),
                builtins.getattr("ResourceWarning")?,
            ),
            Some(&kwargs),
        )?;
    }

    let active = active_asyncgens_set(py, &core)?;
    let active = active.bind(py);
    let reference = PyWeakrefReference::new_with(agen, active.getattr("discard")?)?;
    active.add(reference)?;
    Ok(())
}

#[pyfunction]
/// Unregisters and schedules finalization of an asynchronous generator.
///
/// If the loop is still open, the generator's `aclose()` awaitable is submitted
/// with `call_soon_threadsafe`; closed loops simply discard the registration.
pub fn asyncgen_finalizer_hook(
    py: Python<'_>,
    loop_obj: &Bound<'_, PyAny>,
    agen: &Bound<'_, PyAny>,
) -> PyResult<()> {
    let loop_ref = loop_obj.extract::<PyRef<'_, PyLoop>>()?;
    let core = loop_ref.core.clone();
    drop(loop_ref);

    active_asyncgens_set(py, &core)?
        .bind(py)
        .discard(PyWeakrefReference::new(agen)?)?;
    if !core.is_closed() {
        let create_task = loop_obj.getattr("create_task")?;
        let aclose = agen.call_method0("aclose")?;
        loop_obj.call_method1("call_soon_threadsafe", (create_task, aclose))?;
    }
    Ok(())
}
