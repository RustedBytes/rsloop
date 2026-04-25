use std::collections::{HashMap, HashSet, VecDeque};
use std::io::Read;
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::process::ExitStatusExt;
#[cfg(windows)]
use std::os::windows::io::AsRawHandle;
use std::process::{Child, ChildStdin};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, Sender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use pyo3::exceptions::{PyProcessLookupError, PyRuntimeError};
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyTuple};

use crate::async_event::AsyncEvent;
use crate::context::{ensure_running_loop, run_in_context};
use crate::fd_ops;
use crate::loop_core::{LoopCommand, LoopCore};
use crate::stream_transport::spawn_write_pipe_transport;

enum ProcessCommand {
    Close,
    SendSignal(i32),
    Terminate,
    Kill,
}

enum PendingProcessEvent {
    PipeDataReceived { fd: i32, data: Box<[u8]> },
    PipeConnectionLost { fd: i32, exc: Option<String> },
    ProcessExited { returncode: i32 },
    ConnectionLost { exc: Option<String> },
}

const PROCESS_READER_BUFFER_SIZE: usize = 65_536;
const PROCESS_WAIT_POLL_INTERVAL: Duration = Duration::from_millis(20);

struct ProcessState {
    protocol: Py<PyAny>,
    context: Py<PyAny>,
    context_needs_run: bool,
    pid: u32,
    returncode: Option<i32>,
    closing: bool,
    exited: bool,
    connection_lost_called: bool,
    open_pipes: HashSet<i32>,
    pipe_transports: HashMap<i32, Py<PyAny>>,
}

pub struct ProcessTransportCore {
    loop_core: Arc<LoopCore>,
    loop_obj: Py<PyAny>,
    state: Mutex<ProcessState>,
    text_config: Option<ProcessTextConfig>,
    control_tx: Sender<ProcessCommand>,
    exit_notify: AsyncEvent,
    pending_events: Mutex<VecDeque<PendingProcessEvent>>,
    events_scheduled: AtomicBool,
}

#[pyclass(name = "ProcessTransport", module = "rsloop._loop")]
pub struct PyProcessTransport {
    pub core: Arc<ProcessTransportCore>,
}

struct ProcessPipeTransportCore {
    fd: i32,
    closing: AtomicBool,
}

#[pyclass(name = "ProcessPipeTransport", module = "rsloop._loop")]
pub struct PyProcessPipeTransport {
    core: Arc<ProcessPipeTransportCore>,
}

#[pyclass(module = "rsloop._loop")]
struct PyProcessStdinProtocol {
    core: Arc<ProcessTransportCore>,
}

#[derive(Clone)]
pub struct ProcessTextConfig {
    pub encoding: String,
    pub errors: String,
    pub translate_newlines: bool,
}

pub type BoxedProcessReader = Box<dyn Read + Send + 'static>;

pub struct ProcessTransportParams {
    pub loop_core: Arc<LoopCore>,
    pub loop_obj: Py<PyAny>,
    pub protocol: Py<PyAny>,
    pub context: Py<PyAny>,
    pub context_needs_run: bool,
    pub text_config: Option<ProcessTextConfig>,
    pub child: Child,
    pub stdout_override: Option<BoxedProcessReader>,
    pub stderr_override: Option<BoxedProcessReader>,
}

struct ProcessPipes {
    stdin: Option<ChildStdin>,
    stdout: Option<BoxedProcessReader>,
    stderr: Option<BoxedProcessReader>,
}

impl ProcessPipes {
    fn take_from(
        child: &mut Child,
        stdout_override: Option<BoxedProcessReader>,
        stderr_override: Option<BoxedProcessReader>,
    ) -> Self {
        Self {
            stdin: child.stdin.take(),
            stdout: stdout_override.or_else(|| {
                child
                    .stdout
                    .take()
                    .map(|value| Box::new(value) as BoxedProcessReader)
            }),
            stderr: stderr_override.or_else(|| {
                child
                    .stderr
                    .take()
                    .map(|value| Box::new(value) as BoxedProcessReader)
            }),
        }
    }

