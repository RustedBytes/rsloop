//! Subprocess lifecycle and pipe transport implementation.
//!
//! This file holds what the subprocess modules share: `ProcessTransportCore`
//! and the state it guards, the messages passed between the loop thread and the
//! worker threads, and the pyclasses Python sees. Behaviour lives alongside:
//!
//! - [`params`] — the inputs the binding layer assembles, and [`spawn`], which
//!   turns them plus a `Child` into a live transport
//! - [`core_events`], [`core_protocol`], [`core_state`] — the core's own
//!   behaviour, split by concern
//! - [`py_transport`], [`py_pipes`] — the Python surface
//! - [`worker`] — the blocking reader and waiter threads
//!
//! The threading rule the whole module is built around: worker threads never
//! call Python. They enqueue events and the loop thread drains them, which is
//! why the queue and the protocol dispatch are separate modules.
//!
//! Submodules reach back into the private fields declared here, so those stay
//! private — the module tree is the encapsulation boundary, not the file.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{Arc, Mutex, atomic::AtomicBool, mpsc::Sender},
};

use pyo3::prelude::*;

use crate::{async_event::AsyncEvent, engine::LoopCore};

mod core_events;
mod core_protocol;
mod core_state;
mod params;
mod py_pipes;
mod py_transport;
mod spawn;
mod worker;

pub use params::{BoxedProcessReader, ProcessTextConfig, ProcessTransportParams};
pub use spawn::spawn_process_transport;

enum ProcessCommand {
    Close,
    SendSignal(i32),
    Terminate,
    Kill,
}

enum PendingProcessEvent {
    PipeDataReceived { fd: i32, data: Box<[u8]> },
    PipeConnectionLost { fd: i32, exc: Option<String> },
    ProcessExited { returncode: i32 },
    ConnectionLost { exc: Option<String> },
}

struct ProcessState {
    protocol: Py<PyAny>,
    context: Py<PyAny>,
    context_needs_run: bool,
    pid: u32,
    returncode: Option<i32>,
    closing: bool,
    exited: bool,
    connection_lost_called: bool,
    open_pipes: HashSet<i32>,
    pipe_transports: HashMap<i32, Py<PyAny>>,
}

/// Shared subprocess owner. Worker threads enqueue `PendingProcessEvent`s;
/// the loop thread drains them and is the only place that calls the protocol.
pub struct ProcessTransportCore {
    loop_core: Arc<LoopCore>,
    loop_obj: Py<PyAny>,
    state: Mutex<ProcessState>,
    text_config: Option<ProcessTextConfig>,
    control_tx: Sender<ProcessCommand>,
    exit_notify: AsyncEvent,
    pending_events: Mutex<VecDeque<PendingProcessEvent>>,
    events_scheduled: AtomicBool,
}

/// Python-visible transport for a spawned child process.
///
/// Methods on this class implement the `asyncio.SubprocessTransport` surface;
/// worker threads communicate with it through the shared core.
#[pyclass(name = "ProcessTransport", module = "rsloop._loop")]
pub struct PyProcessTransport {
    /// Shared subprocess state and pending event queue.
    pub core: Arc<ProcessTransportCore>,
}

struct ProcessPipeTransportCore {
    fd: i32,
    closing: AtomicBool,
}

/// Python-visible half-duplex transport for one subprocess stdio pipe.
#[pyclass(name = "ProcessPipeTransport", module = "rsloop._loop")]
pub struct PyProcessPipeTransport {
    core: Arc<ProcessPipeTransportCore>,
}

#[pyclass(module = "rsloop._loop")]
struct PyProcessStdinProtocol {
    core: Arc<ProcessTransportCore>,
}
