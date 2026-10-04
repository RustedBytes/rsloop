//! `contextvars` propagation and running-loop bookkeeping for Python callbacks.

use pyo3::ffi;
use pyo3::prelude::*;
use pyo3::sync::PyOnceLock;
use pyo3::types::PyTuple;

static SET_RUNNING_LOOP_FN: PyOnceLock<Py<PyAny>> = PyOnceLock::new();

#[inline]
fn set_running_loop_fn(py: Python<'_>) -> PyResult<&Py<PyAny>> {
    SET_RUNNING_LOOP_FN.get_or_try_init(py, || {
        Ok(py
            .import("asyncio.events")?
            .getattr("_set_running_loop")?
            .unbind())
    })
}

/// Captures the caller's context unless an explicit context was supplied.
#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
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
#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
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
#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
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
    Ok(callback.bind(py).call0()?.unbind())
}

#[inline]
fn call_onearg(
    py: Python<'_>,
    callback: &Py<PyAny>,
    arg: &Bound<'_, PyAny>,
) -> PyResult<Py<PyAny>> {
    Ok(callback.bind(py).call1((arg,))?.unbind())
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

    // A callback may re-enter the context that is already active on this
    // thread. `asyncio` still runs it, so only unrelated enter errors escape.
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
        (Err(err), _) | (Ok(_), Err(err)) => Err(err),
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
        (Err(err), _) | (Ok(_), Err(err)) => Err(err),
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
        (Err(err), _) | (Ok(_), Err(err)) => Err(err),
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

#[cfg(test)]
mod tests {
    use pyo3::ffi::c_str;
    use pyo3::types::{PyDict, PyTuple};

    use super::*;

    #[test]
    fn captured_context_is_used_and_the_ambient_context_is_restored() {
        crate::initialize_python_for_tests();
        Python::attach(|py| {
            let contextvars = py.import("contextvars").expect("import contextvars");
            let var = contextvars
                .getattr("ContextVar")
                .expect("ContextVar")
                .call1(("value",))
                .expect("create ContextVar");
            var.call_method1("set", ("captured",))
                .expect("set captured value");
            let (context, needs_run) = capture_context(py, None).expect("capture context");
            var.call_method1("set", ("ambient",))
                .expect("set ambient value");

            let locals = PyDict::new(py);
            locals.set_item("var", &var).expect("store ContextVar");
            let callback = py
                .eval(c_str!("lambda: var.get()"), Some(&locals), Some(&locals))
                .expect("build callback")
                .unbind();
            let result = run_in_context_noargs(py, &context, needs_run, &callback)
                .expect("run in captured context");

            assert_eq!(
                result.extract::<String>(py).expect("callback result"),
                "captured"
            );
            assert_eq!(
                var.call_method0("get")
                    .expect("read ambient value")
                    .extract::<String>()
                    .expect("ambient string"),
                "ambient"
            );
        });
    }

    #[test]
    fn callback_failure_still_exits_the_captured_context() {
        crate::initialize_python_for_tests();
        Python::attach(|py| {
            let locals = PyDict::new(py);
            py.run(
                c_str!("import contextvars\nvalue = contextvars.ContextVar('value')\nvalue.set('captured')\ndef fail():\n    raise ValueError(value.get())\n"),
                Some(&locals),
                Some(&locals),
            )
            .expect("define failing callback");
            let (context, needs_run) = capture_context(py, None).expect("capture context");
            let value = locals
                .get_item("value")
                .expect("dict lookup")
                .expect("value");
            value
                .call_method1("set", ("ambient",))
                .expect("set ambient value");
            let callback = locals
                .get_item("fail")
                .expect("dict lookup")
                .expect("callback")
                .unbind();

            let err = run_in_context_noargs(py, &context, needs_run, &callback)
                .expect_err("callback should fail");
            assert!(err.is_instance_of::<pyo3::exceptions::PyValueError>(py));
            assert_eq!(
                value
                    .call_method0("get")
                    .expect("read ambient value")
                    .extract::<String>()
                    .expect("ambient string"),
                "ambient"
            );
        });
    }

    #[test]
    fn nested_context_reentry_falls_back_to_a_direct_call() {
        crate::initialize_python_for_tests();
        Python::attach(|py| {
            let (context, _) = capture_context(py, None).expect("capture context");
            let callback = py
                .eval(c_str!("lambda value: value + 1"), None, None)
                .expect("build callback")
                .unbind();
            let args = PyTuple::new(py, [41]).expect("callback args").unbind();

            enter_context(py, &context).expect("enter context first time");
            let result = run_in_context(py, &context, true, &callback, &args)
                .expect("nested run falls back");
            assert_eq!(result.extract::<i32>(py).expect("integer result"), 42);
            exit_context(py, &context).expect("outer context remains entered");
        });
    }

    #[test]
    fn explicit_context_is_preserved() {
        crate::initialize_python_for_tests();
        Python::attach(|py| {
            let explicit = py
                .import("contextvars")
                .expect("import contextvars")
                .call_method0("copy_context")
                .expect("copy context")
                .unbind();
            let expected_ptr = explicit.as_ptr();

            let (captured, needs_run) =
                capture_context(py, Some(explicit)).expect("accept explicit context");

            assert!(needs_run);
            assert_eq!(captured.as_ptr(), expected_ptr);
        });
    }

    #[test]
    fn running_loop_bookkeeping_sets_and_clears_asyncio_state() {
        crate::initialize_python_for_tests();
        Python::attach(|py| {
            let loop_obj = PyDict::new(py).into_any().unbind();
            ensure_running_loop(py, &loop_obj).expect("set running loop");
            let observed = py
                .import("asyncio.events")
                .expect("import asyncio.events")
                .call_method0("_get_running_loop")
                .expect("get running loop");
            assert!(observed.is(loop_obj.bind(py)));

            clear_running_loop(py).expect("clear running loop");
            let observed = py
                .import("asyncio.events")
                .expect("import asyncio.events")
                .call_method0("_get_running_loop")
                .expect("get cleared running loop");
            assert!(observed.is_none());
        });
    }
}
