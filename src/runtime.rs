use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::task::{Context, Poll};
use std::thread;
use std::time::Instant;

use crate::fd_ops;
use crate::loop_core::{
    LoopCommand, LoopCore, LoopFutureCommand, LoopIoCommand, LoopRunCommand, LoopSignalCommand,
    LoopTransportCommand, ReadyItem,
};
use crossbeam_channel::{Receiver, TryRecvError};
use pyo3::prelude::*;
#[cfg(unix)]
use signal_hook::iterator::{Handle as SignalHandle, Signals};

mod timer_entry;
use timer_entry::TimerEntry;

struct RuntimeDispatcher {
    core: Arc<LoopCore>,
    command_rx: Receiver<LoopCommand>,
    timer_wait: Option<(Instant, vibeio::time::Sleep)>,
    ready_batch: VecDeque<ReadyItem>,
    timers: BinaryHeap<TimerEntry>,
    next_timer_id: u64,
    active_run: Option<ActiveRun>,
    signal_tasks: HashMap<i32, SignalWatcher>,
    reader_tasks: HashMap<fd_ops::RawFd, WatchTask>,
    writer_tasks: HashMap<fd_ops::RawFd, WatchTask>,
    accept_tasks: HashMap<fd_ops::RawFd, WatchTask>,
    shutting_down: bool,
}

struct ActiveRun {
    pending_ready: Arc<std::sync::Mutex<VecDeque<ReadyItem>>>,
    wake_tx: std::sync::mpsc::Sender<()>,
    wake_pending: Arc<std::sync::atomic::AtomicBool>,
}

#[cfg(unix)]
struct SignalWatcher {
    handle: SignalHandle,
    join: thread::JoinHandle<()>,
}

#[cfg(not(unix))]
struct SignalWatcher;