    fn open_pipes(&self) -> HashSet<i32> {
        let mut open_pipes = HashSet::with_capacity(3);
        if self.stdin.is_some() {
            open_pipes.insert(0);
        }
        if self.stdout.is_some() {
            open_pipes.insert(1);
        }
        if self.stderr.is_some() {
            open_pipes.insert(2);
        }
        open_pipes
    }
}

impl ProcessTransportCore {
    fn enqueue_pending_event(self: &Arc<Self>, event: PendingProcessEvent) {
        profiling::scope!("ProcessTransportCore::enqueue_pending_event");
        self.pending_events
            .lock()
            .expect("poisoned process pending queue")
            .push_back(event);

        if !self.events_scheduled.swap(true, Ordering::AcqRel)
            && self
                .loop_core
                .send_command(LoopCommand::ScheduleProcessTransport(Arc::clone(self)))
                .is_err()
        {
            self.events_scheduled.store(false, Ordering::Release);
        }
    }

    pub(crate) fn drain_pending_events_with_py(self: &Arc<Self>, py: Python<'_>) -> PyResult<()> {
        profiling::scope!("ProcessTransportCore::drain_pending_events_with_py");
        let mut drained = VecDeque::new();
        loop {
            {
                let mut queue = self
                    .pending_events
                    .lock()
                    .expect("poisoned process pending queue");
                if queue.is_empty() {
                    self.events_scheduled.store(false, Ordering::Release);
                    return Ok(());
                }

                std::mem::swap(&mut drained, &mut *queue);
            }

            while let Some(event) = drained.pop_front() {
                match event {
                    PendingProcessEvent::PipeDataReceived { fd, data } => {
                        profiling::scope!("process.pending.pipe_data_received");
                        if let Err(err) = self.pipe_data_received_with_py(py, fd, &data) {
                            self.report_error(err, "subprocess pipe_data_received failed");
                            let _ = self.connection_lost_with_py(py, None);
                            self.events_scheduled.store(false, Ordering::Release);
                            return Ok(());
                        }
                    }
                    PendingProcessEvent::PipeConnectionLost { fd, exc } => {
                        profiling::scope!("process.pending.pipe_connection_lost");
                        if let Err(err) = self.pipe_connection_lost_value_with_py(
                            py,
                            fd,
                            exc.map(PyRuntimeError::new_err),
                        ) {
                            self.report_error(err, "subprocess pipe_connection_lost failed");
                            let _ = self.connection_lost_with_py(py, None);
                            self.events_scheduled.store(false, Ordering::Release);
                            return Ok(());
                        }
                    }
                    PendingProcessEvent::ProcessExited { returncode } => {
                        profiling::scope!("process.pending.process_exited");
                        if let Err(err) = self.process_exited_with_py(py, returncode) {
                            self.report_error(err, "subprocess process_exited failed");
                            let _ = self.connection_lost_with_py(py, None);
                            self.events_scheduled.store(false, Ordering::Release);
                            return Ok(());
                        }
                    }
                    PendingProcessEvent::ConnectionLost { exc } => {
                        profiling::scope!("process.pending.connection_lost");
                        let _ = self.connection_lost_with_py(py, exc.map(PyRuntimeError::new_err));
                        self.events_scheduled.store(false, Ordering::Release);
                        return Ok(());
                    }
                }
            }
        }
    }

    fn call_protocol_with_tuple(
        &self,
        py: Python<'_>,
        method: &str,
        args: &Bound<'_, PyTuple>,
    ) -> PyResult<Py<PyAny>> {
        let (protocol, context, context_needs_run) = {
            let state = self.state.lock().expect("poisoned process state");
            (
                state.protocol.clone_ref(py),
                state.context.clone_ref(py),
                state.context_needs_run,
            )
        };
        let callback = protocol.bind(py).getattr(method)?.unbind();
        let tuple = args.clone().unbind();
        run_in_context(py, &context, context_needs_run, &callback, &tuple)
    }

