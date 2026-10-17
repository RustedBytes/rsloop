//! Messages exchanged by the Python loop thread, dispatcher, and I/O workers.

use std::{
    collections::VecDeque,
    sync::{Arc, Mutex},
};

use pyo3::prelude::*;

use super::callbacks::{PyHandle, ReadyCallback, TimerCallback};
use crate::{
    fd_ops::RawFd,
    transport::{
        process::ProcessTransportCore,
        stream::{
            AcceptedStream, ReaderTarget, ServerAcceptTaskGuard, ServerCore, ServerListener,
            StreamTransportCore,
        },
    },
};

/// Work that must be completed on the Python loop thread, usually under the
/// GIL.
pub enum ReadyItem {
    Callback(Arc<ReadyCallback>),
    TimerCallback(Arc<TimerCallback>),
    HandleCallback(Py<PyHandle>),
    FutureSetResult(Box<FutureCompletion>),
    FutureSetException(Box<FutureCompletion>),
    StreamTransportRead(Arc<StreamTransportCore>),
    StreamTransportWrite(Arc<StreamTransportCore>),
    #[cfg(unix)]
    StartSocketReader(Box<SocketReaderStart>),
    ProcessTransport(Arc<ProcessTransportCore>),
    ServerAccepted(Box<AcceptedConnection>),
    StartServerAccept(Box<ServerAcceptStart>),
    StopServerAccept(RawFd),
    // A non-blocking TCP connect finished its writability wait on the vibeio
    // reactor. The Python-object work (SO_ERROR check, set_result /
    // set_exception) is deferred to the loop thread so the vibeio side never
    // touches the GIL — many concurrent connects then drain in one GIL-held
    // batch instead of one contended handoff per completion.
    #[cfg(unix)]
    ConnectCompleted(Box<ConnectCompletion>),
    #[cfg(unix)]
    LoopWork(Box<LoopWork>),
    Stop,
}

// Keep every ready slot within two pointer words. Multiword control-plane
// payloads allocate separately; ordinary callbacks and stream-read/write
// notifications remain allocation-free at the queue boundary.
const _: () = assert!(std::mem::size_of::<ReadyItem>() <= 2 * std::mem::size_of::<usize>());

/// The loop-thread half of work finished off the loop thread: it builds the
/// Python result and settles a future, so the worker never takes the GIL.
#[cfg(unix)]
pub struct LoopWork(pub Box<dyn FnOnce(Python<'_>) -> PyResult<()> + Send>);

pub struct FutureCompletion {
    pub future: Py<PyAny>,
    pub value: Py<PyAny>,
}

pub struct AcceptedConnection {
    pub server: Arc<ServerCore>,
    pub stream: AcceptedStream,
}

#[cfg(unix)]
pub struct ConnectCompletion {
    pub future: Py<PyAny>,
    pub fd: RawFd,
    pub wait_errno: i32,
}

// Reader startup is infrequent compared with callback dispatch. Keep its
// three-word payload out of line so it does not enlarge every ready-queue slot.
#[cfg(unix)]
pub struct SocketReaderStart {
    pub fd: RawFd,
    pub core: Arc<StreamTransportCore>,
    pub reader: ReaderTarget,
}

pub struct ServerAcceptStart {
    pub fd: RawFd,
    pub server: Arc<ServerCore>,
    pub listener: ServerListener,
    pub task_guard: ServerAcceptTaskGuard,
}

/// Off-thread ready items queued for the active run.
///
/// The loop thread schedules into its own queue without a lock, so the two
/// sources only meet when the drain starts the next generation. Each item
/// here records `LoopCore::local_enqueues` as read when it was queued: it was
/// scheduled after that many loop-thread enqueues, and the drain merges it
/// behind exactly those (see `merge_ready_generation`). Marks are stored once
/// per change, as `(index, mark)` pairs, because a burst of off-thread work
/// between two loop-thread enqueues shares one.
pub struct PendingReady<T = ReadyItem> {
    pub(crate) items: VecDeque<T>,
    pub(crate) marks: Vec<(usize, u64)>,
}

impl<T> Default for PendingReady<T> {
    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "<PendingReady as Default>")
    )]
    fn default() -> Self {
        Self {
            items: VecDeque::new(),
            marks: Vec::new(),
        }
    }
}

impl<T> PendingReady<T> {
    /// Queues `item`, scheduled after `local_mark` loop-thread enqueues.
    #[inline]
    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "PendingReady"))]
    pub(crate) fn push(&mut self, item: T, local_mark: u64) {
        self.mark(local_mark);
        self.items.push_back(item);
    }

    /// Queues `items` in order, all scheduled after `local_mark` loop-thread
    /// enqueues.
    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "PendingReady"))]
    pub(crate) fn extend(&mut self, items: impl IntoIterator<Item = T>, local_mark: u64) {
        let (len, marks) = (self.items.len(), self.marks.len());
        self.mark(local_mark);
        self.items.extend(items);
        if self.items.len() == len {
            self.marks.truncate(marks);
        }
    }

    /// Takes every item out of scheduling order tracking, once the run that
    /// could merge it with loop-thread work has ended.
    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "PendingReady"))]
    pub(crate) fn take_items(&mut self) -> VecDeque<T> {
        self.marks.clear();
        std::mem::take(&mut self.items)
    }

    #[inline]
    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "PendingReady"))]
    fn mark(&mut self, local_mark: u64) {
        if self
            .marks
            .last()
            .is_none_or(|&(_, mark)| mark != local_mark)
        {
            self.marks.push((self.items.len(), local_mark));
        }
    }
}

/// Control-plane messages consumed by the dedicated runtime dispatcher.
pub enum LoopCommand {
    /// Enqueues a callback whose lifetime is shared independently of Python.
    ScheduleReady(Arc<ReadyCallback>),
    /// Enqueues a callback owned by its Python `Handle`.
    ScheduleReadyHandle(Py<PyHandle>),
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
    /// Runs the loop-thread half of off-thread work.
    #[cfg(unix)]
    LoopWork(Box<LoopWork>),
    /// Requests that the active `run_forever` call return after ready work
    /// drains.
    RequestStop,
    /// Shuts down the dispatcher and its runtime thread.
    Close,
}

/// Commands that establish and finish one invocation of the loop runner.
pub enum LoopRunCommand {
    /// Installs the ready queue used by the active loop-thread run.
    EnterRun {
        /// Queue shared between producers and the loop-thread drain.
        pending_ready: Arc<Mutex<PendingReady>>,
        /// `LoopCore::local_enqueues` before the run could enqueue anything:
        /// work queued before the run goes ahead of all of the run's own.
        local_mark: u64,
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
        /// Lifetime token released after the task drops its listener.
        task_guard: ServerAcceptTaskGuard,
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
