use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, RecvTimeoutError, TryRecvError};
use pyo3::prelude::*;
#[cfg(unix)]
use signal_hook::iterator::{Handle as SignalHandle, Signals};

use crate::fd_ops;
use crate::loop_core::{LoopCommand, LoopCore, ReadyItem};

struct RuntimeDispatcher {
    core: Arc<LoopCore>,
    command_rx: Receiver<LoopCommand>,
    ready_batch: VecDeque<ReadyItem>,
    timers: BinaryHeap<TimerEntry>,
    next_timer_id: u64,
    active_run: Option<ActiveRun>,
    signal_tasks: HashMap<i32, SignalWatcher>,
    reader_tasks: HashMap<fd_ops::RawFd, WatchTask>,
    writer_tasks: HashMap<fd_ops::RawFd, WatchTask>,
    shutting_down: bool,
}

struct ActiveRun {
    pending_ready: Arc<std::sync::Mutex<VecDeque<ReadyItem>>>,
    wake_tx: std::sync::mpsc::Sender<()>,
}

#[cfg(unix)]
struct SignalWatcher {
    handle: SignalHandle,
    join: thread::JoinHandle<()>,
}

#[cfg(not(unix))]
struct SignalWatcher;

struct WatchTask {
    stop: Arc<AtomicBool>,
    join: thread::JoinHandle<()>,
}

impl WatchTask {
    fn spawn(name: String, task: impl FnOnce(Arc<AtomicBool>) + Send + 'static) -> Self {
        let stop = Arc::new(AtomicBool::new(false));
        let thread_stop = Arc::clone(&stop);
        let join = thread::Builder::new()
            .name(name)
            .spawn(move || task(thread_stop))
            .expect("failed to spawn watch task");
        Self { stop, join }
    }

    fn abort(self) {
        self.stop.store(true, AtomicOrdering::Release);
        let _ = self.join.join();
    }
}

struct TimerEntry {
    when: Instant,
    seq: u64,
    callback: Arc<crate::callbacks::ReadyCallback>,
}

impl PartialEq for TimerEntry {
    fn eq(&self, other: &Self) -> bool {
        self.when == other.when && self.seq == other.seq
    }
}

impl Eq for TimerEntry {}

impl PartialOrd for TimerEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TimerEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .when
            .cmp(&self.when)
            .then_with(|| other.seq.cmp(&self.seq))
    }
}

pub fn run_runtime_thread(core: Arc<LoopCore>, command_rx: Receiver<LoopCommand>) {
    let mut dispatcher = RuntimeDispatcher {
        core: Arc::clone(&core),
        command_rx,
        ready_batch: VecDeque::new(),
        timers: BinaryHeap::new(),
        next_timer_id: 0,
        active_run: None,
        signal_tasks: HashMap::new(),
        reader_tasks: HashMap::new(),
        writer_tasks: HashMap::new(),
        shutting_down: false,
    };
    dispatcher.run();
}

impl RuntimeDispatcher {
    fn run(&mut self) {
        loop {
            if self.shutting_down {
                break;
            }

            self.collect_expired_timers();

            if self.active_run.is_some() && self.has_ready() {
                if self.dispatch_ready_batch() {
                    break;
                }
                continue;
            }

            if self.wait_for_work() {
                break;
            }
        }
    }

    #[inline]
    fn has_ready(&self) -> bool {
        !self.ready_batch.is_empty()
    }

    fn wait_for_work(&mut self) -> bool {
        if self.active_run.is_none() {
            return match self.command_rx.recv() {
                Ok(command) => self.handle_received_command(command),
                Err(_) => true,
            };
        }

        match self.next_wait_timeout() {
            Some(timeout) if timeout.is_zero() => false,
            Some(timeout) => match self.command_rx.recv_timeout(timeout) {
                Ok(command) => self.handle_received_command(command),
                Err(RecvTimeoutError::Timeout) => false,
                Err(RecvTimeoutError::Disconnected) => true,
            },
            None => match self.command_rx.recv() {
                Ok(command) => self.handle_received_command(command),
                Err(_) => true,
            },
        }
    }