    fn call_in_loop_context<T>(
        &self,
        f: impl for<'py> FnOnce(Python<'py>) -> PyResult<T>,
    ) -> PyResult<T> {
        Python::attach(|py| {
            ensure_running_loop(py, &self.loop_obj)?;
            f(py)
        })
    }

    fn call_protocol_method0(&self, py: Python<'_>, method: &str) -> PyResult<Py<PyAny>> {
        let args = PyTuple::empty(py);
        self.call_protocol_with_tuple(py, method, &args)
    }

    fn call_protocol_method1(
        &self,
        py: Python<'_>,
        method: &str,
        arg: Py<PyAny>,
    ) -> PyResult<Py<PyAny>> {
        let args = PyTuple::new(py, [arg])?;
        self.call_protocol_with_tuple(py, method, &args)
    }

    fn call_protocol_method2(
        &self,
        py: Python<'_>,
        method: &str,
        arg0: Py<PyAny>,
        arg1: Py<PyAny>,
    ) -> PyResult<Py<PyAny>> {
        let args = PyTuple::new(py, [arg0, arg1])?;
        self.call_protocol_with_tuple(py, method, &args)
    }

    fn report_error(&self, err: PyErr, message: &str) {
        let _ = Python::attach(|py| -> PyResult<()> {
            let context = PyDict::new(py);
            context.set_item("message", message)?;
            context.set_item("exception", err.value(py))?;
            self.loop_core.call_exception_handler(
                py,
                Some(&self.loop_obj),
                context.unbind().into_any(),
            )
        });
    }

    fn connection_made(&self, transport: Py<PyProcessTransport>) -> PyResult<()> {
        self.call_in_loop_context(|py| {
            self.call_protocol_method1(py, "connection_made", transport.into_any())?;
            Ok(())
        })
    }

    fn pipe_data_received_with_py(&self, py: Python<'_>, fd: i32, data: &[u8]) -> PyResult<()> {
        let payload = if let Some(text_config) = &self.text_config {
            let decoded = pyo3::types::PyBytes::new(py, data)
                .call_method1("decode", (&text_config.encoding, &text_config.errors))?;
            if text_config.translate_newlines {
                decoded
                    .call_method1("replace", ("\r\n", "\n"))?
                    .call_method1("replace", ("\r", "\n"))?
                    .unbind()
                    .into_any()
            } else {
                decoded.unbind()
            }
        } else {
            pyo3::types::PyBytes::new(py, data).unbind().into_any()
        };
        self.call_protocol_method2(
            py,
            "pipe_data_received",
            fd.into_pyobject(py)?.unbind().into_any(),
            payload,
        )?;
        Ok(())
    }

    fn pipe_data_received(self: &Arc<Self>, fd: i32, data: &[u8]) -> PyResult<()> {
        if !self.loop_core.on_runtime_thread() {
            self.enqueue_pending_event(PendingProcessEvent::PipeDataReceived {
                fd,
                data: Box::<[u8]>::from(data),
            });
            return Ok(());
        }

        self.call_in_loop_context(|py| self.pipe_data_received_with_py(py, fd, data))
    }

    fn pipe_connection_lost_value_with_py(
        &self,
        py: Python<'_>,
        fd: i32,
        exc: Option<PyErr>,
    ) -> PyResult<()> {
        let exc = exc.map(|err| err.value(py).clone().unbind().into_any());
        self.call_protocol_method2(
            py,
            "pipe_connection_lost",
            fd.into_pyobject(py)?.unbind().into_any(),
            exc.unwrap_or_else(|| py.None()),
        )?;
        Ok(())
    }

    fn pipe_connection_lost_message(
        self: &Arc<Self>,
        fd: i32,
        exc: Option<String>,
    ) -> PyResult<()> {
        let maybe_finish = {
            let mut state = self.state.lock().expect("poisoned process state");
            if !state.open_pipes.remove(&fd) {
                return Ok(());
            }
            let exited = state.exited;
            let empty = state.open_pipes.is_empty();
            (exc, exited && empty)
        };

        if !self.loop_core.on_runtime_thread() {
            self.enqueue_pending_event(PendingProcessEvent::PipeConnectionLost {
                fd,
                exc: maybe_finish.0,
            });
            if maybe_finish.1 {
                self.enqueue_pending_event(PendingProcessEvent::ConnectionLost { exc: None });
            }
            return Ok(());
        }

        if let Err(err) = self.call_in_loop_context(|py| {
            self.pipe_connection_lost_value_with_py(
                py,
                fd,
                maybe_finish.0.clone().map(PyRuntimeError::new_err),
            )
        }) {
            self.report_error(err, "subprocess pipe_connection_lost failed");
            return Err(PyRuntimeError::new_err(
                "subprocess pipe_connection_lost failed",
            ));
        }

        if maybe_finish.1 {
            self.connection_lost_message(None)?;
        }
        Ok(())
    }

    fn pipe_connection_lost(self: &Arc<Self>, fd: i32, exc: Option<PyErr>) -> PyResult<()> {
        let exc = exc.map(|err| Python::attach(|py| err.value(py).to_string()));
        self.pipe_connection_lost_message(fd, exc)
    }

    fn process_exited_with_py(&self, py: Python<'_>, returncode: i32) -> PyResult<()> {
        let _ = returncode;
        self.call_protocol_method0(py, "process_exited")?;
        Ok(())
    }

    fn process_exited(self: &Arc<Self>, returncode: i32) -> PyResult<()> {
        let should_finish = {
            let mut state = self.state.lock().expect("poisoned process state");
            state.returncode = Some(returncode);
            state.exited = true;
            state.open_pipes.is_empty()
        };
        self.exit_notify.notify_all();

        if !self.loop_core.on_runtime_thread() {
            self.enqueue_pending_event(PendingProcessEvent::ProcessExited { returncode });
            if should_finish {
                self.enqueue_pending_event(PendingProcessEvent::ConnectionLost { exc: None });
            }
            return Ok(());
        }

        self.call_in_loop_context(|py| self.process_exited_with_py(py, returncode))?;

        if should_finish {
            self.connection_lost_message(None)?;
        }
        Ok(())
    }

    fn connection_lost_with_py(&self, py: Python<'_>, exc: Option<PyErr>) -> PyResult<()> {
        let arg = exc
            .map(|err| err.value(py).clone().unbind().into_any())
            .unwrap_or_else(|| py.None());
        self.call_protocol_method1(py, "connection_lost", arg)?;
        Ok(())
    }

    fn connection_lost_message(self: &Arc<Self>, exc: Option<String>) -> PyResult<()> {
        {
            let mut state = self.state.lock().expect("poisoned process state");
            if state.connection_lost_called {
                return Ok(());
            }
            state.connection_lost_called = true;
            state.closing = true;
        }

        if !self.loop_core.on_runtime_thread() {
            self.enqueue_pending_event(PendingProcessEvent::ConnectionLost { exc });
            return Ok(());
        }

        self.call_in_loop_context(|py| {
            self.connection_lost_with_py(py, exc.clone().map(PyRuntimeError::new_err))
        })
    }

    fn connection_lost(self: &Arc<Self>, exc: Option<PyErr>) -> PyResult<()> {
        let exc = exc.map(|err| Python::attach(|py| err.value(py).to_string()));
        self.connection_lost_message(exc)
    }

    #[inline]
    fn get_returncode(&self) -> Option<i32> {
        self.state
            .lock()
            .expect("poisoned process state")
            .returncode
    }

    #[inline]
    fn is_closing(&self) -> bool {
        self.state.lock().expect("poisoned process state").closing
    }

    fn pipe_transport(&self, py: Python<'_>, fd: i32) -> Option<Py<PyAny>> {
        self.state
            .lock()
            .expect("poisoned process state")
            .pipe_transports
            .get(&fd)
            .map(|transport| transport.clone_ref(py))
    }

    fn has_open_pipe(&self, fd: i32) -> bool {
        self.state
            .lock()
            .expect("poisoned process state")
            .open_pipes
            .contains(&fd)
    }

    fn register_pipe_transports(&self, transports: Vec<(i32, Py<PyAny>)>) {
        if transports.is_empty() {
            return;
        }

        let mut state = self.state.lock().expect("poisoned process state");
        state.pipe_transports.extend(transports);
    }
}

