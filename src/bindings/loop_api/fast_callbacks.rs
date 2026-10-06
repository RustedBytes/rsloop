//! FASTCALL descriptors preserve callback arguments without a transient tuple.
//! Nested calls reuse the lifecycle entry's attachment bookkeeping; external
//! calls use PyO3's trampoline. Both paths contain Rust panics.

use std::{cell::Cell, sync::OnceLock};

use pyo3::{exceptions::PyTypeError, ffi, get_trampoline_function, prelude::*, types::PyTuple};

use super::PyLoop;
use crate::engine::{CallbackArgs, CallbackKind};

thread_local! {
    static ATTACHED_CALLBACK_SCOPE: Cell<bool> = const { Cell::new(false) };
}

/// The lifecycle entry's PyO3 guard already tracks attachment while Python
/// callbacks execute. Keep other threads and calls outside that frame on the
/// ordinary trampoline, including its deferred-reference cleanup.
#[cfg_attr(feature = "profile", hotpath::measure)]
pub(super) fn with_attached_callbacks<T>(py: Python<'_>, run: impl FnOnce() -> T) -> T {
    struct Restore(bool);
    impl Drop for Restore {
        #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "Restore"))]
        fn drop(&mut self) {
            ATTACHED_CALLBACK_SCOPE.with(|scope| scope.set(self.0));
        }
    }
    let _py = py;
    let _restore = Restore(ATTACHED_CALLBACK_SCOPE.with(|scope| scope.replace(true)));
    run()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn attached_callback_scope_restores_after_nested_calls_and_unwind() {
        crate::initialize_python_for_tests();
        Python::attach(|py| {
            assert!(!ATTACHED_CALLBACK_SCOPE.with(Cell::get));
            let result = std::panic::catch_unwind(|| {
                with_attached_callbacks(py, || {
                    assert!(ATTACHED_CALLBACK_SCOPE.with(Cell::get));
                    with_attached_callbacks(py, || {
                        assert!(ATTACHED_CALLBACK_SCOPE.with(Cell::get));
                    });
                    assert!(ATTACHED_CALLBACK_SCOPE.with(Cell::get));
                    panic!("scope cleanup probe");
                });
            });
            assert!(result.is_err());
            assert!(!ATTACHED_CALLBACK_SCOPE.with(Cell::get));
        });
    }
}

unsafe extern "C" fn soon_entry(
    slf: *mut ffi::PyObject,
    args: *const *mut ffi::PyObject,
    nargs: ffi::Py_ssize_t,
    names: *mut ffi::PyObject,
) -> *mut ffi::PyObject {
    if !ATTACHED_CALLBACK_SCOPE.with(Cell::get) {
        // SAFETY: CPython supplied this descriptor's FASTCALL arguments.
        return unsafe {
            get_trampoline_function!(fastcall_cfunction_with_keywords, soon)(
                slf, args, nargs, names,
            )
        };
    }
    // Match PyO3's panic containment even if restoring an exception or
    // destroying a panic payload itself panics.
    let trap = pyo3::impl_::panic::PanicTrap::new("uncaught panic in call_soon");
    // SAFETY: CPython invokes method descriptors with an attached thread;
    // with_attached_callbacks additionally guarantees the enclosing PyO3
    // lifecycle frame supplies attachment bookkeeping. The loop's detach
    // sections execute Rust I/O only, never this Python descriptor.
    let py = unsafe { Python::assume_attached() };
    let result = std::panic::catch_unwind(|| {
        // SAFETY: same FASTCALL contract as the ordinary trampoline.
        unsafe { soon(py, slf, args, nargs, names) }
    });
    let output = match result {
        Ok(Ok(value)) => value,
        Ok(Err(error)) => {
            error.restore(py);
            std::ptr::null_mut()
        }
        Err(payload) => {
            let message = payload
                .downcast_ref::<String>()
                .map(String::as_str)
                .or_else(|| payload.downcast_ref::<&str>().copied())
                .unwrap_or("panic from Rust code");
            pyo3::panic::PanicException::new_err(message.to_owned()).restore(py);
            std::ptr::null_mut()
        }
    };
    trap.disarm();
    output
}

#[cfg_attr(feature = "profile", hotpath::measure)]
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

    // CPython passes a null `kwnames` for the normal
    // `loop.call_soon(callback, *args)` form. Keep that hot path independent
    // of keyword parsing so LLVM can see the callback and argument layout
    // directly.
    if kwnames.is_null() {
        if nargs == 0 {
            return Err(PyTypeError::new_err("missing required argument 'callback'"));
        }

        let positional = nargs as usize;
        // SAFETY: FASTCALL supplies `nargs` live positional entries.
        let values = unsafe { std::slice::from_raw_parts(args, positional) };
        // SAFETY: the missing-callback case was rejected above.
        let callback = unsafe { Bound::from_borrowed_ptr(py, values[0]) }.unbind();
        let callback_args = match positional - 1 {
            0 => CallbackArgs::None,
            1 => {
                // SAFETY: this arm proves a second positional entry exists.
                CallbackArgs::One(unsafe { Bound::from_borrowed_ptr(py, values[1]) }.unbind())
            }
            _ => CallbackArgs::Many(
                PyTuple::new(
                    py,
                    (1..positional).map(|index| {
                        // SAFETY: the range is bounded by the FASTCALL array.
                        unsafe { Bound::from_borrowed_ptr(py, values[index]) }
                    }),
                )?
                .unbind(),
            ),
        };
        let handle =
            slf.get()
                .core
                .schedule_callback_args(py, kind, callback, callback_args, None)?;
        return Ok(handle.into_ptr());
    }

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
        slf.get()
            .core
            .schedule_callback_args(py, kind, callback, callback_args, context)?;
    Ok(handle.into_ptr())
}

#[cfg_attr(feature = "profile", hotpath::measure)]
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

#[cfg_attr(feature = "profile", hotpath::measure)]
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

#[cfg_attr(feature = "profile", hotpath::measure)]
pub(crate) fn install_fast_callbacks(py: Python<'_>) -> PyResult<()> {
    // Descriptors borrow their method definition forever. These contain only
    // static strings/function pointers, no interpreter-owned objects. Allocate
    // each definition once process-wide, including with multiple interpreters.
    static SOON: OnceLock<usize> = OnceLock::new();
    static THREADSAFE: OnceLock<usize> = OnceLock::new();
    let class = py.get_type::<PyLoop>();
    for (name, cache, method, doc) in [
        (c"call_soon", &SOON, soon_entry as ffi::PyCFunctionFastWithKeywords,
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
