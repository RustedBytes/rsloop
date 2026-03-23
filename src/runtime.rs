use std::cmp::Ordering;
use std::collections::{BinaryHeap, HashMap, VecDeque};
#[cfg(target_os = "linux")]
use std::os::fd::{FromRawFd, OwnedFd};
#[cfg(not(target_os = "linux"))]
use std::sync::atomic::{AtomicBool, Ordering as AtomicOrdering};
use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use crate::fd_ops;
use crate::loop_core::{LoopCommand, LoopCore, ReadyItem};
#[cfg(windows)]
use crate::windows_vibeio;
#[cfg(target_os = "linux")]
use compio::net::PollFd;
#[cfg(target_os = "linux")]
use compio::runtime::{JoinHandle as CompioJoinHandle, Runtime as CompioRuntime};
#[cfg(not(target_os = "linux"))]
use crossbeam_channel::RecvTimeoutError;
use crossbeam_channel::{Receiver, TryRecvError};
use pyo3::prelude::*;
#[cfg(unix)]
use signal_hook::iterator::{Handle as SignalHandle, Signals};

struct RuntimeDispatcher {
    core: Arc<LoopCore>,
    command_rx: Receiver<LoopCommand>,
    #[cfg(target_os = "linux")]
    io_runtime: CompioRuntime,
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
}

#[cfg(unix)]
struct SignalWatcher {
    handle: SignalHandle,
    join: thread::JoinHandle<()>,
}

#[cfg(not(unix))]
struct SignalWatcher;

#[cfg(target_os = "linux")]
type WatchTask = CompioJoinHandle<()>;

#[cfg(all(not(target_os = "linux"), not(windows)))]
struct WatchTask {
    stop: Arc<AtomicBool>,
    join: thread::JoinHandle<()>,
}

#[cfg(all(not(target_os = "linux"), not(windows)))]
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

#[cfg(windows)]
enum WatchTask {
    Thread {
        stop: Arc<AtomicBool>,
        join: thread::JoinHandle<()>,
    },
    Vibeio(windows_vibeio::TaskHandle),
}

#[cfg(windows)]
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
            Self::Vibeio(task) => windows_vibeio::cancel(task),
        }
    }
}

#[cfg(target_os = "linux")]
fn abort_watch_task(task: WatchTask) {
    drop(task);
}

