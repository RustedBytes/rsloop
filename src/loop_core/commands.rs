use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use pyo3::prelude::*;

use crate::callbacks::{PyHandle, ReadyCallback};
use crate::fd_ops::RawFd;
use crate::process_transport::ProcessTransportCore;
use crate::stream_transport::{
    AcceptedStream, ReaderTarget, ServerCore, ServerListener, StreamTransportCore,
};

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

pub enum LoopCommand {
    ScheduleReady(Arc<ReadyCallback>),
    ScheduleReadyHandle(Py<PyHandle>),
    ScheduleTimer {
        callback: Arc<ReadyCallback>,
        when: Instant,
    },
    Run(LoopRunCommand),
    Signal(LoopSignalCommand),
    Io(LoopIoCommand),
    Future(LoopFutureCommand),
    Transport(LoopTransportCommand),
    #[cfg(unix)]
    ConnectCompleted {
        future: Py<PyAny>,
        fd: RawFd,
        wait_errno: i32,
    },
    RequestStop,
    Close,
}

pub enum LoopRunCommand {
    EnterRun {
        pending_ready: Arc<Mutex<VecDeque<ReadyItem>>>,
    },
    FinishRun {
        done_tx: std::sync::mpsc::Sender<()>,
    },
}

pub enum LoopSignalCommand {
    StartWatcher(i32),
    StopWatcher(i32),
    Fired(i32),
}

pub enum LoopIoCommand {
    // Fd watches hold one persistent ReadyCallback per registration, shared
    // with LoopState's keepalive map. Every readiness event schedules the
    // same callback, so remove_reader()/remove_writer() can cancel pending
    // fires exactly like asyncio's Handle.cancel() on its reader handle.
    StartReader {
        fd: RawFd,
        callback: Arc<ReadyCallback>,
    },
    StopReader(RawFd),
    StartWriter {
        fd: RawFd,
        callback: Arc<ReadyCallback>,
    },
    StopWriter(RawFd),
    StartSocketReader {
        fd: RawFd,
        core: Arc<StreamTransportCore>,
        reader: ReaderTarget,
    },
    StopSocketReader(RawFd),
    StartServerAccept {
        fd: RawFd,
        server: Arc<ServerCore>,
        listener: ServerListener,
    },
    StopServerAccept(RawFd),
    // Watch a connecting TCP socket for writability on the vibeio reactor, then
    // report completion back to the loop thread via `ReadyItem::ConnectCompleted`.
    #[cfg(unix)]
    WatchConnect {
        fd: RawFd,
        future: Py<PyAny>,
    },
}

pub enum LoopFutureCommand {
    SetResult { future: Py<PyAny>, value: Py<PyAny> },
    SetException { future: Py<PyAny>, value: Py<PyAny> },
}

pub enum LoopTransportCommand {
    StreamRead(Arc<StreamTransportCore>),
    StreamWrite(Arc<StreamTransportCore>),
    Process(Arc<ProcessTransportCore>),
    ServerAccepted {
        server: Arc<ServerCore>,
        stream: AcceptedStream,
    },
}
