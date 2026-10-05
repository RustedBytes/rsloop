//! Small, audited `CPython` FFI call helpers.

#[cfg(any(Py_3_12, all(Py_3_11, not(Py_LIMITED_API))))]
use pyo3::ffi;
use pyo3::prelude::*;

#[cfg_attr(feature = "profile", hotpath::measure)]
pub(super) fn call_noargs(py: Python<'_>, callable: &Py<PyAny>) -> PyResult<Py<PyAny>> {
    Ok(callable.bind(py).call0()?.unbind())
}

#[cfg_attr(feature = "profile", hotpath::measure)]
pub(super) fn call_onearg(
    py: Python<'_>,
    callable: &Py<PyAny>,
    arg: &Bound<'_, PyAny>,
) -> PyResult<Py<PyAny>> {
    Ok(callable.bind(py).call1((arg,))?.unbind())
}

#[cfg_attr(feature = "profile", hotpath::measure)]
#[cfg(any(Py_3_12, all(Py_3_11, not(Py_LIMITED_API))))]
pub(super) fn vectorcall(
    py: Python<'_>,
    callable: *mut ffi::PyObject,
    args: *const *mut ffi::PyObject,
    nargsf: usize,
    kwnames: *mut ffi::PyObject,
) -> PyResult<Py<PyAny>> {
    // SAFETY: The callable, positional argument array, and keyword tuple are all
    // live under the GIL for this call. PyO3 converts null returns into
    // `PyErr`.
    let result = unsafe {
        let ptr = ffi::PyObject_Vectorcall(callable, args, nargsf, kwnames);
        Bound::from_owned_ptr_or_err(py, ptr)
    };
    result.map(Bound::unbind)
}
