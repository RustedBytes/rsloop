//! Coordination-thread dispatcher for commands and compatibility watchers.
//!
//! It prepares `ReadyItem`s but leaves Python callback execution to the loop
//! thread. Direct stream I/O can instead run on the loop thread's own reactor.

#[cfg(not(unix))]
use std::collections::HashSet;
use std::collections::{HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::task::{Context, Poll};
use std::thread;

use super::commands::{
    LoopCommand, LoopFutureCommand, LoopIoCommand, LoopRunCommand, LoopSignalCommand,
    LoopTransportCommand, ReadyItem,
};
use super::loop_core::LoopCore;
use crate::fd_ops;
use crossbeam_channel::{Receiver, TryRecvError};
use pyo3::prelude::*;
#[cfg(unix)]
use signal_hook::iterator::{Handle as SignalHandle, Signals};

/// Long-lived future driven by the coordination thread's `vibeio` runtime.
struct RuntimeDispatcher {
    core: Arc<LoopCore>,
    command_rx: Receiver<LoopCommand>,
    ready_batch: VecDeque<ReadyItem>,
    active_run: Option<ActiveRun>,
    #[cfg(unix)]
    signal_tasks: HashMap<i32, SignalWatcher>,
    #[cfg(not(unix))]
    signal_tasks: HashSet<i32>,
    reader_tasks: HashMap<fd_ops::RawFd, WatchTask>,
    writer_tasks: HashMap<fd_ops::RawFd, WatchTask>,
    accept_tasks: HashMap<fd_ops::RawFd, WatchTask>,
    shutting_down: bool,
}

/// Queue shared with the Python loop thread while `run_forever` is active.
struct ActiveRun {
    pending_ready: Arc<std::sync::Mutex<VecDeque<ReadyItem>>>,
}

#[cfg(unix)]
struct SignalWatcher {
    handle: SignalHandle,
    join: thread::JoinHandle<()>,
}

/// A watcher may be an older helper thread or a native `vibeio` task.
enum WatchTask {
    Thread {
        stop: Arc<AtomicBool>,
        join: thread::JoinHandle<()>,
    },
    Vibeio(crate::vibeio::JoinHandle<()>),
}

impl WatchTask {
    fn spawn_thread(name: String, task: impl FnOnce(Arc<AtomicBool>) + Send + 'static) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let join = thread::Builder::new()
            .name(name)
            .spawn(move || task(thread_stop))
            .expect("failed to spawn watch task");
        Self::Thread { stop, join }
    }

    fn abort(self) {
        match self {
            Self::Thread { stop, join } => {
                stop.store(true, AtomicOrdering::Release);
                let _ = join.join();
            }
            Self::Vibeio(task) => task.cancel(),
        }
    }

    fn cancel(self) {
        match self {
            Self::Thread { stop, .. } => {
                stop.store(true, AtomicOrdering::Release);
            }
            Self::Vibeio(task) => task.cancel(),
        }
    }
}

#[inline]
fn abort_watch_task(task: WatchTask) {
    task.abort();
}

#[inline]
fn cancel_watch_task(task: WatchTask) {
    task.cancel();
}

pub fn run_runtime_thread(core: Arc<LoopCore>, command_rx: Receiver<LoopCommand>) {
    let runtime = crate::vibeio::RuntimeBuilder::new()
        .rsloop_profile()
        .enable_timer(true)
        .build()
        .expect("failed to initialize vibeio runtime");
    let dispatcher = RuntimeDispatcher {
        core: Arc::clone(&core),
        command_rx,
        ready_batch: VecDeque::new(),
        active_run: None,
        #[cfg(unix)]
        signal_tasks: HashMap::new(),
        #[cfg(not(unix))]
        signal_tasks: HashSet::new(),
        reader_tasks: HashMap::new(),
        writer_tasks: HashMap::new(),
        accept_tasks: HashMap::new(),
        shutting_down: false,
    };
    runtime.block_on(dispatcher);
    core.set_runtime_waker(None);
}

impl Future for RuntimeDispatcher {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        // Register before inspecting the command channel so a concurrent send
        // either wakes this poll or remains visible when the channel is
        // drained.  The retained waker is intentional: consuming it while the
        // dispatcher task is already queued can strand the following command.
        self.core.set_runtime_waker(Some(cx.waker().clone()));
        loop {
            if self.shutting_down {
                return Poll::Ready(());
            }

            if self.active_run.is_some() && self.has_ready() {
                self.dispatch_ready_batch();
            }

            if self.drain_commands() {
                return Poll::Ready(());
            }
            if self.active_run.is_some() && self.has_ready() {
                self.dispatch_ready_batch();
                continue;
            }

            return Poll::Pending;
        }
    }
}

