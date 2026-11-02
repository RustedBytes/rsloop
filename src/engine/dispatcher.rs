//! Coordination-thread dispatcher for commands and compatibility watchers.
//!
//! It prepares `ReadyItem`s but leaves Python callback execution to the loop
//! thread. Direct stream I/O can instead run on the loop thread's own reactor.

#[cfg(not(unix))]
use std::collections::HashSet;
use std::{
    collections::{HashMap, VecDeque},
    future::Future,
    pin::Pin,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering as AtomicOrdering},
    },
    task::{Context, Poll},
    thread,
};

use crossbeam_channel::{Receiver, TryRecvError};
use pyo3::prelude::*;
#[cfg(unix)]
use signal_hook::iterator::{Handle as SignalHandle, Signals};

#[cfg(unix)]
use super::commands::ConnectCompletion;
use super::{
    commands::{
        AcceptedConnection, FutureCompletion, LoopCommand, LoopFutureCommand, LoopIoCommand,
        LoopRunCommand, LoopSignalCommand, LoopTransportCommand, PendingReady, ReadyItem,
    },
    loop_core::LoopCore,
};
use crate::fd_ops;

/// One readiness wait on the native reactor. A duplicate protects cancellation
/// from descriptor reuse while user code closes or replaces its registration.
#[cfg_attr(feature = "profile", hotpath::measure(future = true))]
pub(super) async fn wait_fd_ready(fd: fd_ops::RawFd, writable: bool) -> std::io::Result<()> {
    let (read, write) = fd_ops::poll_fd(fd, !writable, writable, 0)?;
    if if writable { write } else { read } {
        return Ok(());
    }
    #[cfg(unix)]
    {
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
        let duplicate = fd_ops::dup_raw_fd(fd)?;
        // SAFETY: dup_raw_fd transfers a fresh owned Unix descriptor.
        let owned = unsafe { OwnedFd::from_raw_fd(duplicate as _) };
        let interest = if writable {
            mio::Interest::WRITABLE
        } else {
            mio::Interest::READABLE
        };
        let handle = crate::vibeio::InnerRawHandle::new_with_mode(
            owned.as_raw_fd(),
            interest,
            crate::vibeio::RegistrationMode::Poll,
        )?;
        let mut armed = false;
        std::future::poll_fn(|cx| {
            if armed {
                return Poll::Ready(Ok(()));
            }
            let mut op = if writable {
                crate::vibeio::ReadinessOp::new_writable(&handle)
            } else {
                crate::vibeio::ReadinessOp::new_readable(&handle)
            };
            match handle.poll_op_poll(cx, &mut op) {
                Poll::Pending => {
                    armed = true;
                    Poll::Pending
                }
                result => result,
            }
        })
        .await
    }
    #[cfg(windows)]
    {
        // Winsock descriptors use native AFD readiness. CRT pipe/file handles
        // retain the compatibility polling semantics with a timer, not a new
        // worker thread for every event.
        let interest = if writable {
            mio::Interest::WRITABLE
        } else {
            mio::Interest::READABLE
        };
        if let Ok(handle) = crate::vibeio::InnerRawHandle::new_with_mode(
            crate::vibeio::RawOsHandle::Socket(fd as _),
            interest,
            crate::vibeio::RegistrationMode::Poll,
        ) {
            let mut armed = false;
            let result = std::future::poll_fn(|cx| {
                if armed {
                    return Poll::Ready(Ok(()));
                }
                let mut op = if writable {
                    crate::vibeio::ReadinessOp::new_writable(&handle)
                } else {
                    crate::vibeio::ReadinessOp::new_readable(&handle)
                };
                match handle.poll_op_poll(cx, &mut op) {
                    Poll::Pending => {
                        armed = true;
                        Poll::Pending
                    }
                    result => result,
                }
            })
            .await;
            if result.is_ok() {
                return result;
            }
        }
        loop {
            let (read, write) = fd_ops::poll_fd(fd, !writable, writable, 0)?;
            if if writable { write } else { read } {
                return Ok(());
            }
            crate::vibeio::time::Sleep::new(std::time::Duration::from_millis(10)).await;
        }
    }
}

/// Long-lived future driven by the coordination thread's `vibeio` runtime.
struct RuntimeDispatcher {
    core: Arc<LoopCore>,
    command_rx: Receiver<LoopCommand>,
    ready_batch: VecDeque<ReadyItem>,
    /// Channel handles in `ready_batch`, released from
    /// `LoopCore::channel_handles` once they reach an active run.
    channel_handles: usize,
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
    pending_ready: Arc<std::sync::Mutex<PendingReady>>,
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
    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "WatchTask"))]
    fn spawn_thread(name: String, task: impl FnOnce(Arc<AtomicBool>) + Send + 'static) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let join = thread::Builder::new()
            .name(name)
            .spawn(move || task(thread_stop))
            .expect("failed to spawn watch task");
        Self::Thread { stop, join }
    }

    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "WatchTask"))]
    fn abort(self) {
        match self {
            Self::Thread { stop, join } => {
                stop.store(true, AtomicOrdering::Release);
                let _ = join.join();
            }
            Self::Vibeio(task) => task.cancel(),
        }
    }

    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "WatchTask"))]
    fn cancel(self) {
        match self {
            Self::Thread { stop, .. } => {
                stop.store(true, AtomicOrdering::Release);
            }
            Self::Vibeio(task) => task.cancel(),
        }
    }
}

