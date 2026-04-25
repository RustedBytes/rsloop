use pyo3::ffi;
use pyo3::prelude::*;

pub(super) fn call_noargs(py: Python<'_>, callable: &Py<PyAny>) -> PyResult<Py<PyAny>> {
    // SAFETY: `callable` is a live Python object and the GIL is held. PyO3 takes ownership of a
    // non-null owned return and maps a null return to the active Python exception.
    let result = unsafe {
        let ptr = ffi::compat::PyObject_CallNoArgs(callable.as_ptr());
        Bound::from_owned_ptr_or_err(py, ptr)
    };
    result.map(Bound::unbind)
}

pub(super) fn call_onearg(
    py: Python<'_>,
    callable: &Py<PyAny>,
    arg: &Bound<'_, PyAny>,
) -> PyResult<Py<PyAny>> {
    // SAFETY: `callable` and `arg` are live Python objects under the GIL, and the CPython varargs
    // list is null-terminated. The owned result is immediately wrapped by PyO3.
    let result = unsafe {
        let ptr = ffi::PyObject_CallFunctionObjArgs(
            callable.as_ptr(),
            arg.as_ptr(),
            std::ptr::null_mut::<ffi::PyObject>(),
        );
        Bound::from_owned_ptr_or_err(py, ptr)
    };
    result.map(Bound::unbind)
}

#[cfg(any(Py_3_12, all(Py_3_11, not(Py_LIMITED_API))))]
pub(super) fn vectorcall(
    py: Python<'_>,
    callable: *mut ffi::PyObject,
    args: *const *mut ffi::PyObject,
    nargsf: usize,
    kwnames: *mut ffi::PyObject,
) -> PyResult<Py<PyAny>> {
    // SAFETY: The callable, positional argument array, and keyword tuple are all live under the GIL
    // for this call. PyO3 converts null returns into `PyErr`.
    let result = unsafe {
        let ptr = ffi::PyObject_Vectorcall(callable, args, nargsf, kwnames);
        Bound::from_owned_ptr_or_err(py, ptr)
    };
    result.map(Bound::unbind)
}
