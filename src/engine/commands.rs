//! Messages exchanged by the Python loop thread, dispatcher, and I/O workers.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use pyo3::prelude::*;

use super::callbacks::{PyHandle, ReadyCallback};
use crate::fd_ops::RawFd;
use crate::transport::process::ProcessTransportCore;
use crate::transport::stream::{
    AcceptedStream, ReaderTarget, ServerCore, ServerListener, StreamTransportCore,
};

/// Work that must be completed on the Python loop thread, usually under the GIL.
pub enum ReadyItem {
    Callback(Arc<ReadyCallback>),
    HandleCallback(Py<PyHandle>),
    FutureSetResult {
        future: Py<PyAny>,
        value: Py<PyAny>,
    },
    FutureSetException {
        future: Py<PyAny>,
        value: Py<PyAny>,
    },
    StreamTransportRead(Arc<StreamTransportCore>),
    StreamTransportWrite(Arc<StreamTransportCore>),
    #[cfg(unix)]
    StartTcpReader {
        fd: RawFd,
        core: Arc<StreamTransportCore>,
        stream: Arc<std::net::TcpStream>,
    },
    ProcessTransport(Arc<ProcessTransportCore>),
    ServerAccepted {
        server: Arc<ServerCore>,
        stream: AcceptedStream,
    },
    // A non-blocking TCP connect finished its writability wait on the vibeio
    // reactor. The Python-object work (SO_ERROR check, set_result /
    // set_exception) is deferred to the loop thread so the vibeio side never
    // touches the GIL — many concurrent connects then drain in one GIL-held
    // batch instead of one contended handoff per completion.
    #[cfg(unix)]
    ConnectCompleted {
        future: Py<PyAny>,
        fd: RawFd,
        wait_errno: i32,
    },
    Stop,
}

/// Control-plane messages consumed by the dedicated runtime dispatcher.
pub enum LoopCommand {
    /// Enqueues a callback whose lifetime is shared independently of Python.
    ScheduleReady(Arc<ReadyCallback>),
    /// Enqueues a callback owned by its Python `Handle`.
    ScheduleReadyHandle(Py<PyHandle>),
    /// Registers a callback for execution at a monotonic deadline.
    ScheduleTimer {
        /// Callback to execute.
        callback: Arc<ReadyCallback>,
        /// Monotonic deadline on the runtime clock.
        when: Instant,
    },
    /// Changes the loop's active run session.
    Run(LoopRunCommand),
    /// Starts, stops, or delivers an operating-system signal watcher.
    Signal(LoopSignalCommand),
    /// Starts or stops descriptor-backed I/O work.
    Io(LoopIoCommand),
    /// Resolves a Python future on the loop thread.
    Future(LoopFutureCommand),
    /// Delivers work emitted by a transport or server.
    Transport(LoopTransportCommand),
    /// Completes a non-blocking TCP connection on the loop thread.
    #[cfg(unix)]
    ConnectCompleted {
        /// Python future awaiting the connection.
        future: Py<PyAny>,
        /// Descriptor of the connecting socket.
        fd: RawFd,
        /// Error reported while waiting for writability, or zero.
        wait_errno: i32,
    },
    /// Requests that the active `run_forever` call return after ready work drains.
    RequestStop,
    /// Shuts down the dispatcher and its runtime thread.
    Close,
}

/// Commands that establish and finish one invocation of the loop runner.
pub enum LoopRunCommand {
    /// Installs the ready queue used by the active loop-thread run.
    EnterRun {
        /// Queue shared between producers and the loop-thread drain.
        pending_ready: Arc<Mutex<VecDeque<ReadyItem>>>,
    },
    /// Clears the active run and acknowledges dispatcher cleanup.
    FinishRun {
        /// One-shot acknowledgement sent after cleanup is complete.
        done_tx: std::sync::mpsc::Sender<()>,
    },
}

/// Commands for managing and delivering Unix signal watchers.
pub enum LoopSignalCommand {
    /// Starts watching the given signal number.
    StartWatcher(i32),
    /// Stops watching the given signal number.
    StopWatcher(i32),
    /// Reports that the given signal number was received.
    Fired(i32),
}

/// Commands for descriptor watchers, socket readers, and server accept loops.
pub enum LoopIoCommand {
    // Fd watches hold one persistent ReadyCallback per registration, shared
    // with LoopState's keepalive map. Every readiness event schedules the
    // same callback, so remove_reader()/remove_writer() can cancel pending
    // fires exactly like asyncio's Handle.cancel() on its reader handle.
    /// Starts an ordinary readable-descriptor callback.
    StartReader {
        /// Descriptor to watch.
        fd: RawFd,
        /// Persistent callback scheduled for each readiness notification.
        callback: Arc<ReadyCallback>,
    },
    /// Stops the readable-descriptor callback for the given descriptor.
    StopReader(RawFd),
    /// Starts an ordinary writable-descriptor callback.
    StartWriter {
        /// Descriptor to watch.
        fd: RawFd,
        /// Persistent callback scheduled for each readiness notification.
        callback: Arc<ReadyCallback>,
    },
    /// Stops the writable-descriptor callback for the given descriptor.
    StopWriter(RawFd),
    /// Starts a transport-owned socket reader.
    StartSocketReader {
        /// Descriptor identifying the task in the runtime registry.
        fd: RawFd,
        /// Transport that receives the read events.
        core: Arc<StreamTransportCore>,
        /// Owned endpoint from which the runtime reads.
        reader: ReaderTarget,
    },
    /// Cancels a transport-owned socket reader.
    StopSocketReader {
        /// Descriptor identifying the registered task.
        fd: RawFd,
        /// Acknowledgement sent after cancellation is processed.
        done_tx: std::sync::mpsc::Sender<()>,
    },
    /// Starts accepting connections for a server listener.
    StartServerAccept {
        /// Descriptor identifying the accept task.
        fd: RawFd,
        /// Server that owns accepted connections.
        server: Arc<ServerCore>,
        /// Listener consumed by the accept task.
        listener: ServerListener,
    },
    /// Stops the accept task registered for the given descriptor.
    StopServerAccept(RawFd),
    /// Watches a connecting TCP socket for writability.
    ///
    /// Completion is reported back through `LoopCommand::ConnectCompleted`.
    #[cfg(unix)]
    WatchConnect {
        /// Descriptor of the connecting socket.
        fd: RawFd,
        /// Python future awaiting connection completion.
        future: Py<PyAny>,
    },
}

/// Commands that settle Python futures on the loop thread.
pub enum LoopFutureCommand {
    /// Resolves `future` successfully.
    SetResult {
        /// Future to resolve.
        future: Py<PyAny>,
        /// Successful result value.
        value: Py<PyAny>,
    },
    /// Resolves `future` with an exception.
    SetException {
        /// Future to resolve.
        future: Py<PyAny>,
        /// Python exception instance.
        value: Py<PyAny>,
    },
}

/// Events that move completed transport work onto the Python loop thread.
pub enum LoopTransportCommand {
    /// Drains pending read events for a stream transport.
    StreamRead(Arc<StreamTransportCore>),
    /// Processes pending write state for a stream transport.
    StreamWrite(Arc<StreamTransportCore>),
    /// Drains pending subprocess events.
    Process(Arc<ProcessTransportCore>),
    /// Builds a protocol and transport around an accepted connection.
    ServerAccepted {
        /// Server that accepted the connection.
        server: Arc<ServerCore>,
        /// Accepted socket and its peer metadata.
        stream: AcceptedStream,
    },
}
