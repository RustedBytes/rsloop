//! FASTCALL descriptors preserve callback arguments without a transient tuple.
//! PyO3's trampoline supplies attachment bookkeeping and panic containment.

use std::sync::OnceLock;

use pyo3::exceptions::PyTypeError;
use pyo3::prelude::*;
use pyo3::types::PyTuple;
use pyo3::{ffi, get_trampoline_function};

use super::PyLoop;
use crate::engine::{CallbackArgs, CallbackKind};

unsafe fn schedule(
    py: Python<'_>,
    slf: *mut ffi::PyObject,
    args: *const *mut ffi::PyObject,
    nargs: ffi::Py_ssize_t,
    kwnames: *mut ffi::PyObject,
    kind: CallbackKind,
) -> PyResult<*mut ffi::PyObject> {
    // SAFETY: CPython's method descriptor guarantees a live receiver and a
    // valid FASTCALL positional/keyword array for this attached invocation.
    // PyDescr_NewMethod validates that self is this type or a subclass before
    // invoking the trampoline, including explicitly unbound descriptor calls.
    // SAFETY: that descriptor guarantee makes a second Python type check
    // redundant; the borrowed receiver remains live for this invocation.
    let slf = unsafe { Borrowed::from_ptr(py, slf).cast_unchecked::<PyLoop>() };
    let names = if kwnames.is_null() {
        None
    } else {
        // SAFETY: FASTCALL keyword names are a live tuple of strings.
        Some(unsafe { Bound::from_borrowed_ptr(py, kwnames) }.cast_into::<PyTuple>()?)
    };
    let positional = nargs as usize;
    let count = positional + names.as_ref().map_or(0, |names| names.len());
    let values = if count == 0 {
        &[]
    } else {
        // SAFETY: the FASTCALL array has nargs + len(kwnames) live entries.
        unsafe { std::slice::from_raw_parts(args, count) }
    };
    let value = |index| {
        // SAFETY: callers below bound index by the FASTCALL array length.
        unsafe { Bound::from_borrowed_ptr(py, values[index]) }
    };
    let mut callback = (positional > 0).then(|| value(0).unbind());
    let mut context = None;
    let mut context_seen = false;
    if let Some(names) = names {
        for (index, name) in names.iter().enumerate() {
            match name.extract::<&str>()? {
                "callback" if callback.is_none() => {
                    callback = Some(value(positional + index).unbind())
                }
                "callback" => {
                    return Err(PyTypeError::new_err(
                        "multiple values for argument 'callback'",
                    ));
                }
                "context" if !context_seen => {
                    context_seen = true;
                    let arg = value(positional + index);
                    if !arg.is_none() {
                        context = Some(arg.unbind());
                    }
                }
                "context" => {
                    return Err(PyTypeError::new_err(
                        "multiple values for argument 'context'",
                    ));
                }
                name => {
                    return Err(PyTypeError::new_err(format!(
                        "unexpected keyword argument '{name}'"
                    )));
                }
            }
        }
    }
    let callback =
        callback.ok_or_else(|| PyTypeError::new_err("missing required argument 'callback'"))?;
    let callback_args = match positional.saturating_sub(1) {
        0 => CallbackArgs::None,
        1 => CallbackArgs::One(value(1).unbind()),
        _ => CallbackArgs::Many(PyTuple::new(py, (1..positional).map(value))?.unbind()),
    };
    let handle =
        slf.borrow()
            .core
            .schedule_callback_args(py, kind, callback, callback_args, context)?;
    Ok(handle.into_ptr())
}

unsafe fn soon(
    py: Python<'_>,
    slf: *mut ffi::PyObject,
    args: *const *mut ffi::PyObject,
    nargs: ffi::Py_ssize_t,
    names: *mut ffi::PyObject,
) -> PyResult<*mut ffi::PyObject> {
    // SAFETY: forwarded unchanged from the CPython FASTCALL trampoline.
    unsafe { schedule(py, slf, args, nargs, names, CallbackKind::Soon) }
}

unsafe fn threadsafe(
    py: Python<'_>,
    slf: *mut ffi::PyObject,
    args: *const *mut ffi::PyObject,
    nargs: ffi::Py_ssize_t,
    names: *mut ffi::PyObject,
) -> PyResult<*mut ffi::PyObject> {
    // SAFETY: forwarded unchanged from the CPython FASTCALL trampoline.
    unsafe { schedule(py, slf, args, nargs, names, CallbackKind::Threadsafe) }
}

pub(crate) fn install_fast_callbacks(py: Python<'_>) -> PyResult<()> {
    // Descriptors borrow their method definition forever. These contain only
    // static strings/function pointers, no interpreter-owned objects. Allocate
    // each definition once process-wide, including with multiple interpreters.
    static SOON: OnceLock<usize> = OnceLock::new();
    static THREADSAFE: OnceLock<usize> = OnceLock::new();
    let class = py.get_type::<PyLoop>();
    for (name, cache, method, doc) in [
        (c"call_soon", &SOON, get_trampoline_function!(fastcall_cfunction_with_keywords, soon) as ffi::PyCFunctionFastWithKeywords,
         c"call_soon($self, callback, *args, context=None)\n--\n\nSchedule a callback."),
        (c"call_soon_threadsafe", &THREADSAFE, get_trampoline_function!(fastcall_cfunction_with_keywords, threadsafe) as ffi::PyCFunctionFastWithKeywords,
         c"call_soon_threadsafe($self, callback, *args, context=None)\n--\n\nSchedule a callback from any thread."),
    ] {
        let definition = *cache.get_or_init(|| Box::into_raw(Box::new(ffi::PyMethodDef {
            ml_name: name.as_ptr(),
            ml_meth: ffi::PyMethodDefPointer { PyCFunctionFastWithKeywords: method },
            ml_flags: ffi::METH_FASTCALL | ffi::METH_KEYWORDS,
            ml_doc: doc.as_ptr(),
        })) as usize) as *mut ffi::PyMethodDef;
        // SAFETY: class is a live heap type; the immutable definition has
        // process lifetime and its function signature matches the method flags.
        let descriptor = unsafe { Bound::from_owned_ptr_or_err(py, ffi::PyDescr_NewMethod(class.as_type_ptr(), definition)) }?;
        class.setattr(name.to_str().expect("ASCII method name"), descriptor)?;
    }
    Ok(())
}
