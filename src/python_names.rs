use std::sync::OnceLock;

use pyo3::ffi;
use pyo3::prelude::*;
use pyo3::types::PyString;

fn interned<'py>(
    py: Python<'py>,
    slot: &'static OnceLock<Py<PyString>>,
    value: &str,
) -> &'py Bound<'py, PyString> {
    slot.get_or_init(|| PyString::intern(py, value).unbind())
        .bind(py)
}

pub(crate) fn cancelled<'py>(py: Python<'py>) -> &'py Bound<'py, PyString> {
    static NAME: OnceLock<Py<PyString>> = OnceLock::new();
    interned(py, &NAME, "cancelled")
}

#[cfg(any(Py_3_12, all(Py_3_11, not(Py_LIMITED_API))))]
pub(crate) fn context_kw<'py>(py: Python<'py>) -> &'py Bound<'py, PyString> {
    static NAME: OnceLock<Py<PyString>> = OnceLock::new();
    interned(py, &NAME, "context")
}

pub(crate) fn create_future<'py>(py: Python<'py>) -> &'py Bound<'py, PyString> {
    static NAME: OnceLock<Py<PyString>> = OnceLock::new();
    interned(py, &NAME, "create_future")
}

pub(crate) fn done<'py>(py: Python<'py>) -> &'py Bound<'py, PyString> {
    static NAME: OnceLock<Py<PyString>> = OnceLock::new();
    interned(py, &NAME, "done")
}

#[cfg(any(Py_3_12, all(Py_3_11, not(Py_LIMITED_API))))]
pub(crate) fn loop_kw<'py>(py: Python<'py>) -> &'py Bound<'py, PyString> {
    static NAME: OnceLock<Py<PyString>> = OnceLock::new();
    interned(py, &NAME, "loop")
}

#[cfg(any(Py_3_12, all(Py_3_11, not(Py_LIMITED_API))))]
pub(crate) fn name_kw<'py>(py: Python<'py>) -> &'py Bound<'py, PyString> {
    static NAME: OnceLock<Py<PyString>> = OnceLock::new();
    interned(py, &NAME, "name")
}

pub(crate) fn pause_reading<'py>(py: Python<'py>) -> &'py Bound<'py, PyString> {
    static NAME: OnceLock<Py<PyString>> = OnceLock::new();
    interned(py, &NAME, "pause_reading")
}

pub(crate) fn pause_writing<'py>(py: Python<'py>) -> &'py Bound<'py, PyString> {
    static NAME: OnceLock<Py<PyString>> = OnceLock::new();
    interned(py, &NAME, "pause_writing")
}

pub(crate) fn resume_reading<'py>(py: Python<'py>) -> &'py Bound<'py, PyString> {
    static NAME: OnceLock<Py<PyString>> = OnceLock::new();
    interned(py, &NAME, "resume_reading")
}

pub(crate) fn resume_writing<'py>(py: Python<'py>) -> &'py Bound<'py, PyString> {
    static NAME: OnceLock<Py<PyString>> = OnceLock::new();
    interned(py, &NAME, "resume_writing")
}

pub(crate) fn set_exception<'py>(py: Python<'py>) -> &'py Bound<'py, PyString> {
    static NAME: OnceLock<Py<PyString>> = OnceLock::new();
    interned(py, &NAME, "set_exception")
}

pub(crate) fn set_result<'py>(py: Python<'py>) -> &'py Bound<'py, PyString> {
    static NAME: OnceLock<Py<PyString>> = OnceLock::new();
    interned(py, &NAME, "set_result")
}

#[inline]
pub(crate) fn call_method0(
    py: Python<'_>,
    obj: &Bound<'_, PyAny>,
    method: &Bound<'_, PyString>,
) -> PyResult<Py<PyAny>> {
    // SAFETY: `obj` and `method` are live under the GIL and the varargs list is null-terminated.
    let ptr = unsafe {
        ffi::PyObject_CallMethodObjArgs(
            obj.as_ptr(),
            method.as_ptr(),
            std::ptr::null_mut::<ffi::PyObject>(),
        )
    };
    // SAFETY: `ptr` is the owned result returned by CPython for the call above.
    unsafe { Bound::from_owned_ptr_or_err(py, ptr) }.map(Bound::unbind)
}

#[inline]
pub(crate) fn call_method1(
    py: Python<'_>,
    obj: &Bound<'_, PyAny>,
    method: &Bound<'_, PyString>,
    arg: &Bound<'_, PyAny>,
) -> PyResult<Py<PyAny>> {
    // SAFETY: `obj`, `method`, and `arg` are live Python objects under the active GIL, and the
    // varargs list is null-terminated as required by CPython. PyO3 wraps the owned return pointer.
    let ptr = unsafe {
        ffi::PyObject_CallMethodObjArgs(
            obj.as_ptr(),
            method.as_ptr(),
            arg.as_ptr(),
            std::ptr::null_mut::<ffi::PyObject>(),
        )
    };
    // SAFETY: `ptr` is the owned result returned by CPython for the call above.
    unsafe { Bound::from_owned_ptr_or_err(py, ptr) }.map(Bound::unbind)
}
