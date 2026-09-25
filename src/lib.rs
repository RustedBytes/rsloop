#![warn(missing_docs)]

//! Native extension entry point and Rust interoperability API for `rsloop`.
//!
//! Most users install `rsloop` as a Python package and interact with its
//! `asyncio`-compatible classes. The Rust crate is also linkable by downstream
//! `PyO3` extensions: use [`rust_async`] to turn Rust futures into Python
//! awaitables that inherit the currently running rsloop event loop and Python
//! [`contextvars`](https://docs.python.org/3/library/contextvars.html) context.
//!
//! The remaining exports are compatibility building blocks for rsloop's native
//! Python classes and event-loop engine. [`PyLoop`] owns a [`LoopCore`], which
//! coordinates callbacks and transports while keeping Python execution on the
//! thread that runs the event loop.

mod async_event;
mod bindings;
mod blocking;
mod build_metadata;
mod context;
mod engine;
mod errors;
mod module_init;
mod platform;
mod python_names;
pub mod rust_async;
mod transport;
#[cfg(kani)]
mod verification;
#[path = "vibeio/lib.rs"]
#[allow(
    dead_code,
    reason = "The private Python embedding uses only part of the runtime API; the standalone harness checks dead code"
)]
pub(crate) mod vibeio;

pub(crate) use platform::fd as fd_ops;

// Compatibility re-exports for the crate's existing Rust API. Internal module
// registration imports from the owning modules directly, so these can be
// deprecated or versioned independently in a future breaking release.
pub use bindings::{
    PyLoop, asyncgen_finalizer_hook, asyncgen_firstiter_hook, future_done_stop, new_event_loop,
    signal_bridge,
};
pub use engine::{
    LoopCommand, LoopCore, LoopFutureCommand, LoopIoCommand, LoopRunCommand, LoopSignalCommand,
    LoopTransportCommand, PyHandle, PyTimerHandle, ReadyCallback,
};
pub use transport::process::{PyProcessPipeTransport, PyProcessTransport};
pub use transport::stream::{
    PyFastStreamReader, PyFastStreamWriter, open_connection, start_server,
};
pub use transport::stream::{PyServer, PyStreamTransport};

use pyo3::prelude::*;

#[cfg(test)]
pub(crate) fn initialize_python_for_tests() {
    static INITIALIZE: std::sync::Once = std::sync::Once::new();
    INITIALIZE.call_once(|| {
        Python::initialize();
        // The free-threaded interpreter permits Rust tests to attach in parallel. Complete the
        // imports shared by callback and transport fixtures before releasing the `Once`, otherwise
        // concurrent first imports can observe a partially initialized `asyncio` package.
        Python::attach(|py| {
            py.import("asyncio").expect("preload asyncio for tests");
            py.import("contextvars")
                .expect("preload contextvars for tests");
        });
    });
}

// The mutable-buffer fast paths no longer rely on GIL serialization: the only
// Python-visible buffer rsloop writes through a raw pointer is the generic
// stream reader's `bytearray`, and that resize-and-copy now runs inside a
// critical section on the buffer itself (`transport::stream::protocol`). Every
// other raw CPython call either targets an object the caller exclusively owns
// (the `readexactly` accumulator's private `bytes`) or is an ordinary
// thread-safe C API call. Shared Rust state is behind mutexes, atomics, or
// `PyOnceLock`, and `#[pyclass]` interior mutability uses PyO3's atomic borrow
// flags, so importing under a free-threaded interpreter no longer needs to
// re-enable the GIL.
#[pymodule(gil_used = false)]
fn _loop(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add("__version__", env!("CARGO_PKG_VERSION"))?;
    module_init::add_module_contents(m)
}