#[cfg(not(target_os = "linux"))]
fn abort_watch_task(task: WatchTask) {
    task.abort();
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
    profiling::scope!("runtime.run_thread");
    #[cfg(feature = "profiler")]
    if tracy_client::Client::is_running() {
        tracy_client::set_thread_name!("rsloop-runtime");
    }
    #[cfg(target_os = "linux")]
    let io_runtime = CompioRuntime::new().expect("failed to initialize compio runtime");
    #[cfg(target_os = "linux")]
    core.set_runtime_waker(Some(io_runtime.waker()));

    let mut dispatcher = RuntimeDispatcher {
        core: Arc::clone(&core),
        command_rx,
        #[cfg(target_os = "linux")]
        io_runtime,
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
    dispatcher.run();
    #[cfg(target_os = "linux")]
    core.set_runtime_waker(None);
}

impl RuntimeDispatcher {
    fn run(&mut self) {
        profiling::scope!("runtime.dispatcher.run");
        #[cfg(target_os = "linux")]
        {
            let runtime = self.io_runtime.clone();
            runtime.enter(|| self.run_inner());
            return;
        }

        #[cfg(not(target_os = "linux"))]
        self.run_inner();
    }

    fn run_inner(&mut self) {
        profiling::scope!("runtime.dispatcher.run_inner");
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
        profiling::scope!("runtime.wait_for_work");
        #[cfg(target_os = "linux")]
        {
            self.io_runtime.run();

            loop {
                match self.command_rx.try_recv() {
                    Ok(command) => {
                        if self.handle_received_command(command) {
                            return true;
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return true,
                }
            }

            self.io_runtime.poll_with(self.next_wait_timeout());
            self.io_runtime.run();
            return false;
        }

        #[cfg(not(target_os = "linux"))]
        {
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
    }

    fn handle_received_command(&mut self, command: LoopCommand) -> bool {
        profiling::scope!("runtime.handle_received_command");
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
            LoopCommand::ScheduleFutureSetResult { future, value } => {
                profiling::scope!("runtime.cmd.future_set_result");
                self.ready_batch
                    .push_back(ReadyItem::FutureSetResult { future, value });
            }
            LoopCommand::ScheduleFutureSetException { future, value } => {
                profiling::scope!("runtime.cmd.future_set_exception");
                self.ready_batch
                    .push_back(ReadyItem::FutureSetException { future, value });
            }
            LoopCommand::ScheduleStreamTransportRead(core) => {
                profiling::scope!("runtime.cmd.stream_transport_read");
                self.ready_batch
                    .push_back(ReadyItem::StreamTransportRead(core));
            }
            LoopCommand::ScheduleProcessTransport(core) => {
                profiling::scope!("runtime.cmd.process_transport");
                self.ready_batch
                    .push_back(ReadyItem::ProcessTransport(core));
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
            LoopCommand::EnterRun {
                pending_ready,
                wake_tx,
            } => {
                profiling::scope!("runtime.cmd.enter_run");
                self.active_run = Some(ActiveRun {
                    pending_ready,
                    wake_tx,
                });
                self.dispatch_ready_batch();
            }
            LoopCommand::FinishRun { done_tx } => {
                profiling::scope!("runtime.cmd.finish_run");
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
                    abort_watch_task(task);
                }

                #[cfg(target_os = "linux")]
                let task = {
                    let sender = Arc::clone(&self.core);
                    self.io_runtime.spawn(async move {
                        let Ok(poll_fd) = poll_fd_from_raw(fd) else {
                            return;
                        };
                        if poll_fd.read_ready().await.is_err() {
                            return;
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
                    })
                };

                #[cfg(windows)]
                let task = {
                    let sender = Arc::clone(&self.core);
                    let (service_callback, service_args, service_context) = Python::attach(|py| {
                        (
                            callback.clone_ref(py),
                            args.clone_ref(py),
                            context.clone_ref(py),
                        )
                    });
                    match fd_ops::duplicate_tcp_stream(fd).and_then(|stream| {
                        if stream.peer_addr().is_err() {
                            return Err(std::io::Error::new(
                                std::io::ErrorKind::InvalidInput,
                                "listener sockets stay on the blocking watcher path",
                            ));
                        }
                        windows_vibeio::spawn(move || async move {
                            let ready_result = async {
                                let stream = vibeio::net::PollTcpStream::from_std(stream)?;
                                let mut buf = [0_u8; 1];
                                stream.peek(&mut buf).await.map(|_| ())
                            }
                            .await;
                            if ready_result.is_err() {
                                return;
                            }

                            let ready = Python::attach(|py| {
                                Arc::new(crate::callbacks::ReadyCallback::new(
                                    py,
                                    sender.next_callback_id(),
                                    crate::callbacks::CallbackKind::Reader(fd),
                                    service_callback.clone_ref(py),
                                    service_args.clone_ref(py),
                                    service_context.clone_ref(py),
                                    context_needs_run,
                                ))
                            });

                            let _ = sender.send_command(LoopCommand::ScheduleReady(ready));
                        })
                    }) {
                        Ok(task) => WatchTask::Vibeio(task),
                        Err(_) => {
                            let sender = Arc::clone(&self.core);
                            WatchTask::spawn_thread(format!("rsloop-reader-{fd}"), move |stop| {
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
                            })
                        }
                    }
                };

                #[cfg(all(not(target_os = "linux"), not(windows)))]
                let task = {
                    let sender = Arc::clone(&self.core);
                    WatchTask::spawn(format!("rsloop-reader-{fd}"), move |stop| {
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
                    })
                };

                self.reader_tasks.insert(fd, task);
            }
            LoopCommand::StopReader(fd) => {
                if let Some(task) = self.reader_tasks.remove(&fd) {
                    abort_watch_task(task);
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
                    abort_watch_task(task);
                }

                #[cfg(target_os = "linux")]
                let task = {
                    let sender = Arc::clone(&self.core);
                    self.io_runtime.spawn(async move {
                        let Ok(poll_fd) = poll_fd_from_raw(fd) else {
                            return;
                        };
                        if poll_fd.write_ready().await.is_err() {
                            return;
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
                    })
                };

                #[cfg(windows)]
                let task = {
                    let sender = Arc::clone(&self.core);
                    WatchTask::spawn_thread(format!("rsloop-writer-{fd}"), move |stop| {
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
                    })
                };

                #[cfg(all(not(target_os = "linux"), not(windows)))]
                let task = {
                    let sender = Arc::clone(&self.core);
                    WatchTask::spawn(format!("rsloop-writer-{fd}"), move |stop| {
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
                    })
                };

                self.writer_tasks.insert(fd, task);
            }
            LoopCommand::StopWriter(fd) => {
                if let Some(task) = self.writer_tasks.remove(&fd) {
                    abort_watch_task(task);
                }
            }
            LoopCommand::StartSocketReader { fd, core, reader } => {
                if let Some(task) = self.reader_tasks.remove(&fd) {
                    abort_watch_task(task);
                }

                #[cfg(target_os = "linux")]
                let task = self
                    .io_runtime
                    .spawn(crate::stream_transport::run_socket_reader_task(
                        core, reader,
                    ));

                #[cfg(windows)]
                let task = match reader {
                    crate::stream_transport::ReaderTarget::Tcp(stream) => {
                        let service_core = core.clone();
                        match stream.try_clone().and_then(|dup| {
                            windows_vibeio::spawn(move || {
                                crate::stream_transport::run_tcp_socket_reader_task(
                                    service_core.clone(),
                                    dup,
                                )
                            })
                        }) {
                            Ok(task) => WatchTask::Vibeio(task),
                            Err(_) => WatchTask::spawn_thread(
                                format!("rsloop-socket-reader-{fd}"),
                                move |stop| {
                                    crate::stream_transport::run_socket_reader_blocking(
                                        core,
                                        crate::stream_transport::ReaderTarget::Tcp(stream),
                                        stop,
                                    )
                                },
                            ),
                        }
                    }
                    other => {
                        WatchTask::spawn_thread(format!("rsloop-socket-reader-{fd}"), move |stop| {
                            crate::stream_transport::run_socket_reader_blocking(core, other, stop)
                        })
                    }
                };

                #[cfg(all(not(target_os = "linux"), not(windows)))]
                let task = WatchTask::spawn(format!("rsloop-socket-reader-{fd}"), move |stop| {
                    crate::stream_transport::run_socket_reader_blocking(core, reader, stop)
                });

                self.reader_tasks.insert(fd, task);
            }
            LoopCommand::StopSocketReader(fd) => {
                if let Some(task) = self.reader_tasks.remove(&fd) {
                    abort_watch_task(task);
                }
            }
            LoopCommand::StartServerAccept {
                fd,
                server,
                listener,
            } => {
                if let Some(task) = self.accept_tasks.remove(&fd) {
                    abort_watch_task(task);
                }

                #[cfg(target_os = "linux")]
                let task = self
                    .io_runtime
                    .spawn(crate::stream_transport::run_server_accept_task(
                        server, listener,
                    ));

                #[cfg(windows)]
                let task = match listener {
                    crate::stream_transport::ServerListener::Tcp(listener) => {
                        let service_server = server.clone();
                        match listener.try_clone().and_then(|dup| {
                            windows_vibeio::spawn(move || {
                                crate::stream_transport::run_tcp_accept_task(
                                    service_server.clone(),
                                    dup,
                                )
                            })
                        }) {
                            Ok(task) => WatchTask::Vibeio(task),
                            Err(_) => WatchTask::spawn_thread(
                                format!("rsloop-accept-{fd}"),
                                move |stop| {
                                    crate::stream_transport::run_server_accept_blocking(
                                        server,
                                        crate::stream_transport::ServerListener::Tcp(listener),
                                        stop,
                                    )
                                },
                            ),
                        }
                    }
                    #[cfg(unix)]
                    other => WatchTask::spawn_thread(format!("rsloop-accept-{fd}"), move |stop| {
                        crate::stream_transport::run_server_accept_blocking(server, other, stop)
                    }),
                };

                #[cfg(all(not(target_os = "linux"), not(windows)))]
                let task = WatchTask::spawn(format!("rsloop-accept-{fd}"), move |stop| {
                    crate::stream_transport::run_server_accept_blocking(server, listener, stop)
                });

                self.accept_tasks.insert(fd, task);
            }
            LoopCommand::StopServerAccept(fd) => {
                if let Some(task) = self.accept_tasks.remove(&fd) {
                    abort_watch_task(task);
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

    fn dispatch_ready_batch(&mut self) -> bool {
        profiling::scope!("runtime.dispatch_ready_batch");
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

#[cfg(target_os = "linux")]
fn poll_fd_from_raw(fd: fd_ops::RawFd) -> std::io::Result<PollFd<OwnedFd>> {
    let dup = fd_ops::dup_raw_fd(fd)?;
    let dup: i32 = dup
        .try_into()
        .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "fd out of range"))?;
    PollFd::new(unsafe { OwnedFd::from_raw_fd(dup) })
}