#[pymethods]
impl PyProcessTransport {
    fn get_pid(&self) -> u32 {
        self.core.state.lock().expect("poisoned process state").pid
    }

    #[inline]
    fn get_returncode(&self) -> Option<i32> {
        self.core.get_returncode()
    }

    fn is_closing(&self) -> bool {
        self.core.is_closing()
    }

    fn get_pipe_transport(&self, py: Python<'_>, fd: i32) -> Option<Py<PyAny>> {
        self.core.pipe_transport(py, fd)
    }

    fn send_signal(&self, sig: i32) -> PyResult<()> {
        if self.core.get_returncode().is_some() {
            return Err(PyProcessLookupError::new_err("process is not running"));
        }
        self.core
            .control_tx
            .send(ProcessCommand::SendSignal(sig))
            .map_err(|_| PyProcessLookupError::new_err("process is not running"))
    }

    fn terminate(&self) -> PyResult<()> {
        if self.core.get_returncode().is_some() {
            return Err(PyProcessLookupError::new_err("process is not running"));
        }
        self.core
            .control_tx
            .send(ProcessCommand::Terminate)
            .map_err(|_| PyProcessLookupError::new_err("process is not running"))
    }

    fn kill(&self) -> PyResult<()> {
        if self.core.get_returncode().is_some() {
            return Err(PyProcessLookupError::new_err("process is not running"));
        }
        self.core
            .control_tx
            .send(ProcessCommand::Kill)
            .map_err(|_| PyProcessLookupError::new_err("process is not running"))
    }

