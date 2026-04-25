mod async_event;
mod blocking;
mod callbacks;
mod context;
mod errors;
mod fast_streams;
mod fd_ops;
mod loop_core;
mod module_init;
mod process_transport;
mod profiler;
mod python_api;
mod python_names;
mod runtime;
pub mod rust_async;
mod stream_transport;
mod tls;
#[cfg(windows)]
mod windows_vibeio;

pub use callbacks::{PyHandle, PyTimerHandle, ReadyCallback};
pub use fast_streams::{open_connection, start_server, PyFastStreamReader, PyFastStreamWriter};
pub use loop_core::{
    LoopCommand, LoopCore, LoopFutureCommand, LoopIoCommand, LoopRunCommand, LoopSignalCommand,
    LoopTransportCommand,
};
pub use process_transport::{PyProcessPipeTransport, PyProcessTransport};
pub use profiler::{profiler_running, start_profiler, stop_profiler};
pub use python_api::{
    asyncgen_finalizer_hook, asyncgen_firstiter_hook, future_done_stop, new_event_loop,
    signal_bridge, PyLoop,
};
pub use stream_transport::{PyServer, PyStreamTransport};

use pyo3::prelude::*;

#[pymodule(gil_used = false)]
fn _loop(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    module_init::add_module_contents(m)
}
