mod async_event;
mod blocking;
mod callbacks;
mod context;
mod errors;
mod fast_streams;
mod fd_ops;
mod loop_core;
mod process_transport;
mod profiler;
mod python_api;
mod python_names;
mod runtime;
mod stream_transport;
mod tls;
#[cfg(windows)]
mod windows_vibeio;

pub use callbacks::{PyHandle, PyTimerHandle, ReadyCallback};
pub use fast_streams::{open_connection, start_server, PyFastStreamReader, PyFastStreamWriter};
pub use loop_core::{LoopCommand, LoopCore};
pub use process_transport::{PyProcessPipeTransport, PyProcessTransport};
pub use profiler::{profiler_running, start_profiler, stop_profiler};
pub use python_api::{future_done_stop, new_event_loop, signal_bridge, PyLoop};
pub use stream_transport::{PyServer, PyStreamTransport};

use pyo3::prelude::*;

#[pymodule(gil_used = false)]
fn _loop(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    m.add_class::<PyLoop>()?;
    m.add_class::<PyHandle>()?;
    m.add_class::<PyTimerHandle>()?;
    m.add_class::<PyProcessTransport>()?;
    m.add_class::<PyProcessPipeTransport>()?;
    m.add_class::<PyServer>()?;
    m.add_class::<PyStreamTransport>()?;
    m.add_class::<PyFastStreamReader>()?;
    m.add_class::<PyFastStreamWriter>()?;
    m.add_function(wrap_pyfunction!(new_event_loop, m)?)?;
    m.add_function(wrap_pyfunction!(future_done_stop, m)?)?;
    m.add_function(wrap_pyfunction!(open_connection, m)?)?;
    m.add_function(wrap_pyfunction!(profiler_running, m)?)?;
    m.add_function(wrap_pyfunction!(signal_bridge, m)?)?;
    m.add_function(wrap_pyfunction!(start_profiler, m)?)?;
    m.add_function(wrap_pyfunction!(start_server, m)?)?;
    m.add_function(wrap_pyfunction!(stop_profiler, m)?)?;
    m.add("_future_done_stop", m.getattr("future_done_stop")?)?;
    Ok(())
}