    fn close(&self, py: Python<'_>) -> PyResult<()> {
        {
            let mut state = self.core.state.lock().expect("poisoned process state");
            state.closing = true;
        }
        if let Some(stdin) = self.core.pipe_transport(py, 0) {
            let _ = stdin.call_method0(py, "close");
        }
        let _ = self.core.control_tx.send(ProcessCommand::Close);
        Ok(())
    }

    fn __repr__(&self) -> String {
        format!(
            "<ProcessTransport pid={} returncode={:?} closing={}>",
            self.get_pid(),
            self.get_returncode(),
            self.is_closing()
        )
    }

    fn _wait<'py>(&self, py: Python<'py>) -> PyResult<Bound<'py, PyAny>> {
        let locals = crate::stream_transport::task_locals_for_loop(py, &self.core.loop_obj)?;
        let core = self.core.clone();
        pyo3_async_runtimes::async_std::future_into_py_with_locals(py, locals, async move {
            loop {
                if let Some(returncode) = core.get_returncode() {
                    return Python::attach(|py| -> PyResult<Py<PyAny>> {
                        Ok(returncode.into_pyobject(py)?.unbind().into_any())
                    });
                }
                let wait = core.exit_notify.listen();
                if let Some(returncode) = core.get_returncode() {
                    return Python::attach(|py| -> PyResult<Py<PyAny>> {
                        Ok(returncode.into_pyobject(py)?.unbind().into_any())
                    });
                }
                let _ = wait.await;
            }
        })
    }
}

#[pymethods]
impl PyProcessPipeTransport {
    fn close(&self) {
        self.core.closing.store(true, Ordering::SeqCst);
    }

    fn is_closing(&self) -> bool {
        self.core.closing.load(Ordering::SeqCst)
    }

    fn get_extra_info(&self, py: Python<'_>, _name: &str, default: Option<Py<PyAny>>) -> Py<PyAny> {
        default.unwrap_or_else(|| py.None())
    }

    fn pause_reading(&self) {}

    fn resume_reading(&self) {}

    fn __repr__(&self) -> String {
        format!(
            "<ProcessPipeTransport fd={} closing={}>",
            self.core.fd,
            self.is_closing()
        )
    }
}

#[pymethods]
impl PyProcessStdinProtocol {
    fn connection_made(&self, _transport: Py<PyAny>) {}

    fn pause_writing(&self) {}

    fn resume_writing(&self) {}

