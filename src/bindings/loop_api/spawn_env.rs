//! The per-call environment every transport-creating loop method captures.
//!
//! `create_connection`, `create_server`, `subprocess_exec`, `connect_read_pipe`
//! and friends all start by snapshotting the same four values on the calling
//! thread — the loop core, the loop object, and the caller's context — and then
//! move that snapshot into the Rust future that builds the transport. All four
//! fields are owned, so the snapshot crosses into an `async move` block as one
//! unit.

use std::sync::Arc;

use pyo3::{prelude::*, types::PyTuple};

use super::PyLoop;
use crate::{
    context::{capture_context, ensure_running_loop, run_in_context},
    engine::LoopCore,
    transport::stream::TransportSpawnContext,
};

pub(super) struct LoopSpawnEnv {
    pub(super) core: Arc<LoopCore>,
    pub(super) loop_obj: Py<PyAny>,
    pub(super) context: Py<PyAny>,
    pub(super) context_needs_run: bool,
}

impl LoopSpawnEnv {
    /// Snapshots the loop and the caller's context. Must run on the calling
    /// thread, before the transport future is constructed.
    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "LoopSpawnEnv"))]
    pub(super) fn capture(py: Python<'_>, slf: &Py<PyLoop>) -> PyResult<Self> {
        let loop_obj = PyLoop::as_py_any(py, slf);
        let core = slf.borrow(py).core.clone();
        let (context, context_needs_run) = capture_context(py, None)?;
        Ok(Self {
            core,
            loop_obj,
            context,
            context_needs_run,
        })
    }

    /// Builds the spawn context handed to the transport constructors.
    /// `protocol` is the protocol instance, or the protocol factory for
    /// server creation.
    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "LoopSpawnEnv"))]
    pub(super) fn spawn_context(
        &self,
        py: Python<'_>,
        protocol: &Py<PyAny>,
    ) -> TransportSpawnContext {
        TransportSpawnContext::new(
            py,
            Arc::clone(&self.core),
            &self.loop_obj,
            protocol.clone_ref(py),
            &self.context,
            self.context_needs_run,
        )
    }

    /// Instantiates the protocol in the caller's context, rejecting the call if
    /// the loop is no longer running.
    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "LoopSpawnEnv"))]
    pub(super) fn call_protocol_factory(
        &self,
        py: Python<'_>,
        protocol_factory: &Py<PyAny>,
    ) -> PyResult<Py<PyAny>> {
        ensure_running_loop(py, &self.loop_obj)?;
        let args = PyTuple::empty(py).unbind();
        run_in_context(
            py,
            &self.context,
            self.context_needs_run,
            protocol_factory,
            &args,
        )
    }
}

/// The `(transport, protocol)` tuple every `asyncio` transport-creating method
/// resolves to.
#[cfg_attr(feature = "profile", hotpath::measure)]
pub(super) fn transport_protocol_pair(
    py: Python<'_>,
    transport: Py<PyAny>,
    protocol: &Py<PyAny>,
) -> PyResult<Py<PyAny>> {
    let result = PyTuple::new(py, [transport, protocol.clone_ref(py)])?;
    Ok(result.unbind().into_any())
}

#[cfg_attr(feature = "profile", hotpath::measure)]
pub(super) fn is_asyncio_subprocess_stream_protocol(
    py: Python<'_>,
    protocol: &Py<PyAny>,
) -> PyResult<bool> {
    let asyncio_subprocess = py.import("asyncio.subprocess")?;
    let cls = asyncio_subprocess.getattr("SubprocessStreamProtocol")?;
    protocol.bind(py).is_instance(&cls)
}
