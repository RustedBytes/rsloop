use pyo3::ffi;
use pyo3::prelude::*;
use pyo3::types::PyTuple;
use std::sync::OnceLock;

static SET_RUNNING_LOOP_FN: OnceLock<Py<PyAny>> = OnceLock::new();

#[inline]
fn set_running_loop_fn(py: Python<'_>) -> PyResult<&Py<PyAny>> {
    if let Some(cached) = SET_RUNNING_LOOP_FN.get() {
        return Ok(cached);
    }

    let loaded = py
        .import("asyncio.events")?
        .getattr("_set_running_loop")?
        .unbind();
    Ok(SET_RUNNING_LOOP_FN.get_or_init(|| loaded))
}

pub fn capture_context(py: Python<'_>, explicit: Option<Py<PyAny>>) -> PyResult<(Py<PyAny>, bool)> {
    let context = if let Some(context) = explicit {
        context
    } else {
        unsafe { Bound::from_owned_ptr_or_err(py, ffi::PyContext_CopyCurrent())?.unbind() }
    };

    Ok((context, true))
}

#[inline]
pub fn is_nested_context_error(py: Python<'_>, err: &PyErr) -> bool {
    err.is_instance_of::<pyo3::exceptions::PyRuntimeError>(py)
        && err
            .value(py)
            .str()
            .ok()
            .and_then(|message| message.to_str().ok().map(str::to_owned))
            .is_some_and(|message| message.contains("is already entered"))
}

#[inline]
pub fn enter_context(py: Python<'_>, context: &Py<PyAny>) -> PyResult<()> {
    let status = unsafe { ffi::PyContext_Enter(context.as_ptr()) };
    if status == 0 {
        Ok(())
    } else {
        Err(PyErr::fetch(py))
    }
}

#[inline]
pub fn exit_context(py: Python<'_>, context: &Py<PyAny>) -> PyResult<()> {
    let status = unsafe { ffi::PyContext_Exit(context.as_ptr()) };
    if status == 0 {
        Ok(())
    } else {
        Err(PyErr::fetch(py))
    }
}

pub fn run_in_context(
    py: Python<'_>,
    context: &Py<PyAny>,
    needs_run: bool,
    callback: &Py<PyAny>,
    args: &Py<PyTuple>,
    _context_args: &Py<PyTuple>,
) -> PyResult<Py<PyAny>> {
    if !needs_run {
        return callback.call1(py, args.clone_ref(py));
    }

    if let Err(err) = enter_context(py, context) {
        return if is_nested_context_error(py, &err) {
            callback.call1(py, args.clone_ref(py))
        } else {
            Err(err)
        };
    }

    let callback_result = callback.call1(py, args.clone_ref(py));
    let exit_result = exit_context(py, context);

    match (callback_result, exit_result) {
        (Ok(result), Ok(())) => Ok(result),
        (Err(err), _) => Err(err),
        (Ok(_), Err(err)) => Err(err),
    }
}

pub fn build_context_args(
    py: Python<'_>,
    callback: &Py<PyAny>,
    args: &Py<PyTuple>,
) -> PyResult<Py<PyTuple>> {
    let args_bound = args.bind(py);
    let mut run_args = Vec::with_capacity(args_bound.len() + 1);
    run_args.push(callback.clone_ref(py));
    run_args.extend(args_bound.iter().map(|item| item.unbind()));
    Ok(PyTuple::new(py, run_args)?.unbind())
}

#[inline]
pub fn ensure_running_loop(py: Python<'_>, loop_obj: &Py<PyAny>) -> PyResult<()> {
    set_running_loop_fn(py)?.call1(py, (loop_obj.clone_ref(py),))?;
    Ok(())
}

#[inline]
pub fn clear_running_loop(py: Python<'_>) -> PyResult<()> {
    set_running_loop_fn(py)?.call1(py, (py.None(),))?;
    Ok(())
}
