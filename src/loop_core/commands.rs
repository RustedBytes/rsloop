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
    ProcessTransport(Arc<ProcessTransportCore>),
    ServerAccepted {
        server: Arc<ServerCore>,
        stream: AcceptedStream,
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
    RequestStop,
    Close,
}

pub enum LoopRunCommand {
    EnterRun {
        pending_ready: Arc<Mutex<VecDeque<ReadyItem>>>,
        wake_tx: std::sync::mpsc::Sender<()>,
        wake_pending: Arc<std::sync::atomic::AtomicBool>,
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
}

pub enum LoopFutureCommand {
    SetResult { future: Py<PyAny>, value: Py<PyAny> },
    SetException { future: Py<PyAny>, value: Py<PyAny> },
}

pub enum LoopTransportCommand {
    StreamRead(Arc<StreamTransportCore>),
    Process(Arc<ProcessTransportCore>),
    ServerAccepted {
        server: Arc<ServerCore>,
        stream: AcceptedStream,
    },
}