    fn handle_received_command(&mut self, command: LoopCommand) -> bool {
        if self.handle_command(command) {
            return true;
        }

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

    #[inline]
    fn next_wait_timeout(&self) -> Option<Duration> {
        self.timers.peek().map(|entry| {
            let now = Instant::now();
            entry.when.saturating_duration_since(now)
        })
    }

    fn collect_expired_timers(&mut self) {
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
        match command {
            LoopCommand::ScheduleReady(callback) => {
                self.ready_batch.push_back(ReadyItem::Callback(callback));
            }
            LoopCommand::ScheduleFutureSetResult { future, value } => {
                self.ready_batch
                    .push_back(ReadyItem::FutureSetResult { future, value });
            }
            LoopCommand::ScheduleFutureSetException { future, value } => {
                self.ready_batch
                    .push_back(ReadyItem::FutureSetException { future, value });
            }
            LoopCommand::ScheduleStreamTransportRead(core) => {
                self.ready_batch
                    .push_back(ReadyItem::StreamTransportRead(core));
            }
            LoopCommand::ScheduleTimer { callback, when } => {
                let seq = self.next_timer_id;
                self.next_timer_id += 1;
                self.timers.push(TimerEntry {
                    when,
                    seq,
                    callback,
                });
            }
            LoopCommand::EnterRun {
                pending_ready,
                wake_tx,
            } => {
                self.active_run = Some(ActiveRun {
                    pending_ready,
                    wake_tx,
                });
                self.dispatch_ready_batch();
            }
            LoopCommand::FinishRun { done_tx } => {
                self.finish_run();
                let _ = done_tx.send(());
            }
            LoopCommand::StartSignalWatcher(sig) => {
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
                                let _ = sender.send_command(LoopCommand::SignalFired(delivered));
                            }
                        })
                        .expect("failed to spawn signal watcher thread");

                    self.signal_tasks
                        .insert(sig, SignalWatcher { handle, join });
                }
            }
            LoopCommand::StopSignalWatcher(sig) => {
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
            LoopCommand::SignalFired(sig) => {
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
            LoopCommand::StartReader {
                fd,
                callback,
                args,
                context,
                context_needs_run,
            } => {
                if let Some(task) = self.reader_tasks.remove(&fd) {
                    task.abort();
                }

                let sender = Arc::clone(&self.core);
                let task = WatchTask::spawn(format!("rsloop-reader-{fd}"), move |stop| {
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

                    let ready = Python::attach(|py| {
                        Arc::new(crate::callbacks::ReadyCallback::new(
                            py,
                            sender.next_callback_id(),
                            crate::callbacks::CallbackKind::Reader(fd),
                            callback.clone_ref(py),
                            args.clone_ref(py),
                            context.clone_ref(py),
                            context_needs_run,
                        ))
                    });

                    let _ = sender.send_command(LoopCommand::ScheduleReady(ready));
                });

                self.reader_tasks.insert(fd, task);
            }
            LoopCommand::StopReader(fd) => {
                if let Some(task) = self.reader_tasks.remove(&fd) {
                    task.abort();
                }
            }
            LoopCommand::StartWriter {
                fd,
                callback,
                args,
                context,
                context_needs_run,
            } => {
                if let Some(task) = self.writer_tasks.remove(&fd) {
                    task.abort();
                }

                let sender = Arc::clone(&self.core);
                let task = WatchTask::spawn(format!("rsloop-writer-{fd}"), move |stop| {
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

                    let ready = Python::attach(|py| {
                        Arc::new(crate::callbacks::ReadyCallback::new(
                            py,
                            sender.next_callback_id(),
                            crate::callbacks::CallbackKind::Writer(fd),
                            callback.clone_ref(py),
                            args.clone_ref(py),
                            context.clone_ref(py),
                            context_needs_run,
                        ))
                    });

                    let _ = sender.send_command(LoopCommand::ScheduleReady(ready));
                });

                self.writer_tasks.insert(fd, task);
            }
            LoopCommand::StopWriter(fd) => {
                if let Some(task) = self.writer_tasks.remove(&fd) {
                    task.abort();
                }
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

    fn dispatch_ready_batch(&mut self) -> bool {
        let Some(active_run) = self.active_run.as_ref() else {
            return false;
        };

        if self.ready_batch.is_empty() {
            return false;
        }

        let mut pending = active_run
            .pending_ready
            .lock()
            .expect("poisoned pending ready queue");
        pending.extend(self.ready_batch.drain(..));
        drop(pending);
        let _ = active_run.wake_tx.send(());
        false
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
            task.abort();
        }
        for (_, task) in self.writer_tasks.drain() {
            task.abort();
        }
    }
}