    #[pyo3(signature=(_exc=None))]
    fn connection_lost(&self, _exc: Option<Py<PyAny>>) -> PyResult<()> {
        if !self.core.has_open_pipe(0) {
            return Ok(());
        }
        self.core.pipe_connection_lost_message(0, None)
    }
}

fn new_process_pipe_transport(py: Python<'_>, fd: i32) -> PyResult<Py<PyAny>> {
    Ok(Py::new(
        py,
        PyProcessPipeTransport {
            core: Arc::new(ProcessPipeTransportCore {
                fd,
                closing: AtomicBool::new(false),
            }),
        },
    )?
    .into_any())
}

fn process_text_extra_entries(
    py: Python<'_>,
    text_config: Option<&ProcessTextConfig>,
) -> Option<HashMap<String, Py<PyAny>>> {
    text_config.map(|text_config| {
        let mut extra = HashMap::with_capacity(2);
        extra.insert(
            "text_encoding".to_owned(),
            pyo3::types::PyString::new(py, &text_config.encoding)
                .unbind()
                .into_any(),
        );
        extra.insert(
            "text_errors".to_owned(),
            pyo3::types::PyString::new(py, &text_config.errors)
                .unbind()
                .into_any(),
        );
        extra
    })
}

fn spawn_stdin_pipe_transport(
    py: Python<'_>,
    core: &Arc<ProcessTransportCore>,
    stdin: ChildStdin,
    extra_entries: Option<HashMap<String, Py<PyAny>>>,
) -> PyResult<Py<PyAny>> {
    #[cfg(unix)]
    let file_obj: Py<PyAny> = make_python_pipe_file(py, stdin.as_raw_fd() as i64, "wb")?;
    #[cfg(windows)]
    let file_obj: Py<PyAny> = make_python_pipe_file_from_handle(py, stdin.as_raw_handle(), "wb")?;
    let stdin_protocol = Py::new(py, PyProcessStdinProtocol { core: core.clone() })?.into_any();
    let stdin_context = py
        .import("contextvars")?
        .getattr("Context")?
        .call0()?
        .unbind();
    let transport = spawn_write_pipe_transport(
        py,
        crate::stream_transport::TransportSpawnContext {
            loop_core: core.loop_core.clone(),
            loop_obj: core.loop_obj.clone_ref(py),
            protocol: stdin_protocol,
            context: stdin_context,
            context_needs_run: false,
        },
        file_obj.clone_ref(py),
        extra_entries,
    )?;
    if let Err(err) = file_obj.call_method0(py, "close") {
        core.report_error(err, "subprocess stdin pipe close failed");
    }
    Ok(transport.into_any())
}

fn register_initial_pipe_transports(
    py: Python<'_>,
    core: &Arc<ProcessTransportCore>,
    stdin: Option<ChildStdin>,
    has_stdout: bool,
    has_stderr: bool,
) -> PyResult<()> {
    let mut pipe_transport_entries = Vec::with_capacity(3);
    if let Some(stdin) = stdin {
        let extra_entries = process_text_extra_entries(py, core.text_config.as_ref());
        let transport = spawn_stdin_pipe_transport(py, core, stdin, extra_entries)?;
        pipe_transport_entries.push((0, transport));
    }
    if has_stdout {
        pipe_transport_entries.push((1, new_process_pipe_transport(py, 1)?));
    }
    if has_stderr {
        pipe_transport_entries.push((2, new_process_pipe_transport(py, 2)?));
    }
    core.register_pipe_transports(pipe_transport_entries);
    Ok(())
}

fn spawn_process_reader_thread(
    name: &str,
    core: Arc<ProcessTransportCore>,
    fd: i32,
    reader: BoxedProcessReader,
) -> PyResult<()> {
    thread::Builder::new()
        .name(name.to_owned())
        .spawn(move || run_process_reader(core, fd, reader))
        .map(|_| ())
        .map_err(|err| PyRuntimeError::new_err(format!("failed to spawn {name}: {err}")))
}