impl RuntimeDispatcher {
    #[inline]
    fn has_ready(&self) -> bool {
        !self.ready_batch.is_empty()
    }

    fn drain_commands(&mut self) -> bool {
        loop {
            match self.command_rx.try_recv() {
                Ok(command) => {
                    if self.handle_command(command) {
                        return true;
                    }
                }
                Err(TryRecvError::Empty) => return false,
                Err(TryRecvError::Disconnected) => return true,
            }
        }
    }

    fn handle_command(&mut self, command: LoopCommand) -> bool {
        match command {
            LoopCommand::ScheduleReady(callback) => {
                self.ready_batch.push_back(ReadyItem::Callback(callback));
            }
            LoopCommand::ScheduleReadyHandle(handle) => {
                self.ready_batch
                    .push_back(ReadyItem::HandleCallback(handle));
            }
            LoopCommand::Future(LoopFutureCommand::SetResult { future, value }) => {
                self.ready_batch
                    .push_back(ReadyItem::FutureSetResult { future, value });
            }
            LoopCommand::Future(LoopFutureCommand::SetException { future, value }) => {
                self.ready_batch
                    .push_back(ReadyItem::FutureSetException { future, value });
            }
            LoopCommand::Transport(LoopTransportCommand::StreamRead(core)) => {
                self.ready_batch
                    .push_back(ReadyItem::StreamTransportRead(core));
            }
            LoopCommand::Transport(LoopTransportCommand::StreamWrite(core)) => {
                self.ready_batch
                    .push_back(ReadyItem::StreamTransportWrite(core));
            }
            LoopCommand::Transport(LoopTransportCommand::Process(core)) => {
                self.ready_batch
                    .push_back(ReadyItem::ProcessTransport(core));
            }
            LoopCommand::Transport(LoopTransportCommand::ServerAccepted { server, stream }) => {
                self.ready_batch
                    .push_back(ReadyItem::ServerAccepted { server, stream });
            }
            LoopCommand::Run(LoopRunCommand::EnterRun { pending_ready }) => {
                self.active_run = Some(ActiveRun { pending_ready });
                self.dispatch_ready_batch();
            }
            LoopCommand::Run(LoopRunCommand::FinishRun { done_tx }) => {
                self.finish_run();
                let _ = done_tx.send(());
            }
            LoopCommand::Signal(LoopSignalCommand::StartWatcher(sig)) => {
                #[cfg(unix)]
                {
                    if let Some(watcher) = self.signal_tasks.remove(&sig) {
                        watcher.handle.close();
                        let _ = watcher.join.join();
                    }

                    let sender = self.core.clone();
                    let mut signals = match Signals::new([sig]) {
                        Ok(signals) => signals,
                        Err(_) => return false,
                    };
                    let handle = signals.handle();
                    let join = thread::Builder::new()
                        .name(format!("rsloop-signal-{sig}"))
                        .spawn(move || {
                            for delivered in signals.forever() {
                                let _ = sender.send_command(LoopCommand::Signal(
                                    LoopSignalCommand::Fired(delivered),
                                ));
                            }
                        })
                        .expect("failed to spawn signal watcher thread");

                    self.signal_tasks
                        .insert(sig, SignalWatcher { handle, join });
                }
                #[cfg(not(unix))]
                {
                    let _ = sig;
                }
            }
            LoopCommand::Signal(LoopSignalCommand::StopWatcher(sig)) => {
                #[cfg(unix)]
                if let Some(watcher) = self.signal_tasks.remove(&sig) {
                    watcher.handle.close();
                    let _ = watcher.join.join();
                }
                #[cfg(not(unix))]
                {
                    let _ = sig;
                }
            }
            LoopCommand::Signal(LoopSignalCommand::Fired(sig)) => {
                let maybe_ready = Python::attach(|py| -> PyResult<Option<_>> {
                    let (callback, args, context, context_needs_run) = {
                        let state = self.core.state.lock().expect("poisoned loop state");
                        let Some(handler) = state.signal_handlers.get(&sig) else {
                            return Ok(None);
                        };
                        (
                            handler.callback.clone_ref(py),
                            handler.args.clone_ref(py),
                            handler.context.clone_ref(py),
                            handler.context_needs_run,
                        )
                    };

                    Ok(Some(Arc::new(super::callbacks::ReadyCallback::new(
                        py,
                        self.core.next_callback_id(),
                        super::callbacks::CallbackKind::Signal(sig),
                        callback,
                        args,
                        context,
                        context_needs_run,
                    ))))
                });

                if let Ok(Some(ready)) = maybe_ready {
                    self.ready_batch.push_back(ReadyItem::Callback(ready));
                }
            }
            LoopCommand::Io(LoopIoCommand::StartReader { fd, callback }) => {
                if let Some(task) = self.reader_tasks.remove(&fd) {
                    cancel_watch_task(task);
                }

                let sender = Arc::clone(&self.core);
                let task = WatchTask::spawn_thread(format!("rsloop-reader-{fd}"), move |stop| {
                    loop {
                        if stop.load(AtomicOrdering::Acquire) {
                            return;
                        }
                        match fd_ops::poll_fd(fd, true, false, 50) {
                            Ok((true, _)) => break,
                            Ok((false, _)) => continue,
                            Err(_) => return,
                        }
                    }

                    if stop.load(AtomicOrdering::Acquire) || callback.cancelled() {
                        return;
                    }

                    let _ = sender.send_command(LoopCommand::ScheduleReady(callback));
                });

                self.reader_tasks.insert(fd, task);
            }
            LoopCommand::Io(LoopIoCommand::StopReader(fd)) => {
                if let Some(task) = self.reader_tasks.remove(&fd) {
                    cancel_watch_task(task);
                }
            }
            LoopCommand::Io(LoopIoCommand::StartWriter { fd, callback }) => {
                if let Some(task) = self.writer_tasks.remove(&fd) {
                    cancel_watch_task(task);
                }

                let sender = Arc::clone(&self.core);
                let task = WatchTask::spawn_thread(
                    format!("rsloop-writer-{fd}"),
                    move |stop: Arc<AtomicBool>| {
                        loop {
                            if stop.load(AtomicOrdering::Acquire) {
                                return;
                            }
                            match fd_ops::poll_fd(fd, false, true, 50) {
                                Ok((false, true)) => break,
                                Ok((false, false) | (true, _)) => continue,
                                Err(_) => return,
                            }
                        }

                        if stop.load(AtomicOrdering::Acquire) || callback.cancelled() {
                            return;
                        }

                        let _ = sender.send_command(LoopCommand::ScheduleReady(callback));
                    },
                );

                self.writer_tasks.insert(fd, task);
            }
            LoopCommand::Io(LoopIoCommand::StopWriter(fd)) => {
                if let Some(task) = self.writer_tasks.remove(&fd) {
                    cancel_watch_task(task);
                }
            }
            LoopCommand::Io(LoopIoCommand::StartSocketReader { fd, core, reader }) => {
                if let Some(task) = self.reader_tasks.remove(&fd) {
                    cancel_watch_task(task);
                }

                let task = match reader {
                    crate::transport::stream::ReaderTarget::Tcp(stream) => {
                        WatchTask::Vibeio(crate::vibeio::spawn(
                            crate::transport::stream::run_tcp_socket_reader_task(core, stream),
                        ))
                    }
                    #[cfg(unix)]
                    crate::transport::stream::ReaderTarget::Unix(stream) => {
                        WatchTask::Vibeio(crate::vibeio::spawn(
                            crate::transport::stream::run_unix_socket_reader_task(core, stream),
                        ))
                    }
                    other @ crate::transport::stream::ReaderTarget::File(_) => {
                        WatchTask::spawn_thread(format!("rsloop-socket-reader-{fd}"), move |stop| {
                            crate::transport::stream::run_socket_reader_blocking(core, other, stop)
                        })
                    }
                };

                self.reader_tasks.insert(fd, task);
            }
            LoopCommand::Io(LoopIoCommand::StopSocketReader { fd, done_tx }) => {
                if let Some(task) = self.reader_tasks.remove(&fd) {
                    cancel_watch_task(task);
                }
                let _ = done_tx.send(());
            }
            LoopCommand::Io(LoopIoCommand::StartServerAccept {
                fd,
                server,
                listener,
            }) => {
                if let Some(task) = self.accept_tasks.remove(&fd) {
                    cancel_watch_task(task);
                }

                let task = WatchTask::Vibeio(crate::vibeio::spawn(
                    crate::transport::stream::run_server_accept_task(server, listener),
                ));

                self.accept_tasks.insert(fd, task);
            }
            LoopCommand::Io(LoopIoCommand::StopServerAccept(fd)) => {
                if let Some(task) = self.accept_tasks.remove(&fd) {
                    cancel_watch_task(task);
                }
            }
            #[cfg(unix)]
            LoopCommand::Io(LoopIoCommand::WatchConnect { fd, future }) => {
                // Detached: the task self-reports via ConnectCompleted, and a
                // vibeio JoinHandle has no Drop, so dropping it detaches (does
                // not cancel) the running task.
                let core = Arc::clone(&self.core);
                std::mem::drop(crate::vibeio::spawn(
                    crate::transport::stream::run_connect_watch_task(core, fd, future),
                ));
            }
            #[cfg(unix)]
            LoopCommand::ConnectCompleted {
                future,
                fd,
                wait_errno,
            } => {
                self.ready_batch.push_back(ReadyItem::ConnectCompleted {
                    future,
                    fd,
                    wait_errno,
                });
            }
            LoopCommand::RequestStop => {
                self.ready_batch.push_back(ReadyItem::Stop);
            }
            LoopCommand::Close => {
                self.finish_run();
                self.cleanup_watchers();
                self.shutting_down = true;
                return true;
            }
        }

        false
    }

