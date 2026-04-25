use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Instant;

use pyo3::prelude::*;
use pyo3::types::PyTuple;

use crate::callbacks::ReadyCallback;
use crate::fd_ops::RawFd;
use crate::process_transport::ProcessTransportCore;
use crate::stream_transport::{ReaderTarget, ServerCore, ServerListener, StreamTransportCore};

pub enum ReadyItem {
    Callback(Arc<ReadyCallback>),
    FutureSetResult { future: Py<PyAny>, value: Py<PyAny> },
    FutureSetException { future: Py<PyAny>, value: Py<PyAny> },
    StreamTransportRead(Arc<StreamTransportCore>),
    ProcessTransport(Arc<ProcessTransportCore>),
    Stop,
}

pub enum LoopCommand {
    ScheduleReady(Arc<ReadyCallback>),
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
    StartReader {
        fd: RawFd,
        callback: Py<PyAny>,
        args: Py<PyTuple>,
        context: Py<PyAny>,
        context_needs_run: bool,
    },
    StopReader(RawFd),
    StartWriter {
        fd: RawFd,
        callback: Py<PyAny>,
        args: Py<PyTuple>,
        context: Py<PyAny>,
        context_needs_run: bool,
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
}