fn spawn_process_waiter_thread(
    core: Arc<ProcessTransportCore>,
    child: Child,
    control_rx: Receiver<ProcessCommand>,
) -> PyResult<()> {
    thread::Builder::new()
        .name("rsloop-process-waiter".to_owned())
        .spawn(move || run_process_waiter(core, child, control_rx))
        .map(|_| ())
        .map_err(|err| PyRuntimeError::new_err(format!("failed to spawn process waiter: {err}")))
}

fn spawn_process_workers(
    core: Arc<ProcessTransportCore>,
    stdout: Option<BoxedProcessReader>,
    stderr: Option<BoxedProcessReader>,
    child: Child,
    control_rx: Receiver<ProcessCommand>,
) -> PyResult<()> {
    if let Some(stdout) = stdout {
        spawn_process_reader_thread("rsloop-process-stdout", core.clone(), 1, stdout)?;
    }
    if let Some(stderr) = stderr {
        spawn_process_reader_thread("rsloop-process-stderr", core.clone(), 2, stderr)?;
    }
    spawn_process_waiter_thread(core, child, control_rx)
}

pub fn spawn_process_transport(
    py: Python<'_>,
    params: ProcessTransportParams,
) -> PyResult<Py<PyProcessTransport>> {
    let ProcessTransportParams {
        loop_core,
        loop_obj,
        protocol,
        context,
        context_needs_run,
        text_config,
        mut child,
        stdout_override,
        stderr_override,
    } = params;
    let pid = child.id();
    let mut pipes = ProcessPipes::take_from(&mut child, stdout_override, stderr_override);
    let (control_tx, control_rx) = mpsc::channel();

    let core = Arc::new(ProcessTransportCore {
        loop_core,
        loop_obj,
        state: Mutex::new(ProcessState {
            protocol,
            context,
            context_needs_run,
            pid,
            returncode: None,
            closing: false,
            exited: false,
            connection_lost_called: false,
            open_pipes: pipes.open_pipes(),
            pipe_transports: HashMap::with_capacity(3),
        }),
        text_config,
        control_tx,
        exit_notify: AsyncEvent::new(),
        pending_events: Mutex::new(VecDeque::new()),
        events_scheduled: AtomicBool::new(false),
    });

    let stdout_open = pipes.stdout.is_some();
    let stderr_open = pipes.stderr.is_some();
    register_initial_pipe_transports(py, &core, pipes.stdin.take(), stdout_open, stderr_open)?;

    let transport = Py::new(py, PyProcessTransport { core: core.clone() })?;
    core.connection_made(transport.clone_ref(py))?;

    spawn_process_workers(core.clone(), pipes.stdout, pipes.stderr, child, control_rx)?;

    Ok(transport)
}

fn make_python_pipe_file(py: Python<'_>, fd: fd_ops::RawFd, mode: &str) -> PyResult<Py<PyAny>> {
    let os = py.import("os")?;
    let dup = fd_ops::dup_raw_fd(fd).map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
    Ok(os.getattr("fdopen")?.call1((dup, mode, 0))?.unbind())
}

#[cfg(windows)]
fn make_python_pipe_file_from_handle(
    py: Python<'_>,
    handle: std::os::windows::io::RawHandle,
    mode: &str,
) -> PyResult<Py<PyAny>> {
    let duplicated =
        fd_ops::duplicate_handle(handle).map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
    let msvcrt = py.import("msvcrt")?;
    let os = py.import("os")?;
    let flags = if mode.starts_with('r') {
        libc::O_RDONLY
    } else {
        libc::O_WRONLY
    } | libc::O_BINARY;
    let fd = msvcrt
        .getattr("open_osfhandle")?
        .call1((duplicated as isize, flags))?
        .extract::<i64>()?;
    Ok(os.getattr("fdopen")?.call1((fd, mode, 0))?.unbind())
}

fn report_process_result(core: &Arc<ProcessTransportCore>, result: PyResult<()>, message: &str) {
    if let Err(err) = result {
        core.report_error(err, message);
    }
}

#[inline]
fn report_process_io_error(core: &Arc<ProcessTransportCore>, err: std::io::Error, message: &str) {
    core.report_error(PyRuntimeError::new_err(err.to_string()), message);
}

