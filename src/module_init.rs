//! Registration of Rust classes and functions in the `_loop` extension module.

use pyo3::prelude::*;

use crate::bindings::{
    PyLoop, asyncgen_finalizer_hook, asyncgen_firstiter_hook, future_done_stop, new_event_loop,
    signal_bridge,
};
use crate::build_metadata::build_info;
use crate::engine::{PyHandle, PyTimerHandle};
use crate::transport::process::{PyProcessPipeTransport, PyProcessTransport};
use crate::transport::stream::{
    PyFastStreamReader, PyFastStreamWriter, PyServer, PyStreamTransport, open_connection,
    reset_transport_stats, start_server, transport_stats,
};

pub(crate) fn add_module_contents(m: &Bound<'_, PyModule>) -> PyResult<()> {
    add_module_classes(m)?;
    add_event_loop_functions(m)?;
    add_stream_functions(m)?;
    add_diagnostic_functions(m)?;
    add_module_compat_aliases(m)?;
    Ok(())
}

fn add_module_classes(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_class::<PyLoop>()?;
    crate::bindings::install_fast_callbacks(m.py())?;
    m.add_class::<PyHandle>()?;
    m.add_class::<PyTimerHandle>()?;
    m.add_class::<PyProcessTransport>()?;
    m.add_class::<PyProcessPipeTransport>()?;
    m.add_class::<PyServer>()?;
    m.add_class::<PyStreamTransport>()?;
    m.add_class::<PyFastStreamReader>()?;
    m.add_class::<PyFastStreamWriter>()?;
    Ok(())
}

fn add_event_loop_functions(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(new_event_loop, m)?)?;
    m.add_function(wrap_pyfunction!(asyncgen_firstiter_hook, m)?)?;
    m.add_function(wrap_pyfunction!(asyncgen_finalizer_hook, m)?)?;
    m.add_function(wrap_pyfunction!(future_done_stop, m)?)?;
    m.add_function(wrap_pyfunction!(signal_bridge, m)?)?;
    Ok(())
}

fn add_stream_functions(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(open_connection, m)?)?;
    m.add_function(wrap_pyfunction!(start_server, m)?)?;
    Ok(())
}

fn add_diagnostic_functions(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(build_info, m)?)?;
    m.add_function(wrap_pyfunction!(transport_stats, m)?)?;
    m.add_function(wrap_pyfunction!(reset_transport_stats, m)?)?;
    Ok(())
}

fn add_module_compat_aliases(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("_future_done_stop", m.getattr("future_done_stop")?)?;
    m.add(
        "_asyncgen_firstiter_hook",
        m.getattr("asyncgen_firstiter_hook")?,
    )?;
    m.add(
        "_asyncgen_finalizer_hook",
        m.getattr("asyncgen_finalizer_hook")?,
    )?;
    Ok(())
}