    fn dispatch_ready_batch(&mut self) {
        let Some(active_run) = self.active_run.as_ref() else {
            return;
        };

        if self.ready_batch.is_empty() {
            return;
        }

        let mut pending = active_run
            .pending_ready
            .lock()
            .expect("poisoned pending ready queue");
        pending.extend(self.ready_batch.drain(..));
        drop(pending);
        // Wake the parked loop thread so runtime-dispatched work (fd
        // watchers) is observed; `signal_ready` coalesces redundant wakes.
        self.core.signal_ready();
    }

    fn finish_run(&mut self) {
        let Some(active_run) = self.active_run.take() else {
            return;
        };

        if let Ok(mut pending) = active_run.pending_ready.lock() {
            self.ready_batch.extend(pending.drain(..));
        }

        {
            let mut state = self.core.state.lock().expect("poisoned loop state");
            state.running = false;
            state.stopping = false;
        }
    }

    fn cleanup_watchers(&mut self) {
        #[cfg(unix)]
        for (_, watcher) in self.signal_tasks.drain() {
            watcher.handle.close();
            let _ = watcher.join.join();
        }
        #[cfg(not(unix))]
        self.signal_tasks.clear();
        for (_, task) in self.reader_tasks.drain() {
            abort_watch_task(task);
        }
        for (_, task) in self.writer_tasks.drain() {
            abort_watch_task(task);
        }
        for (_, task) in self.accept_tasks.drain() {
            abort_watch_task(task);
        }
    }
}
