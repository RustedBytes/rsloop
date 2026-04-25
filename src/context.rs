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
        // SAFETY: The GIL is held by `py`, and `PyContext_CopyCurrent` returns a new owned
        // reference or null with a Python exception set. PyO3 converts both cases correctly.
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
    // SAFETY: `context` is a live Python context object and the GIL is held. CPython returns
    // `0` on success and sets an exception on failure.
    let status = unsafe { ffi::PyContext_Enter(context.as_ptr()) };
    if status == 0 {
        Ok(())
    } else {
        Err(PyErr::fetch(py))
    }
}

#[inline]
pub fn exit_context(py: Python<'_>, context: &Py<PyAny>) -> PyResult<()> {
    // SAFETY: `context` is the same kind of live Python context object expected by CPython and
    // the GIL is held. A nonzero result means an exception is available via `PyErr::fetch`.
    let status = unsafe { ffi::PyContext_Exit(context.as_ptr()) };
    if status == 0 {
        Ok(())
    } else {
        Err(PyErr::fetch(py))
    }
}

#[inline]
fn call_noargs(py: Python<'_>, callback: &Py<PyAny>) -> PyResult<Py<PyAny>> {
    // SAFETY: `callback` is a live Python callable and the GIL is held. The owned return pointer
    // is transferred to PyO3, which converts null returns into `PyErr`.
    unsafe { Bound::from_owned_ptr_or_err(py, ffi::compat::PyObject_CallNoArgs(callback.as_ptr())) }
        .map(Bound::unbind)
}

#[inline]
fn call_onearg(
    py: Python<'_>,
    callback: &Py<PyAny>,
    arg: &Bound<'_, PyAny>,
) -> PyResult<Py<PyAny>> {
    // SAFETY: `callback` and `arg` are live Python objects under the GIL, and the CPython varargs
    // list is terminated with a null pointer. PyO3 owns successful returns and maps null to `PyErr`.
    let ptr = unsafe {
        ffi::PyObject_CallFunctionObjArgs(
            callback.as_ptr(),
            arg.as_ptr(),
            std::ptr::null_mut::<ffi::PyObject>(),
        )
    };
    // SAFETY: `ptr` is the owned result returned by CPython for the call above.
    unsafe { Bound::from_owned_ptr_or_err(py, ptr) }.map(Bound::unbind)
}

pub fn run_in_context(
    py: Python<'_>,
    context: &Py<PyAny>,
    needs_run: bool,
    callback: &Py<PyAny>,
    args: &Py<PyTuple>,
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

#[inline]
pub fn run_in_context_noargs(
    py: Python<'_>,
    context: &Py<PyAny>,
    needs_run: bool,
    callback: &Py<PyAny>,
) -> PyResult<Py<PyAny>> {
    if !needs_run {
        return call_noargs(py, callback);
    }

    if let Err(err) = enter_context(py, context) {
        return if is_nested_context_error(py, &err) {
            call_noargs(py, callback)
        } else {
            Err(err)
        };
    }

    let callback_result = call_noargs(py, callback);
    let exit_result = exit_context(py, context);

    match (callback_result, exit_result) {
        (Ok(result), Ok(())) => Ok(result),
        (Err(err), _) => Err(err),
        (Ok(_), Err(err)) => Err(err),
    }
}

#[inline]
pub fn run_in_context_onearg(
    py: Python<'_>,
    context: &Py<PyAny>,
    needs_run: bool,
    callback: &Py<PyAny>,
    arg: &Bound<'_, PyAny>,
) -> PyResult<Py<PyAny>> {
    if !needs_run {
        return call_onearg(py, callback, arg);
    }

    if let Err(err) = enter_context(py, context) {
        return if is_nested_context_error(py, &err) {
            call_onearg(py, callback, arg)
        } else {
            Err(err)
        };
    }

    let callback_result = call_onearg(py, callback, arg);
    let exit_result = exit_context(py, context);

    match (callback_result, exit_result) {
        (Ok(result), Ok(())) => Ok(result),
        (Err(err), _) => Err(err),
        (Ok(_), Err(err)) => Err(err),
    }
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