#[cfg(unix)]
fn send_process_signal(child: &Child, signal: i32) -> std::io::Result<()> {
    // SAFETY: `libc::kill` is called with the child PID returned by `std::process::Child`
    // and a signal value supplied by the caller/Python API. It does not retain pointers.
    let result = unsafe { libc::kill(child.id() as i32, signal) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

fn process_exit_code(status: std::process::ExitStatus) -> i32 {
    #[cfg(unix)]
    {
        status
            .code()
            .or_else(|| status.signal().map(|signal| -signal))
            .unwrap_or(-1)
    }
    #[cfg(windows)]
    {
        status.code().unwrap_or(-1)
    }
}

fn handle_process_exit(core: &Arc<ProcessTransportCore>, code: i32) {
    if core
        .state
        .lock()
        .expect("poisoned process state")
        .open_pipes
        .contains(&0)
    {
        report_process_result(
            core,
            core.pipe_connection_lost(0, None),
            "subprocess pipe_connection_lost failed",
        );
    }
    report_process_result(
        core,
        core.process_exited(code),
        "subprocess process_exited failed",
    );
}

fn kill_process_child(core: &Arc<ProcessTransportCore>, child: &mut Child, message: &str) {
    if let Err(err) = child.kill() {
        report_process_io_error(core, err, message);
    }
}

fn handle_process_command(
    core: &Arc<ProcessTransportCore>,
    child: &mut Child,
    command: ProcessCommand,
) {
    match command {
        ProcessCommand::Close | ProcessCommand::Kill => {
            kill_process_child(core, child, "subprocess kill failed");
        }
        #[cfg(unix)]
        ProcessCommand::Terminate => {
            if let Err(err) = send_process_signal(child, libc::SIGTERM) {
                report_process_io_error(core, err, "subprocess terminate failed");
            }
        }
        #[cfg(unix)]
        ProcessCommand::SendSignal(sig) => {
            if let Err(err) = send_process_signal(child, sig) {
                report_process_io_error(core, err, "subprocess send_signal failed");
            }
        }
        #[cfg(windows)]
        ProcessCommand::Terminate | ProcessCommand::SendSignal(_) => {
            kill_process_child(core, child, "subprocess kill failed");
        }
    }
}

fn run_process_reader(core: Arc<ProcessTransportCore>, fd: i32, mut reader: BoxedProcessReader) {
    profiling::scope!("process.run_reader");
    let mut buf = [0_u8; PROCESS_READER_BUFFER_SIZE];
    loop {
        match reader.read(&mut buf) {
            Ok(0) => {
                report_process_result(
                    &core,
                    core.pipe_connection_lost(fd, None),
                    "subprocess pipe_connection_lost failed",
                );
                return;
            }
            Ok(n) => {
                if let Err(err) = core.pipe_data_received(fd, &buf[..n]) {
                    core.report_error(err, "subprocess pipe_data_received failed");
                    report_process_result(
                        &core,
                        core.pipe_connection_lost(fd, None),
                        "subprocess pipe_connection_lost failed",
                    );
                    return;
                }
            }
            Err(err) => {
                report_process_result(
                    &core,
                    core.pipe_connection_lost(fd, Some(PyRuntimeError::new_err(err.to_string()))),
                    "subprocess pipe_connection_lost failed",
                );
                return;
            }
        }
    }
}

fn run_process_waiter(
    core: Arc<ProcessTransportCore>,
    mut child: Child,
    control_rx: Receiver<ProcessCommand>,
) {
    profiling::scope!("process.run_waiter");
    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                handle_process_exit(&core, process_exit_code(status));
                return;
            }
            Ok(None) => {}
            Err(err) => {
                report_process_io_error(&core, err, "subprocess wait failed");
                report_process_result(
                    &core,
                    core.connection_lost(None),
                    "subprocess connection_lost failed",
                );
                return;
            }
        }

        match control_rx.recv_timeout(PROCESS_WAIT_POLL_INTERVAL) {
            Ok(command) => handle_process_command(&core, &mut child, command),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => return,
        }
    }
}
