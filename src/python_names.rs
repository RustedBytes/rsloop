use std::sync::OnceLock;

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

pub(crate) fn create_future<'py>(py: Python<'py>) -> &'py Bound<'py, PyString> {
    static NAME: OnceLock<Py<PyString>> = OnceLock::new();
    interned(py, &NAME, "create_future")
}

pub(crate) fn done<'py>(py: Python<'py>) -> &'py Bound<'py, PyString> {
    static NAME: OnceLock<Py<PyString>> = OnceLock::new();
    interned(py, &NAME, "done")
}

pub(crate) fn pause_reading<'py>(py: Python<'py>) -> &'py Bound<'py, PyString> {
    static NAME: OnceLock<Py<PyString>> = OnceLock::new();
    interned(py, &NAME, "pause_reading")
}

pub(crate) fn resume_reading<'py>(py: Python<'py>) -> &'py Bound<'py, PyString> {
    static NAME: OnceLock<Py<PyString>> = OnceLock::new();
    interned(py, &NAME, "resume_reading")
}

pub(crate) fn set_exception<'py>(py: Python<'py>) -> &'py Bound<'py, PyString> {
    static NAME: OnceLock<Py<PyString>> = OnceLock::new();
    interned(py, &NAME, "set_exception")
}

pub(crate) fn set_result<'py>(py: Python<'py>) -> &'py Bound<'py, PyString> {
    static NAME: OnceLock<Py<PyString>> = OnceLock::new();
    interned(py, &NAME, "set_result")
}