enum WatchTask {
    Thread {
        stop: Arc<AtomicBool>,
        join: thread::JoinHandle<()>,
    },
    Vibeio(vibeio::JoinHandle<()>),
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
    profiling::scope!("runtime.run_thread");
    #[cfg(feature = "profiler")]
    if tracy_client::Client::is_running() {
        tracy_client::set_thread_name!("rsloop-runtime");
    }
    let runtime = vibeio::RuntimeBuilder::new()
        .enable_timer(true)
        .build()
        .expect("failed to initialize vibeio runtime");
    let dispatcher = RuntimeDispatcher {
        core: Arc::clone(&core),
        command_rx,
        timer_wait: None,
        ready_batch: VecDeque::new(),
        timers: BinaryHeap::new(),
        next_timer_id: 0,
        active_run: None,
        signal_tasks: HashMap::new(),
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
        profiling::scope!("runtime.dispatcher.poll");
        self.core.set_runtime_waker(Some(cx.waker().clone()));
        loop {
            if self.shutting_down {
                return Poll::Ready(());
            }

            self.collect_expired_timers();

            if self.active_run.is_some() && self.has_ready() {
                self.dispatch_ready_batch();
            }

            if self.drain_commands() {
                return Poll::Ready(());
            }
            self.collect_expired_timers();
            if self.active_run.is_some() && self.has_ready() {
                self.dispatch_ready_batch();
                continue;
            }

            if self.active_run.is_none() {
                self.timer_wait = None;
                return Poll::Pending;
            }

            let Some(deadline) = self.timers.peek().map(|entry| entry.when) else {
                self.timer_wait = None;
                return Poll::Pending;
            };
            let replace_timer = self
                .timer_wait
                .as_ref()
                .is_none_or(|(current, _)| *current != deadline);
            if replace_timer {
                self.timer_wait = Some((deadline, vibeio::time::Sleep::sleep_until(deadline)));
            }
            let (_, sleep) = self.timer_wait.as_mut().expect("timer wait missing");
            if Pin::new(sleep).poll(cx).is_ready() {
                self.timer_wait = None;
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

    fn collect_expired_timers(&mut self) {
        profiling::scope!("runtime.collect_expired_timers");
        if self.active_run.is_none() {
            return;
        }

        let now = Instant::now();
        while self.timers.peek().is_some_and(|entry| entry.when <= now) {
            let entry = self.timers.pop().expect("timer heap peeked but empty");
            if entry.callback.cancelled() {
                continue;
            }
            self.ready_batch
                .push_back(ReadyItem::Callback(entry.callback));
        }
    }

    fn handle_command(&mut self, command: LoopCommand) -> bool {
        profiling::scope!("runtime.handle_command");
        match command {
            LoopCommand::ScheduleReady(callback) => {
                profiling::scope!("runtime.cmd.schedule_ready");
                self.ready_batch.push_back(ReadyItem::Callback(callback));
            }
            LoopCommand::ScheduleReadyHandle(handle) => {
                profiling::scope!("runtime.cmd.schedule_ready_handle");
                self.ready_batch
                    .push_back(ReadyItem::HandleCallback(handle));
            }
            LoopCommand::Future(LoopFutureCommand::SetResult { future, value }) => {
                profiling::scope!("runtime.cmd.future_set_result");
                self.ready_batch
                    .push_back(ReadyItem::FutureSetResult { future, value });
            }
            LoopCommand::Future(LoopFutureCommand::SetException { future, value }) => {
                profiling::scope!("runtime.cmd.future_set_exception");
                self.ready_batch
                    .push_back(ReadyItem::FutureSetException { future, value });
            }
            LoopCommand::Transport(LoopTransportCommand::StreamRead(core)) => {
                profiling::scope!("runtime.cmd.stream_transport_read");
                self.ready_batch
                    .push_back(ReadyItem::StreamTransportRead(core));
            }
            LoopCommand::Transport(LoopTransportCommand::Process(core)) => {
                profiling::scope!("runtime.cmd.process_transport");
                self.ready_batch
                    .push_back(ReadyItem::ProcessTransport(core));
            }
            LoopCommand::Transport(LoopTransportCommand::ServerAccepted { server, stream }) => {
                profiling::scope!("runtime.cmd.server_accepted");
                self.ready_batch
                    .push_back(ReadyItem::ServerAccepted { server, stream });
            }
            LoopCommand::ScheduleTimer { callback, when } => {
                profiling::scope!("runtime.cmd.schedule_timer");
                let seq = self.next_timer_id;
                self.next_timer_id += 1;
                self.timers.push(TimerEntry {
                    when,
                    seq,
                    callback,
                });
            }
            LoopCommand::Run(LoopRunCommand::EnterRun {
                pending_ready,
                wake_tx,
                wake_pending,
            }) => {
                profiling::scope!("runtime.cmd.enter_run");
                self.active_run = Some(ActiveRun {
                    pending_ready,
                    wake_tx,
                    wake_pending,
                });
                self.dispatch_ready_batch();
            }
            LoopCommand::Run(LoopRunCommand::FinishRun { done_tx }) => {
                profiling::scope!("runtime.cmd.finish_run");
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

                    Ok(Some(Arc::new(crate::callbacks::ReadyCallback::new(
                        py,
                        self.core.next_callback_id(),
                        crate::callbacks::CallbackKind::Signal(sig),
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
                            Ok((false, false)) => continue,
                            Ok((false, true)) => continue,
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
                                Ok((false, false)) => continue,
                                Ok((true, _)) => continue,
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
                    crate::stream_transport::ReaderTarget::Tcp(stream) => {
                        WatchTask::Vibeio(vibeio::spawn(
                            crate::stream_transport::run_tcp_socket_reader_task(core, stream),
                        ))
                    }
                    #[cfg(unix)]
                    crate::stream_transport::ReaderTarget::Unix(stream) => {
                        WatchTask::Vibeio(vibeio::spawn(
                            crate::stream_transport::run_unix_socket_reader_task(core, stream),
                        ))
                    }
                    other @ crate::stream_transport::ReaderTarget::File(_) => {
                        WatchTask::spawn_thread(format!("rsloop-socket-reader-{fd}"), move |stop| {
                            crate::stream_transport::run_socket_reader_blocking(core, other, stop)
                        })
                    }
                };

                self.reader_tasks.insert(fd, task);
            }
            LoopCommand::Io(LoopIoCommand::StopSocketReader(fd)) => {
                if let Some(task) = self.reader_tasks.remove(&fd) {
                    cancel_watch_task(task);
                }
            }
            LoopCommand::Io(LoopIoCommand::StartServerAccept {
                fd,
                server,
                listener,
            }) => {
                if let Some(task) = self.accept_tasks.remove(&fd) {
                    cancel_watch_task(task);
                }

                let task = WatchTask::Vibeio(vibeio::spawn(
                    crate::stream_transport::run_server_accept_task(server, listener),
                ));

                self.accept_tasks.insert(fd, task);
            }
            LoopCommand::Io(LoopIoCommand::StopServerAccept(fd)) => {
                if let Some(task) = self.accept_tasks.remove(&fd) {
                    cancel_watch_task(task);
                }
            }
            LoopCommand::RequestStop => {
                profiling::scope!("runtime.cmd.request_stop");
                self.ready_batch.push_back(ReadyItem::Stop);
            }
            LoopCommand::Close => {
                profiling::scope!("runtime.cmd.close");
                self.finish_run();
                self.cleanup_watchers();
                self.shutting_down = true;
                return true;
            }
        }

        false
    }

    fn dispatch_ready_batch(&mut self) {
        profiling::scope!("runtime.dispatch_ready_batch");
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
        // Mirror the wake protocol used by try_enqueue_active_ready so the
        // loop thread's spin-wait observes runtime-dispatched work (timers,
        // fd watchers) too, and redundant wake tokens are not queued.
        if !active_run
            .wake_pending
            .swap(true, std::sync::atomic::Ordering::AcqRel)
        {
            let _ = active_run.wake_tx.send(());
        }
    }

    fn finish_run(&mut self) {
        profiling::scope!("runtime.finish_run");
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
        profiling::scope!("runtime.cleanup_watchers");
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
