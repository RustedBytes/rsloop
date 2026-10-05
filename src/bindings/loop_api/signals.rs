//! Unix signal handlers.
//!
//! Signal delivery is process-wide and main-thread-only, so registration is
//! rejected off the main thread the same way `asyncio` rejects it. The handler
//! itself is not run from the signal context: [`signal_bridge`] is what the
//! watcher calls, and it only hands the callback to `call_soon_threadsafe`.

#[cfg(unix)]
use pyo3::exceptions::PyValueError;
use pyo3::{
    exceptions::PyRuntimeError,
    prelude::*,
    types::{PyDict, PyTuple},
};

use super::PyLoop;
#[cfg(unix)]
use crate::context::capture_context;
#[cfg(unix)]
use crate::engine::SignalHandlerTemplate;
use crate::engine::{LoopCommand, LoopSignalCommand};

#[cfg_attr(feature = "profile", hotpath::measure)]
pub(super) fn add_signal_handler(
    slf: Py<PyLoop>,
    py: Python<'_>,
    sig: i32,
    callback: Py<PyAny>,
    args: &Bound<'_, PyTuple>,
) -> PyResult<()> {
    #[cfg(not(unix))]
    {
        let _ = (slf, py, sig, callback, args);
        Err(PyLoop::not_implemented("add_signal_handler"))
    }
    #[cfg(unix)]
    {
        let threading = py.import("threading")?;
        let current_thread = threading.getattr("current_thread")?.call0()?;
        let main_thread = threading.getattr("main_thread")?.call0()?;
        if !current_thread.is(&main_thread) {
            return Err(PyValueError::new_err(
                "set_wakeup_fd only works in main thread of the main interpreter",
            ));
        }

        let loop_ref = slf.borrow(py);
        let core = loop_ref.core.clone();
        drop(loop_ref);

        if sig == libc::SIGCHLD {
            return Err(PyRuntimeError::new_err(
                "SIGCHLD is reserved for subprocess handling",
            ));
        }
        let (context, context_needs_run) = capture_context(py, None)?;

        let newly_installed = {
            let mut state = core.state.lock().expect("poisoned loop state");
            let newly_installed = !state.signal_handlers.contains_key(&sig);
            state.signal_handlers.insert(
                sig,
                SignalHandlerTemplate {
                    callback,
                    args: args.clone().unbind(),
                    context,
                    context_needs_run,
                },
            );
            newly_installed
        };

        if newly_installed {
            core.send_command(LoopCommand::Signal(LoopSignalCommand::StartWatcher(sig)))
                .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
        }
        Ok(())
    }
}

#[cfg_attr(feature = "profile", hotpath::measure)]
pub(super) fn remove_signal_handler(slf: Py<PyLoop>, py: Python<'_>, sig: i32) -> PyResult<bool> {
    let loop_ref = slf.borrow(py);
    let core = loop_ref.core.clone();
    drop(loop_ref);

    let removed = {
        let mut state = core.state.lock().expect("poisoned loop state");
        let removed = state.signal_handlers.remove(&sig).is_some();
        if removed {
            state.previous_signal_handlers.remove(&sig);
        }
        removed
    };
    if removed {
        core.send_command(LoopCommand::Signal(LoopSignalCommand::StopWatcher(sig)))
            .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
    }
    Ok(removed)
}

/// Called by the signal watcher, not from a signal handler: re-enters the loop
/// through `call_soon_threadsafe` so the user callback runs on the loop thread
/// in the context captured at registration.
#[cfg_attr(feature = "profile", hotpath::measure)]
#[pyfunction]
#[pyo3(signature=(loop_obj, callback, args, context, *_signal_info))]
pub fn signal_bridge(
    py: Python<'_>,
    loop_obj: &Bound<'_, PyAny>,
    callback: Py<PyAny>,
    args: Py<PyTuple>,
    context: Py<PyAny>,
    _signal_info: &Bound<'_, PyTuple>,
) -> PyResult<()> {
    let mut call_items = Vec::with_capacity(args.bind(py).len() + 1);
    call_items.push(callback);
    call_items.extend(args.bind(py).iter().map(|item| item.unbind()));
    let call_args = PyTuple::new(py, call_items)?;
    let kwargs = PyDict::new(py);
    kwargs.set_item("context", context)?;
    loop_obj.call_method("call_soon_threadsafe", call_args, Some(&kwargs))?;
    Ok(())
}
