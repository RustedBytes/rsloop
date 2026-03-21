use pyo3::exceptions::{PyBaseException, PyException};
use pyo3::prelude::*;
use pyo3::types::PyDict;

use crate::loop_core::LoopCore;

pub fn handle_callback_error(
    py: Python<'_>,
    loop_core: &LoopCore,
    loop_obj: Option<&Py<PyAny>>,
    err: PyErr,
    handle_repr: String,
) -> PyResult<Option<PyErr>> {
    if err.is_instance_of::<PyException>(py) {
        let context = PyDict::new(py);
        context.set_item("message", "Unhandled exception in event loop callback")?;
        context.set_item("exception", err.value(py))?;
        context.set_item("handle", handle_repr)?;
        loop_core.call_exception_handler(py, loop_obj, context.unbind().into())?;
        return Ok(None);
    }

    if err.is_instance_of::<PyBaseException>(py) {
        return Ok(Some(err));
    }

    Ok(Some(err))
}