#[cfg_attr(feature = "profile", hotpath::measure)]
#[inline]
fn abort_watch_task(task: WatchTask) {
    task.abort();
}

#[cfg_attr(feature = "profile", hotpath::measure)]
#[inline]
fn cancel_watch_task(task: WatchTask) {
    task.cancel();
}

#[cfg_attr(feature = "profile", hotpath::measure)]
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
        channel_handles: 0,
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

    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "<RuntimeDispatcher as Future>")
    )]
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
    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "RuntimeDispatcher"))]
    #[inline]
    fn has_ready(&self) -> bool {
        !self.ready_batch.is_empty()
    }

    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "RuntimeDispatcher"))]
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

    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "RuntimeDispatcher"))]
    fn handle_command(&mut self, command: LoopCommand) -> bool {
        match command {
            LoopCommand::ScheduleReady(callback) => {
                self.ready_batch.push_back(ReadyItem::Callback(callback));
            }
            LoopCommand::ScheduleReadyHandle(handle) => {
                self.channel_handles += 1;
                self.ready_batch
                    .push_back(ReadyItem::HandleCallback(handle));
            }
            LoopCommand::Future(LoopFutureCommand::SetResult { future, value }) => {
                self.ready_batch
                    .push_back(ReadyItem::FutureSetResult(Box::new(FutureCompletion {
                        future,
                        value,
                    })));
            }
            LoopCommand::Future(LoopFutureCommand::SetException { future, value }) => {
                self.ready_batch
                    .push_back(ReadyItem::FutureSetException(Box::new(FutureCompletion {
                        future,
                        value,
                    })));
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
                    .push_back(ReadyItem::ServerAccepted(Box::new(AcceptedConnection {
                        server,
                        stream,
                    })));
            }
            LoopCommand::Run(LoopRunCommand::EnterRun {
                pending_ready,
                local_mark,
            }) => {
                self.active_run = Some(ActiveRun {
                    pending_ready: Arc::clone(&pending_ready),
                });
                self.forward_ready_batch(local_mark);
                self.core.activate_ready_dispatch(&pending_ready);
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
                let task = WatchTask::Vibeio(crate::vibeio::spawn(async move {
                    if wait_fd_ready(fd, false).await.is_ok() && !callback.cancelled() {
                        let _ = sender.send_command(LoopCommand::ScheduleReady(callback));
                    }
                }));

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
                let task = WatchTask::Vibeio(crate::vibeio::spawn(async move {
                    if wait_fd_ready(fd, true).await.is_ok() && !callback.cancelled() {
                        let _ = sender.send_command(LoopCommand::ScheduleReady(callback));
                    }
                }));

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
                task_guard,
            }) => {
                if let Some(task) = self.accept_tasks.remove(&fd) {
                    cancel_watch_task(task);
                }

                let task = WatchTask::Vibeio(crate::vibeio::spawn(
                    crate::transport::stream::run_server_accept_task(server, listener, task_guard),
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
                    crate::transport::stream::run_connect_watch_task(core, fd, future, None, None),
                ));
            }
            #[cfg(unix)]
            LoopCommand::ConnectCompleted {
                future,
                fd,
                wait_errno,
            } => {
                self.ready_batch
                    .push_back(ReadyItem::ConnectCompleted(Box::new(ConnectCompletion {
                        future,
                        fd,
                        wait_errno,
                    })));
            }
            #[cfg(unix)]
            LoopCommand::LoopWork(work) => {
                self.ready_batch.push_back(ReadyItem::LoopWork(work));
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

    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "RuntimeDispatcher"))]
    fn dispatch_ready_batch(&mut self) {
        // Commands received mid-run are merged behind every loop-thread
        // enqueue so far; their producers called in before forwarding, so this
        // only errs towards running them later, as a busy queue would.
        self.forward_ready_batch(self.core.local_enqueue_mark());
    }

    /// Moves received commands into the active run, scheduled after
    /// `local_mark` loop-thread enqueues.
    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "RuntimeDispatcher"))]
    fn forward_ready_batch(&mut self, local_mark: u64) {
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
        pending.extend(self.ready_batch.drain(..), local_mark);
        drop(pending);
        self.core
            .channel_handles_forwarded(std::mem::take(&mut self.channel_handles));
        // Wake the parked loop thread so runtime-dispatched work (fd
        // watchers) is observed; `signal_ready` coalesces redundant wakes.
        self.core.signal_ready();
    }

    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "RuntimeDispatcher"))]
    fn finish_run(&mut self) {
        let Some(active_run) = self.active_run.take() else {
            return;
        };

        // The run's queue holds older work (its unexecuted callbacks, then
        // direct off-thread handles) than commands received since the last
        // dispatch, so it goes first.
        if let Ok(mut pending) = active_run.pending_ready.lock() {
            let mut queued = pending.take_items();
            queued.append(&mut self.ready_batch);
            self.ready_batch = queued;
        }

        {
            let mut state = self.core.state.lock().expect("poisoned loop state");
            state.running = false;
            state.stopping = false;
        }
    }

    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "RuntimeDispatcher"))]
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
