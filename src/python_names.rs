//! Interned Python method names and low-overhead method-call helpers.

use pyo3::{prelude::*, types::PyString};

#[cfg_attr(feature = "profile", hotpath::measure)]
pub(crate) fn cancelled<'py>(py: Python<'py>) -> &'py Bound<'py, PyString> {
    pyo3::intern!(py, "cancelled")
}

#[cfg_attr(feature = "profile", hotpath::measure)]
#[cfg(any(Py_3_12, all(Py_3_11, not(Py_LIMITED_API))))]
pub(crate) fn context_kw<'py>(py: Python<'py>) -> &'py Bound<'py, PyString> {
    pyo3::intern!(py, "context")
}

#[cfg_attr(feature = "profile", hotpath::measure)]
pub(crate) fn create_future<'py>(py: Python<'py>) -> &'py Bound<'py, PyString> {
    pyo3::intern!(py, "create_future")
}

#[cfg_attr(feature = "profile", hotpath::measure)]
#[cfg(unix)]
pub(crate) fn errno<'py>(py: Python<'py>) -> &'py Bound<'py, PyString> {
    pyo3::intern!(py, "errno")
}

#[cfg_attr(feature = "profile", hotpath::measure)]
pub(crate) fn done<'py>(py: Python<'py>) -> &'py Bound<'py, PyString> {
    pyo3::intern!(py, "done")
}

#[cfg_attr(feature = "profile", hotpath::measure)]
#[cfg(any(Py_3_12, all(Py_3_11, not(Py_LIMITED_API))))]
pub(crate) fn loop_kw<'py>(py: Python<'py>) -> &'py Bound<'py, PyString> {
    pyo3::intern!(py, "loop")
}

#[cfg_attr(feature = "profile", hotpath::measure)]
#[cfg(any(Py_3_12, all(Py_3_11, not(Py_LIMITED_API))))]
pub(crate) fn name_kw<'py>(py: Python<'py>) -> &'py Bound<'py, PyString> {
    pyo3::intern!(py, "name")
}

#[cfg_attr(feature = "profile", hotpath::measure)]
pub(crate) fn pause_reading<'py>(py: Python<'py>) -> &'py Bound<'py, PyString> {
    pyo3::intern!(py, "pause_reading")
}

#[cfg_attr(feature = "profile", hotpath::measure)]
pub(crate) fn pause_writing<'py>(py: Python<'py>) -> &'py Bound<'py, PyString> {
    pyo3::intern!(py, "pause_writing")
}

#[cfg_attr(feature = "profile", hotpath::measure)]
pub(crate) fn resume_reading<'py>(py: Python<'py>) -> &'py Bound<'py, PyString> {
    pyo3::intern!(py, "resume_reading")
}

#[cfg_attr(feature = "profile", hotpath::measure)]
pub(crate) fn resume_writing<'py>(py: Python<'py>) -> &'py Bound<'py, PyString> {
    pyo3::intern!(py, "resume_writing")
}

#[cfg_attr(feature = "profile", hotpath::measure)]
pub(crate) fn set_exception<'py>(py: Python<'py>) -> &'py Bound<'py, PyString> {
    pyo3::intern!(py, "set_exception")
}

#[cfg_attr(feature = "profile", hotpath::measure)]
pub(crate) fn set_result<'py>(py: Python<'py>) -> &'py Bound<'py, PyString> {
    pyo3::intern!(py, "set_result")
}

#[cfg_attr(feature = "profile", hotpath::measure)]
#[inline]
pub(crate) fn call_method0(
    _py: Python<'_>,
    obj: &Bound<'_, PyAny>,
    method: &Bound<'_, PyString>,
) -> PyResult<Py<PyAny>> {
    Ok(obj.call_method0(method)?.unbind())
}

#[cfg_attr(feature = "profile", hotpath::measure)]
#[inline]
pub(crate) fn call_method1(
    _py: Python<'_>,
    obj: &Bound<'_, PyAny>,
    method: &Bound<'_, PyString>,
    arg: &Bound<'_, PyAny>,
) -> PyResult<Py<PyAny>> {
    Ok(obj.call_method1(method, (arg,))?.unbind())
}
