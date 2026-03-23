use std::fs::File;
#[cfg(unix)]
use std::os::fd::FromRawFd;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
#[cfg(windows)]
use std::os::windows::io::FromRawHandle;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::process::Command;
use std::sync::Arc;
use std::sync::OnceLock;
use std::time::Duration;

use pyo3::exceptions::{PyNotImplementedError, PyRuntimeError, PyTypeError, PyValueError};
use pyo3::ffi;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyModule, PySlice, PyTuple};
use pyo3_async_runtimes::TaskLocals;

use crate::callbacks::{CallbackKind, PyHandle, PyTimerHandle};
use crate::context::{capture_context, ensure_running_loop, run_in_context};
use crate::fd_ops;
#[cfg(unix)]
use crate::loop_core::SignalHandlerTemplate;
use crate::loop_core::{LoopCommand, LoopCore, LoopCoreError};
use crate::process_transport::{
    spawn_process_transport, BoxedProcessReader, ProcessTextConfig, ProcessTransportParams,
};
use crate::python_names;
use crate::stream_transport::{
    create_server as create_py_server, remove_unix_socket_if_present, spawn_read_pipe_transport,
    spawn_write_pipe_transport, start_tls_transport, tcp_listener_from_socket_fd,
    tcp_server_listener, transport_from_socket, transport_from_socket_server_tls,
    transport_from_socket_tls, PyServer, PyStreamTransport, ServerCreateParams,
    TransportSpawnContext,
};
#[cfg(unix)]
use crate::stream_transport::{unix_listener_from_socket_fd, unix_server_listener};
use crate::tls::{client_tls_settings, server_tls_settings};

static ASYNCIO_TASK_CLS: OnceLock<Py<PyAny>> = OnceLock::new();
static ASYNCIO_FUTURE_CLS: OnceLock<Py<PyAny>> = OnceLock::new();
static ASYNCIO_GET_RUNNING_LOOP_FN: OnceLock<Py<PyAny>> = OnceLock::new();
static ASYNCIO_FUTURE_LOOP_KWNAMES: OnceLock<Py<PyTuple>> = OnceLock::new();
static ASYNCIO_TASK_LOOP_KWNAMES: OnceLock<Py<PyTuple>> = OnceLock::new();
static ASYNCIO_TASK_LOOP_NAME_KWNAMES: OnceLock<Py<PyTuple>> = OnceLock::new();
static ASYNCIO_TASK_LOOP_CONTEXT_KWNAMES: OnceLock<Py<PyTuple>> = OnceLock::new();
static ASYNCIO_TASK_LOOP_NAME_CONTEXT_KWNAMES: OnceLock<Py<PyTuple>> = OnceLock::new();

type ResolvedStreamAddrinfo = (i32, i32, i32, Py<PyAny>);

struct TcpServerSocketOptions {
    family: i32,
    flags: i32,
    backlog: i32,
    reuse_address: Option<bool>,
    reuse_port: Option<bool>,
    keep_alive: Option<bool>,
}

#[pyclass(subclass, module = "rsloop._loop")]
pub struct PyLoop {
    pub core: Arc<LoopCore>,
}

impl PyLoop {
    fn as_py_any(py: Python<'_>, slf: &Py<Self>) -> Py<PyAny> {
        slf.clone_ref(py).into_any()
    }

    fn task_locals(py: Python<'_>, slf: &Py<Self>) -> PyResult<TaskLocals> {
        TaskLocals::new(Self::as_py_any(py, slf).into_bound(py)).copy_context(py)
    }

    fn schedule_now(
        &self,
        py: Python<'_>,
        kind: CallbackKind,
        callback: Py<PyAny>,
        args: Py<PyTuple>,
        context: Option<Py<PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        let ready = self
            .core
            .schedule_callback(py, kind, callback, args, context)?;
        Ok(Py::new(py, PyHandle::new(ready.id(), &ready))?.into_any())
    }

    #[allow(dead_code)]
    fn not_implemented(feature: &str) -> PyErr {
        PyNotImplementedError::new_err(format!("{feature} is not implemented in rust-impl yet"))
    }

    fn map_loop_error(err: LoopCoreError) -> PyErr {
        PyRuntimeError::new_err(err.to_string())
    }
}

fn warn_default_executor_timeout(py: Python<'_>, timeout: f64) -> PyResult<()> {
    let warnings = py.import("warnings")?;
    let builtins = py.import("builtins")?;
    warnings.call_method(
        "warn",
        (
            format!("The executor did not finishing joining its threads within {timeout} seconds."),
            builtins.getattr("RuntimeWarning")?,
        ),
        Some(&{
            let kwargs = PyDict::new(py);
            kwargs.set_item("stacklevel", 2)?;
            kwargs
        }),
    )?;
    Ok(())
}

fn asyncio_task_cls(py: Python<'_>) -> PyResult<&Py<PyAny>> {
    if let Some(cached) = ASYNCIO_TASK_CLS.get() {
        return Ok(cached);
    }

    let loaded = py.import("asyncio")?.getattr("Task")?.unbind();
    Ok(ASYNCIO_TASK_CLS.get_or_init(|| loaded))
}

fn asyncio_future_cls(py: Python<'_>) -> PyResult<&Py<PyAny>> {
    if let Some(cached) = ASYNCIO_FUTURE_CLS.get() {
        return Ok(cached);
    }

    let loaded = py.import("asyncio")?.getattr("Future")?.unbind();
    Ok(ASYNCIO_FUTURE_CLS.get_or_init(|| loaded))
}

fn asyncio_get_running_loop_fn(py: Python<'_>) -> PyResult<&Py<PyAny>> {
    if let Some(cached) = ASYNCIO_GET_RUNNING_LOOP_FN.get() {
        return Ok(cached);
    }

    let loaded = py
        .import("asyncio.events")?
        .getattr("_get_running_loop")?
        .unbind();
    Ok(ASYNCIO_GET_RUNNING_LOOP_FN.get_or_init(|| loaded))
}

#[inline]
fn call_callable_noargs(py: Python<'_>, callable: &Py<PyAny>) -> PyResult<Py<PyAny>> {
    unsafe { Bound::from_owned_ptr_or_err(py, ffi::compat::PyObject_CallNoArgs(callable.as_ptr())) }
        .map(Bound::unbind)
}

#[inline]
fn call_callable_onearg(
    py: Python<'_>,
    callable: &Py<PyAny>,
    arg: &Bound<'_, PyAny>,
) -> PyResult<Py<PyAny>> {
    unsafe {
        Bound::from_owned_ptr_or_err(
            py,
            ffi::PyObject_CallFunctionObjArgs(
                callable.as_ptr(),
                arg.as_ptr(),
                std::ptr::null_mut::<ffi::PyObject>(),
            ),
        )
    }
    .map(Bound::unbind)
}

fn asyncio_future_loop_kwnames(py: Python<'_>) -> &Py<PyTuple> {
    ASYNCIO_FUTURE_LOOP_KWNAMES.get_or_init(|| {
        PyTuple::new(py, [python_names::loop_kw(py)])
            .expect("loop keyword tuple")
            .unbind()
    })
}

fn asyncio_task_kwnames_for_options(
    py: Python<'_>,
    include_name: bool,
    include_context: bool,
) -> &Py<PyTuple> {
    match (include_name, include_context) {
        (false, false) => ASYNCIO_TASK_LOOP_KWNAMES.get_or_init(|| {
            PyTuple::new(py, [python_names::loop_kw(py)])
                .expect("loop keyword tuple")
                .unbind()
        }),
        (true, false) => ASYNCIO_TASK_LOOP_NAME_KWNAMES.get_or_init(|| {
            PyTuple::new(py, [python_names::loop_kw(py), python_names::name_kw(py)])
                .expect("loop/name keyword tuple")
                .unbind()
        }),
        (false, true) => ASYNCIO_TASK_LOOP_CONTEXT_KWNAMES.get_or_init(|| {
            PyTuple::new(
                py,
                [python_names::loop_kw(py), python_names::context_kw(py)],
            )
            .expect("loop/context keyword tuple")
            .unbind()
        }),
        (true, true) => ASYNCIO_TASK_LOOP_NAME_CONTEXT_KWNAMES.get_or_init(|| {
            PyTuple::new(
                py,
                [
                    python_names::loop_kw(py),
                    python_names::name_kw(py),
                    python_names::context_kw(py),
                ],
            )
            .expect("loop/name/context keyword tuple")
            .unbind()
        }),
    }
}

fn is_current_running_loop(py: Python<'_>, loop_obj: &Py<PyAny>) -> PyResult<bool> {
    let current = asyncio_get_running_loop_fn(py)?.call0(py)?;
    if current.is_none(py) {
        return Ok(false);
    }
    Ok(current.bind(py).is(loop_obj.bind(py)))
}

fn create_asyncio_future_for_loop(py: Python<'_>, loop_obj: &Py<PyAny>) -> PyResult<Py<PyAny>> {
    #[cfg(any(Py_3_12, all(Py_3_11, not(Py_LIMITED_API))))]
    {
        let args = [loop_obj.as_ptr()];
        return unsafe {
            Bound::from_owned_ptr_or_err(
                py,
                ffi::PyObject_Vectorcall(
                    asyncio_future_cls(py)?.as_ptr(),
                    args.as_ptr(),
                    0,
                    asyncio_future_loop_kwnames(py).as_ptr(),
                ),
            )
        }
        .map(Bound::unbind);
    }

    #[cfg(not(any(Py_3_12, all(Py_3_11, not(Py_LIMITED_API)))))]
    {
        let kwargs = PyDict::new(py);
        kwargs.set_item("loop", loop_obj.clone_ref(py))?;
        asyncio_future_cls(py)?.call(py, (), Some(&kwargs))
    }
}

fn create_asyncio_future_for_running_loop(py: Python<'_>) -> PyResult<Py<PyAny>> {
    call_callable_noargs(py, asyncio_future_cls(py)?)
}

fn create_asyncio_task_for_loop(
    py: Python<'_>,
    loop_obj: &Py<PyAny>,
    coro: Py<PyAny>,
    name: Option<Py<PyAny>>,
    context: Option<Py<PyAny>>,
) -> PyResult<Py<PyAny>> {
    #[cfg(any(Py_3_12, all(Py_3_11, not(Py_LIMITED_API))))]
    {
        let name = name.as_ref();
        let context = context.as_ref();
        let mut args = Vec::with_capacity(4);
        args.push(coro.as_ptr());
        args.push(loop_obj.as_ptr());
        if let Some(name) = name {
            args.push(name.as_ptr());
        }
        if let Some(context) = context {
            args.push(context.as_ptr());
        }

        return unsafe {
            Bound::from_owned_ptr_or_err(
                py,
                ffi::PyObject_Vectorcall(
                    asyncio_task_cls(py)?.as_ptr(),
                    args.as_ptr(),
                    1,
                    asyncio_task_kwnames_for_options(py, name.is_some(), context.is_some())
                        .as_ptr(),
                ),
            )
        }
        .map(Bound::unbind);
    }

    #[cfg(not(any(Py_3_12, all(Py_3_11, not(Py_LIMITED_API)))))]
    {
        let kwargs = PyDict::new(py);
        kwargs.set_item("loop", loop_obj.clone_ref(py))?;
        if let Some(name) = name {
            kwargs.set_item("name", name)?;
        }
        if let Some(context) = context {
            kwargs.set_item("context", context)?;
        }
        asyncio_task_cls(py)?.call(py, (coro,), Some(&kwargs))
    }
}

fn create_asyncio_task_for_running_loop(py: Python<'_>, coro: Py<PyAny>) -> PyResult<Py<PyAny>> {
    call_callable_onearg(py, asyncio_task_cls(py)?, coro.bind(py))
}

fn create_asyncio_task_with_kwargs(
    py: Python<'_>,
    loop_obj: Option<&Py<PyAny>>,
    coro: Py<PyAny>,
    kwargs: &Bound<'_, PyDict>,
) -> PyResult<Py<PyAny>> {
    let task_kwargs = kwargs.copy()?;
    if let Some(loop_obj) = loop_obj {
        task_kwargs.set_item("loop", loop_obj.clone_ref(py))?;
    }
    asyncio_task_cls(py)?.call(py, (coro,), Some(&task_kwargs))
}

fn call_protocol_factory(
    py: Python<'_>,
    loop_obj: &Py<PyAny>,
    context: &Py<PyAny>,
    context_needs_run: bool,
    protocol_factory: &Py<PyAny>,
) -> PyResult<Py<PyAny>> {
    ensure_running_loop(py, loop_obj)?;
    let args = PyTuple::empty(py).unbind();
    run_in_context(py, context, context_needs_run, protocol_factory, &args)
}

fn is_asyncio_subprocess_stream_protocol(py: Python<'_>, protocol: &Py<PyAny>) -> PyResult<bool> {
    let asyncio_subprocess = py.import("asyncio.subprocess")?;
    let cls = asyncio_subprocess.getattr("SubprocessStreamProtocol")?;
    protocol.bind(py).is_instance(&cls)
}

fn resolve_stream_addrinfos(
    py: Python<'_>,
    host: Option<Py<PyAny>>,
    port: Option<Py<PyAny>>,
    family: i32,
    proto: i32,
    flags: i32,
) -> PyResult<Vec<ResolvedStreamAddrinfo>> {
    let socket_mod = py.import("socket")?;
    let kwargs = PyDict::new(py);
    kwargs.set_item("family", family)?;
    kwargs.set_item("type", socket_mod.getattr("SOCK_STREAM")?)?;
    kwargs.set_item("proto", proto)?;
    kwargs.set_item("flags", flags)?;

    let host = host.unwrap_or_else(|| py.None());
    let port = port.unwrap_or_else(|| py.None());
    let addrinfos = socket_mod
        .getattr("getaddrinfo")?
        .call((host, port), Some(&kwargs))?;

    let mut resolved = Vec::new();
    for entry in addrinfos.try_iter()? {
        let entry = entry?;
        let tuple = entry.cast::<PyTuple>()?;
        resolved.push((
            tuple.get_item(0)?.extract::<i32>()?,
            tuple.get_item(1)?.extract::<i32>()?,
            tuple.get_item(2)?.extract::<i32>()?,
            tuple.get_item(4)?.unbind(),
        ));
    }
    Ok(resolved)
}

fn build_stream_socket(
    py: Python<'_>,
    family: i32,
    sock_type: i32,
    proto: i32,
) -> PyResult<Py<PyAny>> {
    let socket_mod = py.import("socket")?;
    let sock = socket_mod
        .getattr("socket")?
        .call1((family, sock_type, proto))?;
    sock.call_method1("setblocking", (false,))?;
    Ok(sock.unbind())
}

async fn connect_socket_to_address(sock: Py<PyAny>, address: Py<PyAny>) -> PyResult<()> {
    let fd = Python::attach(|py| fd_ops::fileobj_to_fd(py, sock.bind(py)))?;
    match Python::attach(|py| -> PyResult<()> {
        sock.call_method1(py, "connect", (address.clone_ref(py),))?;
        Ok(())
    }) {
        Ok(()) => return Ok(()),
        Err(err) => {
            let retry = Python::attach(|py| fd_ops::is_retryable_socket_error(py, &err))?;
            if !retry {
                if Python::attach(|py| is_already_connected_socket_error(py, &err))? {
                    return Ok(());
                }
                return Err(err);
            }
        }
    }

    loop {
        fd_ops::wait_writable(fd).await?;
        let so_error = Python::attach(|py| socket_so_error(py, &sock))?;
        if so_error == 0 {
            return Ok(());
        }
        if is_connect_in_progress_errno(so_error) {
            continue;
        }
        if is_already_connected_errno(so_error) {
            return Ok(());
        }
        return Python::attach(|py| socket_os_error(py, so_error));
    }
}

fn socket_so_error(py: Python<'_>, sock: &Py<PyAny>) -> PyResult<i32> {
    let socket_mod = py.import("socket")?;
    sock.call_method1(
        py,
        "getsockopt",
        (
            socket_mod.getattr("SOL_SOCKET")?,
            socket_mod.getattr("SO_ERROR")?,
        ),
    )?
    .extract(py)
}

fn socket_os_error(py: Python<'_>, errno: i32) -> PyResult<()> {
    let builtins = py.import("builtins")?;
    let oserror = builtins.getattr("OSError")?;
    Err(PyErr::from_value(oserror.call1((
        errno,
        format!("socket connect failed: {errno}"),
    ))?))
}

fn is_already_connected_socket_error(py: Python<'_>, err: &PyErr) -> PyResult<bool> {
    let builtins = py.import("builtins")?;
    let oserror = builtins.getattr("OSError")?;
    if !err.is_instance(py, &oserror) {
        return Ok(false);
    }
    Ok(err
        .value(py)
        .getattr("errno")?
        .extract::<i32>()
        .ok()
        .is_some_and(is_already_connected_errno))
}

fn is_already_connected_errno(errno: i32) -> bool {
    errno == libc::EISCONN || errno == 10056
}

fn is_connect_in_progress_errno(errno: i32) -> bool {
    errno == libc::EINPROGRESS || errno == libc::EALREADY || errno == libc::EWOULDBLOCK
}

fn listener_sources_from_sockets(
    py: Python<'_>,
    sockets: &[Py<PyAny>],
) -> PyResult<Vec<crate::stream_transport::ServerListener>> {
    let mut listeners = Vec::with_capacity(sockets.len());
    for socket in sockets {
        let family = socket.getattr(py, "family")?.extract::<i32>(py)?;
        #[cfg(windows)]
        let fd = socket.call_method0(py, "fileno")?.extract(py)?;
        #[cfg(not(windows))]
        let fd = socket
            .call_method0(py, "dup")?
            .call_method0(py, "detach")?
            .extract(py)?;
        #[cfg(unix)]
        {
            listeners.push(if family == libc::AF_UNIX {
                unix_server_listener(unix_listener_from_socket_fd(fd)?)
            } else {
                tcp_server_listener(tcp_listener_from_socket_fd(fd)?)
            });
        }
        #[cfg(not(unix))]
        {
            let _ = family;
            listeners.push(tcp_server_listener(tcp_listener_from_socket_fd(fd)?));
        }
    }
    Ok(listeners)
}

fn build_tcp_server_sockets(
    py: Python<'_>,
    host: Option<Py<PyAny>>,
    port: Option<Py<PyAny>>,
    options: TcpServerSocketOptions,
) -> PyResult<Vec<Py<PyAny>>> {
    let TcpServerSocketOptions {
        family,
        flags,
        backlog,
        reuse_address,
        reuse_port,
        keep_alive,
    } = options;
    let socket_mod = py.import("socket")?;
    let sol_socket = socket_mod.getattr("SOL_SOCKET")?;
    let so_reuseaddr = socket_mod.getattr("SO_REUSEADDR")?;
    let so_reuseport = socket_mod.getattr("SO_REUSEPORT").ok();
    let so_keepalive = socket_mod.getattr("SO_KEEPALIVE")?;
    let addrinfos = resolve_stream_addrinfos(py, host, port, family, 0, flags)?;
    let mut sockets = Vec::with_capacity(addrinfos.len());

    for (addr_family, sock_type, proto, sockaddr) in addrinfos {
        let sock = build_stream_socket(py, addr_family, sock_type, proto)?;
        if reuse_address == Some(true) {
            sock.call_method1(
                py,
                "setsockopt",
                (sol_socket.clone(), so_reuseaddr.clone(), 1),
            )?;
        }
        if reuse_port == Some(true) {
            if let Some(so_reuseport) = &so_reuseport {
                sock.call_method1(
                    py,
                    "setsockopt",
                    (sol_socket.clone(), so_reuseport.clone(), 1),
                )?;
            }
        }
        if let Some(keep_alive) = keep_alive {
            sock.call_method1(
                py,
                "setsockopt",
                (
                    sol_socket.clone(),
                    so_keepalive.clone(),
                    i32::from(keep_alive),
                ),
            )?;
        }
        sock.call_method1(py, "bind", (sockaddr,))?;
        sock.call_method1(py, "listen", (backlog,))?;
        sockets.push(sock);
    }

    Ok(sockets)
}

fn build_unix_server_socket(
    py: Python<'_>,
    path: Option<Py<PyAny>>,
    backlog: i32,
) -> PyResult<Py<PyAny>> {
    let Some(path) = path else {
        return Err(PyRuntimeError::new_err(
            "path is required when sock is not provided",
        ));
    };

    let socket_mod = py.import("socket")?;
    let sock = socket_mod.getattr("socket")?.call1((
        socket_mod.getattr("AF_UNIX")?,
        socket_mod.getattr("SOCK_STREAM")?,
    ))?;
    sock.call_method1("setblocking", (false,))?;
    remove_unix_socket_if_present(&path.bind(py).extract::<String>()?)
        .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
    sock.call_method1("bind", (path,))?;
    sock.call_method1("listen", (backlog,))?;
    Ok(sock.unbind())
}

enum ProcessStdioSpec {
    Inherit,
    Pipe,
    DevNull,
    Fd(fd_ops::RawFd),
    Stdout,
}

#[derive(Clone)]
struct UnixPreExecConfig {
    restore_signals: bool,
    start_new_session: bool,
    process_group: Option<i32>,
    pass_fds: Vec<i32>,
    gid: Option<u32>,
    extra_groups: Option<Vec<u32>>,
    uid: Option<u32>,
    umask: Option<u32>,
}

#[derive(Clone)]
struct ProcessSpawnConfig {
    text: Option<ProcessTextConfig>,
    unix: UnixPreExecConfig,
}

impl Default for UnixPreExecConfig {
    fn default() -> Self {
        Self {
            restore_signals: true,
            start_new_session: false,
            process_group: None,
            pass_fds: Vec::new(),
            gid: None,
            extra_groups: None,
            uid: None,
            umask: None,
        }
    }
}

fn parse_process_stdio(
    py: Python<'_>,
    value: &Py<PyAny>,
    allow_stdout_redirect: bool,
) -> PyResult<ProcessStdioSpec> {
    let subprocess = py.import("asyncio.subprocess")?;
    let bound = value.bind(py);
    if bound.is_none() {
        return Ok(ProcessStdioSpec::Inherit);
    }
    if bound.eq(&subprocess.getattr("PIPE")?)? {
        return Ok(ProcessStdioSpec::Pipe);
    }
    if bound.eq(&subprocess.getattr("DEVNULL")?)? {
        return Ok(ProcessStdioSpec::DevNull);
    }
    if allow_stdout_redirect && bound.eq(&subprocess.getattr("STDOUT")?)? {
        return Ok(ProcessStdioSpec::Stdout);
    }
    if !allow_stdout_redirect && bound.eq(&subprocess.getattr("STDOUT")?)? {
        return Err(PyValueError::new_err("STDOUT can only be used for stderr"));
    }
    Ok(ProcessStdioSpec::Fd(fd_ops::fileobj_to_fd(py, bound)?))
}

fn stdio_from_fd(fd: fd_ops::RawFd) -> PyResult<std::process::Stdio> {
    #[cfg(windows)]
    {
        let handle = fd_ops::duplicate_handle_from_fd(fd)
            .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
        let file = unsafe { File::from_raw_handle(handle as _) };
        return Ok(std::process::Stdio::from(file));
    }

    #[cfg(unix)]
    {
        let dup = fd_ops::dup_raw_fd(fd as fd_ops::RawFd)
            .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
        let file = unsafe { File::from_raw_fd(dup as i32) };
        Ok(std::process::Stdio::from(file))
    }
}

fn new_pipe() -> PyResult<(File, File)> {
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::HANDLE;
        use windows_sys::Win32::System::Pipes::CreatePipe;

        let mut read_end: HANDLE = std::ptr::null_mut();
        let mut write_end: HANDLE = std::ptr::null_mut();
        let ok = unsafe { CreatePipe(&mut read_end, &mut write_end, std::ptr::null(), 0) };
        if ok == 0 {
            return Err(PyRuntimeError::new_err(
                std::io::Error::last_os_error().to_string(),
            ));
        }
        let read_end = unsafe { File::from_raw_handle(read_end as _) };
        let write_end = unsafe { File::from_raw_handle(write_end as _) };
        return Ok((read_end, write_end));
    }

    #[cfg(unix)]
    {
        let mut fds = [0_i32; 2];
        let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
        if rc == -1 {
            return Err(PyRuntimeError::new_err(
                std::io::Error::last_os_error().to_string(),
            ));
        }
        let read_end = unsafe { File::from_raw_fd(fds[0]) };
        let write_end = unsafe { File::from_raw_fd(fds[1]) };
        Ok((read_end, write_end))
    }
}

fn apply_stdio(
    command: &mut Command,
    stdin: ProcessStdioSpec,
    stdout: ProcessStdioSpec,
    stderr: ProcessStdioSpec,
) -> PyResult<(Option<BoxedProcessReader>, Option<BoxedProcessReader>)> {
    use std::process::Stdio;

    match stdin {
        ProcessStdioSpec::Inherit => {
            command.stdin(Stdio::inherit());
        }
        ProcessStdioSpec::Pipe => {
            command.stdin(Stdio::piped());
        }
        ProcessStdioSpec::DevNull => {
            command.stdin(Stdio::null());
        }
        ProcessStdioSpec::Fd(fd) => {
            command.stdin(stdio_from_fd(fd)?);
        }
        ProcessStdioSpec::Stdout => {
            return Err(PyValueError::new_err("STDOUT can only be used for stderr"));
        }
    }

    let mut stdout_override = None;
    let stderr_override = None;

    match (stdout, stderr) {
        (ProcessStdioSpec::Pipe, ProcessStdioSpec::Stdout) => {
            let (read_end, write_end) = new_pipe()?;
            let stderr_end = write_end
                .try_clone()
                .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
            command.stdout(Stdio::from(write_end));
            command.stderr(Stdio::from(stderr_end));
            stdout_override = Some(Box::new(read_end) as BoxedProcessReader);
        }
        (stdout, stderr) => {
            match stdout {
                ProcessStdioSpec::Inherit => {
                    command.stdout(Stdio::inherit());
                }
                ProcessStdioSpec::Pipe => {
                    command.stdout(Stdio::piped());
                }
                ProcessStdioSpec::DevNull => {
                    command.stdout(Stdio::null());
                }
                ProcessStdioSpec::Fd(fd) => {
                    command.stdout(stdio_from_fd(fd)?);
                }
                ProcessStdioSpec::Stdout => {
                    return Err(PyValueError::new_err("STDOUT can only be used for stderr"));
                }
            }

            match stderr {
                ProcessStdioSpec::Inherit => {
                    command.stderr(Stdio::inherit());
                }
                ProcessStdioSpec::Pipe => {
                    command.stderr(Stdio::piped());
                }
                ProcessStdioSpec::DevNull => {
                    command.stderr(Stdio::null());
                }
                ProcessStdioSpec::Fd(fd) => {
                    command.stderr(stdio_from_fd(fd)?);
                }
                ProcessStdioSpec::Stdout => {
                    match stdout {
                        ProcessStdioSpec::Inherit => command.stderr(Stdio::inherit()),
                        ProcessStdioSpec::DevNull => command.stderr(Stdio::null()),
                        ProcessStdioSpec::Fd(fd) => command.stderr(stdio_from_fd(fd)?),
                        ProcessStdioSpec::Pipe | ProcessStdioSpec::Stdout => {
                            return Err(PyRuntimeError::new_err(
                                "invalid stderr=STDOUT configuration",
                            ));
                        }
                    };
                }
            }
        }
    }

    Ok((stdout_override, stderr_override))
}

fn resolve_numeric_id(
    py: Python<'_>,
    value: &Bound<'_, PyAny>,
    module_name: &str,
    lookup: &str,
    field: &str,
    label: &str,
) -> PyResult<u32> {
    if let Ok(id) = value.extract::<u32>() {
        return Ok(id);
    }
    if let Ok(name) = value.extract::<String>() {
        let module = py.import(module_name)?;
        let entry = module.getattr(lookup)?.call1((name,))?;
        return entry.getattr(field)?.extract::<u32>();
    }
    Err(PyTypeError::new_err(format!(
        "{label} must be an int or str"
    )))
}

fn resolve_extra_groups(py: Python<'_>, value: &Bound<'_, PyAny>) -> PyResult<Vec<u32>> {
    let mut groups = Vec::new();
    for item in value.try_iter()? {
        let item = item?;
        groups.push(resolve_numeric_id(
            py,
            &item,
            "grp",
            "getgrnam",
            "gr_gid",
            "extra_groups entries",
        )?);
    }
    Ok(groups)
}

fn parse_process_text_config(
    py: Python<'_>,
    universal_newlines: bool,
    encoding: Option<Py<PyAny>>,
    errors: Option<Py<PyAny>>,
    text: Option<bool>,
) -> PyResult<Option<ProcessTextConfig>> {
    if text == Some(false) && universal_newlines {
        return Err(PyValueError::new_err(
            "text and universal_newlines have different values",
        ));
    }
    let text_enabled =
        universal_newlines || text == Some(true) || encoding.is_some() || errors.is_some();
    if !text_enabled {
        return Ok(None);
    }
    let encoding = if let Some(encoding) = encoding {
        encoding.bind(py).extract::<String>()?
    } else {
        py.import("locale")?
            .getattr("getpreferredencoding")?
            .call1((false,))?
            .extract::<String>()?
    };
    let errors = if let Some(errors) = errors {
        errors.bind(py).extract::<String>()?
    } else {
        "strict".to_owned()
    };
    Ok(Some(ProcessTextConfig {
        encoding,
        errors,
        translate_newlines: true,
    }))
}

#[cfg(unix)]
fn apply_unix_pre_exec(command: &mut Command, config: UnixPreExecConfig) {
    unsafe {
        command.pre_exec(move || {
            if config.restore_signals {
                if libc::signal(libc::SIGPIPE, libc::SIG_DFL) == libc::SIG_ERR {
                    return Err(std::io::Error::last_os_error());
                }
                #[cfg(any(target_os = "linux", target_os = "android"))]
                {
                    if libc::signal(libc::SIGXFSZ, libc::SIG_DFL) == libc::SIG_ERR {
                        return Err(std::io::Error::last_os_error());
                    }
                }
            }

            for fd in &config.pass_fds {
                let flags = libc::fcntl(*fd, libc::F_GETFD);
                if flags == -1 {
                    return Err(std::io::Error::last_os_error());
                }
                if libc::fcntl(*fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
            }

            if config.start_new_session && libc::setsid() == -1 {
                return Err(std::io::Error::last_os_error());
            }
            if let Some(process_group) = config.process_group {
                if !(config.start_new_session && process_group == 0)
                    && libc::setpgid(0, process_group) == -1
                {
                    return Err(std::io::Error::last_os_error());
                }
            }
            if let Some(groups) = &config.extra_groups {
                #[cfg(any(target_os = "linux", target_os = "android"))]
                let ngroups = groups.len();
                #[cfg(not(any(target_os = "linux", target_os = "android")))]
                let ngroups: libc::c_int = groups.len().try_into().map_err(|_| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "extra_groups length exceeds platform limit",
                    )
                })?;

                if libc::setgroups(ngroups, groups.as_ptr()) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            if let Some(gid) = config.gid {
                if libc::setgid(gid) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            if let Some(uid) = config.uid {
                if libc::setuid(uid) == -1 {
                    return Err(std::io::Error::last_os_error());
                }
            }
            if let Some(umask) = config.umask {
                libc::umask(umask as libc::mode_t);
            }
            Ok(())
        });
    }
}

#[cfg(not(unix))]
fn apply_unix_pre_exec(_command: &mut Command, _config: UnixPreExecConfig) {}

fn apply_common_process_kwargs(
    py: Python<'_>,
    command: &mut Command,
    kwargs: Option<&Bound<'_, PyDict>>,
) -> PyResult<ProcessSpawnConfig> {
    let mut spawn_config = ProcessSpawnConfig {
        text: None,
        unix: UnixPreExecConfig::default(),
    };
    let Some(kwargs) = kwargs else {
        return Ok(spawn_config);
    };

    for (key, value) in kwargs.iter() {
        let key = key.extract::<String>()?;
        match key.as_str() {
            "cwd" => {
                let os = py.import("os")?;
                let path = os
                    .getattr("fspath")?
                    .call1((value.clone(),))?
                    .extract::<String>()?;
                command.current_dir(path);
            }
            "env" => {
                let env_dict = value.cast::<PyDict>()?;
                for (env_key, env_value) in env_dict.iter() {
                    command.env(env_key.extract::<String>()?, env_value.extract::<String>()?);
                }
            }
            "executable" => {
                let os = py.import("os")?;
                let executable = os
                    .getattr("fspath")?
                    .call1((value.clone(),))?
                    .extract::<String>()?;
                #[cfg(unix)]
                command.arg0(executable);
                #[cfg(windows)]
                {
                    let _ = executable;
                    return Err(PyNotImplementedError::new_err(
                        "subprocess executable override is not implemented on Windows",
                    ));
                }
            }
            "restore_signals" => {
                spawn_config.unix.restore_signals = value.is_truthy()?;
            }
            "start_new_session" => {
                spawn_config.unix.start_new_session = value.is_truthy()?;
            }
            "process_group" => {
                if !value.is_none() {
                    spawn_config.unix.process_group = Some(value.extract::<i32>()?);
                }
            }
            "pass_fds" => {
                if !value.is_none() {
                    spawn_config.unix.pass_fds = value
                        .try_iter()?
                        .map(|item| item.and_then(|value| value.extract::<i32>()))
                        .collect::<PyResult<Vec<_>>>()?;
                }
            }
            "group" => {
                if !value.is_none() {
                    spawn_config.unix.gid = Some(resolve_numeric_id(
                        py, &value, "grp", "getgrnam", "gr_gid", "group",
                    )?);
                }
            }
            "extra_groups" => {
                if !value.is_none() {
                    spawn_config.unix.extra_groups = Some(resolve_extra_groups(py, &value)?);
                }
            }
            "user" => {
                if !value.is_none() {
                    spawn_config.unix.uid = Some(resolve_numeric_id(
                        py, &value, "pwd", "getpwnam", "pw_uid", "user",
                    )?);
                }
            }
            "umask" => {
                if !value.is_none() {
                    let mask = value.extract::<i64>()?;
                    if !(0..=0o777).contains(&mask) {
                        return Err(PyValueError::new_err("umask must be between 0 and 0o777"));
                    }
                    spawn_config.unix.umask = Some(mask as u32);
                }
            }
            "preexec_fn" => {
                if !value.is_none() {
                    return Err(PyNotImplementedError::new_err(
                        "preexec_fn remains unsupported in rust-impl because it is unsafe in this runtime model",
                    ));
                }
            }
            _ => {}
        }
    }

    apply_unix_pre_exec(command, spawn_config.unix.clone());
    Ok(spawn_config)
}

#[pymethods]
impl PyLoop {
    #[new]
    fn new() -> Self {
        Self {
            core: LoopCore::new(),
        }
    }

    #[pyo3(signature=(callback, *args, context=None))]
    fn call_soon(
        &self,
        py: Python<'_>,
        callback: Py<PyAny>,
        args: &Bound<'_, PyTuple>,
        context: Option<Py<PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        self.schedule_now(
            py,
            CallbackKind::Soon,
            callback,
            args.clone().unbind(),
            context,
        )
    }

    #[pyo3(signature=(callback, *args, context=None))]
    fn call_soon_threadsafe(
        &self,
        py: Python<'_>,
        callback: Py<PyAny>,
        args: &Bound<'_, PyTuple>,
        context: Option<Py<PyAny>>,
    ) -> PyResult<Py<PyAny>> {
        self.schedule_now(
            py,
            CallbackKind::Threadsafe,
            callback,
            args.clone().unbind(),
            context,
        )
    }

    #[pyo3(signature=(delay, callback, *args, context=None))]
    fn call_later(
        &self,
        py: Python<'_>,
        delay: f64,
        callback: Py<PyAny>,
        args: &Bound<'_, PyTuple>,
        context: Option<Py<PyAny>>,
    ) -> PyResult<Py<PyTimerHandle>> {
        let delay = delay.max(0.0);
        let (ready, when) = self.core.schedule_timer(
            py,
            Duration::from_secs_f64(delay),
            callback,
            args.clone().unbind(),
            context,
        )?;

        Py::new(py, PyTimerHandle::new(ready.id(), when, &ready))
    }

    #[pyo3(signature=(when, callback, *args, context=None))]
    fn call_at(
        &self,
        py: Python<'_>,
        when: f64,
        callback: Py<PyAny>,
        args: &Bound<'_, PyTuple>,
        context: Option<Py<PyAny>>,
    ) -> PyResult<Py<PyTimerHandle>> {
        let delay = (when - self.time()).max(0.0);
        self.call_later(py, delay, callback, args, context)
    }

    fn time(&self) -> f64 {
        self.core.time()
    }

    fn stop(&self) -> PyResult<()> {
        self.core.schedule_stop().map_err(Self::map_loop_error)
    }

    fn close(&self, py: Python<'_>) -> PyResult<()> {
        let executor = {
            let mut state = self.core.state.lock().expect("poisoned loop state");
            if state.running {
                return Err(Self::map_loop_error(LoopCoreError::Running));
            }
            if state.closed {
                return Ok(());
            }
            state.executor_shutdown_called = true;
            state.default_executor.take()
        };

        self.core.close().map_err(Self::map_loop_error)?;

        if let Some(executor) = executor {
            executor.call_method1(py, "shutdown", (false,))?;
        }

        Ok(())
    }

    fn is_running(&self) -> bool {
        self.core.is_running()
    }

    fn is_closed(&self) -> bool {
        self.core.is_closed()
    }

    fn get_debug(&self) -> bool {
        self.core.get_debug()
    }

    fn set_debug(&self, enabled: bool) {
        self.core.set_debug(enabled);
    }

    #[profiling::function]
    fn run_forever(slf: Py<Self>, py: Python<'_>) -> PyResult<()> {
        let loop_obj = Self::as_py_any(py, &slf);
        slf.borrow(py).core.run_forever(py, loop_obj)
    }

    #[profiling::function]
    fn run_until_complete(slf: Py<Self>, py: Python<'_>, future: Py<PyAny>) -> PyResult<Py<PyAny>> {
        let asyncio = py.import("asyncio")?;
        let new_task = !asyncio
            .getattr("isfuture")?
            .call1((future.clone_ref(py),))?
            .extract::<bool>()?;
        let kwargs = PyDict::new(py);
        kwargs.set_item("loop", Self::as_py_any(py, &slf))?;
        let wrapped = asyncio
            .getattr("ensure_future")?
            .call((future,), Some(&kwargs))?;

        let helper_mod = PyModule::import(py, "rsloop._loop")?;
        let functools = py.import("functools")?;
        let stopper = functools.getattr("partial")?.call1((
            helper_mod.getattr("future_done_stop")?,
            Self::as_py_any(py, &slf),
        ))?;

        wrapped.call_method1("add_done_callback", (stopper.clone(),))?;
        let result = slf
            .borrow(py)
            .core
            .run_forever(py, Self::as_py_any(py, &slf));
        let _ = wrapped.call_method1("remove_done_callback", (stopper,));
        if let Err(err) = result {
            if wrapped.call_method0("done")?.extract::<bool>()?
                && !wrapped.call_method0("cancelled")?.extract::<bool>()?
            {
                let _ = wrapped.call_method0("result");
                if new_task {
                    let _ = wrapped.call_method0("exception");
                }
            }
            return Err(err);
        }

        if !wrapped.call_method0("done")?.extract::<bool>()? {
            return Err(PyRuntimeError::new_err(
                "Event loop stopped before Future completed.",
            ));
        }

        Ok(wrapped.call_method0("result")?.unbind())
    }

    fn create_future(slf: Py<Self>, py: Python<'_>) -> PyResult<Py<PyAny>> {
        let core = Arc::clone(&slf.borrow(py).core);
        let loop_obj = Self::as_py_any(py, &slf);
        if core.on_runtime_thread() {
            return create_asyncio_future_for_running_loop(py);
        }
        if is_current_running_loop(py, &loop_obj)? {
            return create_asyncio_future_for_running_loop(py);
        }

        create_asyncio_future_for_loop(py, &loop_obj)
    }

    #[pyo3(signature=(coro, *, name=None, context=None, eager_start=None, **kwargs))]
    fn create_task(
        slf: Py<Self>,
        py: Python<'_>,
        coro: Py<PyAny>,
        name: Option<Py<PyAny>>,
        context: Option<Py<PyAny>>,
        eager_start: Option<bool>,
        kwargs: Option<Py<PyDict>>,
    ) -> PyResult<Py<PyAny>> {
        let core = Arc::clone(&slf.borrow(py).core);
        let loop_obj = Self::as_py_any(py, &slf);
        let task_kwargs = PyDict::new(py);
        if let Some(kwargs_in) = kwargs.as_ref() {
            for (key, value) in kwargs_in.bind(py).iter() {
                task_kwargs.set_item(key, value)?;
            }
        }
        if let Some(name) = name.as_ref() {
            task_kwargs.set_item("name", name)?;
        }
        if let Some(context) = context.as_ref() {
            task_kwargs.set_item("context", context)?;
        }
        if let Some(eager_start) = eager_start {
            task_kwargs.set_item("eager_start", eager_start)?;
        }
        let has_kwargs = !task_kwargs.is_empty();

        if !core.has_task_factory() && !has_kwargs && core.on_runtime_thread() {
            return create_asyncio_task_for_running_loop(py, coro);
        }

        let task_factory = if core.has_task_factory() {
            core.state
                .lock()
                .expect("poisoned loop state")
                .task_factory
                .as_ref()
                .map(|factory| factory.clone_ref(py))
        } else {
            None
        };
        if let Some(factory) = task_factory {
            let created = factory.call(py, (loop_obj.clone_ref(py), coro), Some(&task_kwargs))?;
            return Ok(created);
        }

        if is_current_running_loop(py, &loop_obj)? {
            if !has_kwargs {
                return create_asyncio_task_for_running_loop(py, coro);
            }
            return create_asyncio_task_with_kwargs(py, None, coro, &task_kwargs);
        }

        if !has_kwargs {
            return create_asyncio_task_for_loop(py, &loop_obj, coro, name, context);
        }

        create_asyncio_task_with_kwargs(py, Some(&loop_obj), coro, &task_kwargs)
    }

    fn set_task_factory(&self, factory: Option<Py<PyAny>>) {
        let installed = factory.is_some();
        self.core
            .state
            .lock()
            .expect("poisoned loop state")
            .task_factory = factory;
        self.core.set_task_factory_installed(installed);
    }

    fn get_task_factory(&self) -> Option<Py<PyAny>> {
        Python::attach(|py| {
            self.core
                .state
                .lock()
                .expect("poisoned loop state")
                .task_factory
                .as_ref()
                .map(|factory| factory.clone_ref(py))
        })
    }

    fn default_exception_handler(&self, py: Python<'_>, context: Py<PyAny>) -> PyResult<()> {
        self.core.default_exception_handler(py, context)
    }

    fn get_exception_handler(&self) -> Option<Py<PyAny>> {
        Python::attach(|py| {
            self.core
                .state
                .lock()
                .expect("poisoned loop state")
                .exception_handler
                .as_ref()
                .map(|handler| handler.clone_ref(py))
        })
    }

    fn set_exception_handler(&self, handler: Option<Py<PyAny>>) {
        self.core
            .state
            .lock()
            .expect("poisoned loop state")
            .exception_handler = handler;
    }

    fn call_exception_handler(slf: Py<Self>, py: Python<'_>, context: Py<PyAny>) -> PyResult<()> {
        slf.borrow(py)
            .core
            .call_exception_handler(py, Some(&Self::as_py_any(py, &slf)), context)
    }

    fn set_default_executor(&self, executor: Option<Py<PyAny>>) {
        self.core
            .state
            .lock()
            .expect("poisoned loop state")
            .default_executor = executor;
    }

    fn __repr__(&self) -> String {
        format!(
            "<rsloop.Loop running={} closed={} debug={}>",
            self.is_running(),
            self.is_closed(),
            self.get_debug()
        )
    }

    #[pyo3(signature=(protocol_factory, host=None, port=None, *, family=0, flags=1, sock=None, backlog=100, ssl=None, reuse_address=None, reuse_port=None, keep_alive=None, ssl_handshake_timeout=None, ssl_shutdown_timeout=None, start_serving=true))]
    #[expect(
        clippy::too_many_arguments,
        reason = "Mirrors asyncio loop.create_server()"
    )]
    fn create_server(
        slf: Py<Self>,
        py: Python<'_>,
        protocol_factory: Py<PyAny>,
        host: Option<Py<PyAny>>,
        port: Option<Py<PyAny>>,
        family: i32,
        flags: i32,
        sock: Option<Py<PyAny>>,
        backlog: i32,
        ssl: Option<Py<PyAny>>,
        reuse_address: Option<bool>,
        reuse_port: Option<bool>,
        keep_alive: Option<bool>,
        ssl_handshake_timeout: Option<f64>,
        ssl_shutdown_timeout: Option<f64>,
        start_serving: bool,
    ) -> PyResult<Bound<'_, PyAny>> {
        profiling::scope!("PyLoop::create_server");
        if ssl_handshake_timeout.is_some() && ssl.is_none() {
            return Err(PyValueError::new_err(
                "ssl_handshake_timeout is only meaningful with ssl",
            ));
        }
        if ssl_shutdown_timeout.is_some() && ssl.is_none() {
            return Err(PyValueError::new_err(
                "ssl_shutdown_timeout is only meaningful with ssl",
            ));
        }
        let tls = ssl
            .as_ref()
            .map(|ssl| {
                server_tls_settings(
                    py,
                    ssl.bind(py),
                    ssl_handshake_timeout,
                    ssl_shutdown_timeout,
                )
            })
            .transpose()?
            .map(Arc::new);

        let locals = Self::task_locals(py, &slf)?;
        let loop_obj = Self::as_py_any(py, &slf);
        let core = slf.borrow(py).core.clone();
        let (context, context_needs_run) = capture_context(py, None)?;

        pyo3_async_runtimes::async_std::future_into_py_with_locals(py, locals, async move {
            let sockets = Python::attach(|py| -> PyResult<Vec<Py<PyAny>>> {
                if let Some(sock) = &sock {
                    sock.call_method1(py, "setblocking", (false,))?;
                    return Ok(vec![sock.clone_ref(py)]);
                }
                build_tcp_server_sockets(
                    py,
                    host,
                    port,
                    TcpServerSocketOptions {
                        family,
                        flags,
                        backlog,
                        reuse_address,
                        reuse_port,
                        keep_alive,
                    },
                )
            })?;

            let server = Python::attach(|py| -> PyResult<Py<PyServer>> {
                let listeners = listener_sources_from_sockets(py, &sockets)?;
                let server_sockets = sockets
                    .iter()
                    .map(|socket| socket.clone_ref(py))
                    .collect::<Vec<_>>();
                let server = create_py_server(
                    py,
                    ServerCreateParams {
                        loop_core: core.clone(),
                        loop_obj: loop_obj.clone_ref(py),
                        protocol_factory: protocol_factory.clone_ref(py),
                        context: context.clone_ref(py),
                        context_needs_run,
                        sockets: server_sockets,
                        listeners,
                        cleanup_path: None,
                        tls: tls.clone(),
                    },
                )?;
                if start_serving {
                    server.borrow(py).core.spawn_accept_tasks();
                }
                Ok(server)
            })?;

            Ok(Python::attach(|py| server.into_any().clone_ref(py)))
        })
    }

    #[pyo3(signature=(protocol_factory, host=None, port=None, *, ssl=None, family=0, proto=0, flags=0, sock=None, local_addr=None, server_hostname=None, ssl_handshake_timeout=None, ssl_shutdown_timeout=None, happy_eyeballs_delay=None, interleave=None, all_errors=false))]
    #[expect(
        clippy::too_many_arguments,
        reason = "Mirrors asyncio loop.create_connection()"
    )]
    fn create_connection(
        slf: Py<Self>,
        py: Python<'_>,
        protocol_factory: Py<PyAny>,
        host: Option<Py<PyAny>>,
        port: Option<Py<PyAny>>,
        ssl: Option<Py<PyAny>>,
        family: i32,
        proto: i32,
        flags: i32,
        sock: Option<Py<PyAny>>,
        local_addr: Option<Py<PyAny>>,
        server_hostname: Option<Py<PyAny>>,
        ssl_handshake_timeout: Option<f64>,
        ssl_shutdown_timeout: Option<f64>,
        happy_eyeballs_delay: Option<f64>,
        interleave: Option<i32>,
        all_errors: bool,
    ) -> PyResult<Bound<'_, PyAny>> {
        profiling::scope!("PyLoop::create_connection");
        let _ = (happy_eyeballs_delay, interleave, all_errors);
        if server_hostname.is_some() && ssl.is_none() {
            return Err(PyValueError::new_err(
                "server_hostname is only meaningful with ssl",
            ));
        }
        if ssl_handshake_timeout.is_some() && ssl.is_none() {
            return Err(PyValueError::new_err(
                "ssl_handshake_timeout is only meaningful with ssl",
            ));
        }
        if ssl_shutdown_timeout.is_some() && ssl.is_none() {
            return Err(PyValueError::new_err(
                "ssl_shutdown_timeout is only meaningful with ssl",
            ));
        }

        let locals = Self::task_locals(py, &slf)?;
        let loop_obj = Self::as_py_any(py, &slf);
        let core = slf.borrow(py).core.clone();
        let (context, context_needs_run) = capture_context(py, None)?;

        pyo3_async_runtimes::async_std::future_into_py_with_locals(py, locals, async move {
            let protocol = Python::attach(|py| {
                call_protocol_factory(
                    py,
                    &loop_obj,
                    &context,
                    context_needs_run,
                    &protocol_factory,
                )
            })?;

            let socket_obj = if let Some(sock) = sock {
                Python::attach(|py| -> PyResult<Py<PyAny>> {
                    sock.call_method1(py, "setblocking", (false,))?;
                    Ok(sock.clone_ref(py))
                })?
            } else {
                let addrinfos = Python::attach(|py| {
                    resolve_stream_addrinfos(py, host, port, family, proto, flags)
                })?;
                let mut last_error: Option<PyErr> = None;
                let mut connected: Option<Py<PyAny>> = None;

                for (addr_family, sock_type, resolved_proto, sockaddr) in addrinfos {
                    let sock = Python::attach(|py| -> PyResult<Py<PyAny>> {
                        let sock = build_stream_socket(py, addr_family, sock_type, resolved_proto)?;
                        if let Some(local_addr) = &local_addr {
                            let _ = sock.call_method1(py, "bind", (local_addr.clone_ref(py),));
                        }
                        Ok(sock)
                    })?;

                    let sock_for_connect = Python::attach(|py| sock.clone_ref(py));
                    match connect_socket_to_address(sock_for_connect, sockaddr).await {
                        Ok(()) => {
                            connected = Some(sock);
                            break;
                        }
                        Err(err) => {
                            last_error = Some(err);
                            let _ = Python::attach(|py| sock.call_method0(py, "close"));
                        }
                    }
                }

                connected.ok_or_else(|| {
                    last_error
                        .unwrap_or_else(|| PyRuntimeError::new_err("failed to connect socket"))
                })?
            };

            let transport = Python::attach(|py| {
                if let Some(ssl) = ssl.as_ref() {
                    let tls = client_tls_settings(
                        py,
                        ssl.bind(py),
                        server_hostname.as_ref().map(|value| value.bind(py)),
                        ssl_handshake_timeout,
                        ssl_shutdown_timeout,
                    )?;
                    transport_from_socket_tls(
                        py,
                        core.clone(),
                        loop_obj.clone_ref(py),
                        protocol.clone_ref(py),
                        context.clone_ref(py),
                        context_needs_run,
                        socket_obj,
                        tls,
                    )
                } else {
                    transport_from_socket(
                        py,
                        core.clone(),
                        loop_obj.clone_ref(py),
                        protocol.clone_ref(py),
                        context.clone_ref(py),
                        context_needs_run,
                        socket_obj,
                    )
                }
            })?;

            Python::attach(|py| {
                let result = PyTuple::new(py, [transport.into_any(), protocol.clone_ref(py)])?;
                Ok(result.unbind().into_any())
            })
        })
    }

    #[pyo3(signature=(protocol_factory, path=None, *, sock=None, backlog=100, ssl=None, ssl_handshake_timeout=None, ssl_shutdown_timeout=None, start_serving=true, cleanup_socket=true))]
    #[expect(
        clippy::too_many_arguments,
        reason = "Mirrors asyncio loop.create_unix_server()"
    )]
    fn create_unix_server(
        slf: Py<Self>,
        py: Python<'_>,
        protocol_factory: Py<PyAny>,
        path: Option<Py<PyAny>>,
        sock: Option<Py<PyAny>>,
        backlog: i32,
        ssl: Option<Py<PyAny>>,
        ssl_handshake_timeout: Option<f64>,
        ssl_shutdown_timeout: Option<f64>,
        start_serving: bool,
        cleanup_socket: bool,
    ) -> PyResult<Bound<'_, PyAny>> {
        if ssl_handshake_timeout.is_some() && ssl.is_none() {
            return Err(PyValueError::new_err(
                "ssl_handshake_timeout is only meaningful with ssl",
            ));
        }
        if ssl_shutdown_timeout.is_some() && ssl.is_none() {
            return Err(PyValueError::new_err(
                "ssl_shutdown_timeout is only meaningful with ssl",
            ));
        }
        #[cfg(not(unix))]
        {
            let _ = (
                slf,
                protocol_factory,
                path,
                sock,
                backlog,
                start_serving,
                cleanup_socket,
            );
            return Err(Self::not_implemented("create_unix_server"));
        }
        let tls = ssl
            .as_ref()
            .map(|ssl| {
                server_tls_settings(
                    py,
                    ssl.bind(py),
                    ssl_handshake_timeout,
                    ssl_shutdown_timeout,
                )
            })
            .transpose()?
            .map(Arc::new);

        let locals = Self::task_locals(py, &slf)?;
        let loop_obj = Self::as_py_any(py, &slf);
        let core = slf.borrow(py).core.clone();
        let (context, context_needs_run) = capture_context(py, None)?;

        pyo3_async_runtimes::async_std::future_into_py_with_locals(py, locals, async move {
            let socket_obj = Python::attach(|py| -> PyResult<Py<PyAny>> {
                if let Some(sock) = &sock {
                    sock.call_method1(py, "setblocking", (false,))?;
                    return Ok(sock.clone_ref(py));
                }
                build_unix_server_socket(
                    py,
                    path.as_ref().map(|value| value.clone_ref(py)),
                    backlog,
                )
            })?;

            let server = Python::attach(|py| -> PyResult<Py<PyServer>> {
                let sockets = vec![socket_obj.clone_ref(py)];
                let listeners = listener_sources_from_sockets(py, &sockets)?;
                let cleanup_path = if cleanup_socket {
                    path.as_ref()
                        .and_then(|value| value.bind(py).extract::<String>().ok())
                        .map(std::path::PathBuf::from)
                } else {
                    None
                };
                let server = create_py_server(
                    py,
                    ServerCreateParams {
                        loop_core: core.clone(),
                        loop_obj: loop_obj.clone_ref(py),
                        protocol_factory: protocol_factory.clone_ref(py),
                        context: context.clone_ref(py),
                        context_needs_run,
                        sockets,
                        listeners,
                        cleanup_path,
                        tls: tls.clone(),
                    },
                )?;
                if start_serving {
                    server.borrow(py).core.spawn_accept_tasks();
                }
                Ok(server)
            })?;

            Ok(Python::attach(|py| server.into_any().clone_ref(py)))
        })
    }

    #[pyo3(signature=(protocol_factory, path=None, *, ssl=None, sock=None, server_hostname=None, ssl_handshake_timeout=None, ssl_shutdown_timeout=None))]
    #[expect(
        clippy::too_many_arguments,
        reason = "Mirrors asyncio loop.create_unix_connection()"
    )]
    fn create_unix_connection(
        slf: Py<Self>,
        py: Python<'_>,
        protocol_factory: Py<PyAny>,
        path: Option<Py<PyAny>>,
        ssl: Option<Py<PyAny>>,
        sock: Option<Py<PyAny>>,
        server_hostname: Option<Py<PyAny>>,
        ssl_handshake_timeout: Option<f64>,
        ssl_shutdown_timeout: Option<f64>,
    ) -> PyResult<Bound<'_, PyAny>> {
        if server_hostname.is_some() && ssl.is_none() {
            return Err(PyValueError::new_err(
                "server_hostname is only meaningful with ssl",
            ));
        }
        if ssl_handshake_timeout.is_some() && ssl.is_none() {
            return Err(PyValueError::new_err(
                "ssl_handshake_timeout is only meaningful with ssl",
            ));
        }
        if ssl_shutdown_timeout.is_some() && ssl.is_none() {
            return Err(PyValueError::new_err(
                "ssl_shutdown_timeout is only meaningful with ssl",
            ));
        }
        #[cfg(not(unix))]
        {
            let _ = (slf, protocol_factory, path, sock);
            return Err(Self::not_implemented("create_unix_connection"));
        }

        let locals = Self::task_locals(py, &slf)?;
        let loop_obj = Self::as_py_any(py, &slf);
        let core = slf.borrow(py).core.clone();
        let (context, context_needs_run) = capture_context(py, None)?;

        pyo3_async_runtimes::async_std::future_into_py_with_locals(py, locals, async move {
            let protocol = Python::attach(|py| {
                call_protocol_factory(
                    py,
                    &loop_obj,
                    &context,
                    context_needs_run,
                    &protocol_factory,
                )
            })?;

            let socket_obj = if let Some(sock) = sock {
                Python::attach(|py| -> PyResult<Py<PyAny>> {
                    sock.call_method1(py, "setblocking", (false,))?;
                    Ok(sock.clone_ref(py))
                })?
            } else {
                let socket_obj = Python::attach(|py| -> PyResult<Py<PyAny>> {
                    let socket_mod = py.import("socket")?;
                    let sock = socket_mod.getattr("socket")?.call1((
                        socket_mod.getattr("AF_UNIX")?,
                        socket_mod.getattr("SOCK_STREAM")?,
                    ))?;
                    sock.call_method1("setblocking", (false,))?;
                    Ok(sock.unbind())
                })?;
                let address = path.ok_or_else(|| {
                    PyRuntimeError::new_err("path is required when sock is not provided")
                })?;
                let socket_for_connect = Python::attach(|py| socket_obj.clone_ref(py));
                connect_socket_to_address(socket_for_connect, address).await?;
                socket_obj
            };

            let transport = Python::attach(|py| {
                if let Some(ssl) = ssl.as_ref() {
                    let tls = client_tls_settings(
                        py,
                        ssl.bind(py),
                        server_hostname.as_ref().map(|value| value.bind(py)),
                        ssl_handshake_timeout,
                        ssl_shutdown_timeout,
                    )?;
                    transport_from_socket_tls(
                        py,
                        core.clone(),
                        loop_obj.clone_ref(py),
                        protocol.clone_ref(py),
                        context.clone_ref(py),
                        context_needs_run,
                        socket_obj,
                        tls,
                    )
                } else {
                    transport_from_socket(
                        py,
                        core.clone(),
                        loop_obj.clone_ref(py),
                        protocol.clone_ref(py),
                        context.clone_ref(py),
                        context_needs_run,
                        socket_obj,
                    )
                }
            })?;

            Python::attach(|py| {
                let result = PyTuple::new(py, [transport.into_any(), protocol.clone_ref(py)])?;
                Ok(result.unbind().into_any())
            })
        })
    }

    #[pyo3(signature=(protocol_factory, sock, *, ssl=None, ssl_handshake_timeout=None, ssl_shutdown_timeout=None))]
    fn connect_accepted_socket(
        slf: Py<Self>,
        py: Python<'_>,
        protocol_factory: Py<PyAny>,
        sock: Py<PyAny>,
        ssl: Option<Py<PyAny>>,
        ssl_handshake_timeout: Option<f64>,
        ssl_shutdown_timeout: Option<f64>,
    ) -> PyResult<Bound<'_, PyAny>> {
        profiling::scope!("PyLoop::connect_accepted_socket");
        if ssl_handshake_timeout.is_some() && ssl.is_none() {
            return Err(PyValueError::new_err(
                "ssl_handshake_timeout is only meaningful with ssl",
            ));
        }
        if ssl_shutdown_timeout.is_some() && ssl.is_none() {
            return Err(PyValueError::new_err(
                "ssl_shutdown_timeout is only meaningful with ssl",
            ));
        }

        let locals = Self::task_locals(py, &slf)?;
        let loop_obj = Self::as_py_any(py, &slf);
        let core = slf.borrow(py).core.clone();
        let (context, context_needs_run) = capture_context(py, None)?;

        pyo3_async_runtimes::async_std::future_into_py_with_locals(py, locals, async move {
            let protocol = Python::attach(|py| {
                call_protocol_factory(
                    py,
                    &loop_obj,
                    &context,
                    context_needs_run,
                    &protocol_factory,
                )
            })?;
            let socket_obj = Python::attach(|py| -> PyResult<Py<PyAny>> {
                sock.call_method1(py, "setblocking", (false,))?;
                Ok(sock.clone_ref(py))
            })?;
            let transport = Python::attach(|py| {
                if let Some(ssl) = ssl.as_ref() {
                    let tls = server_tls_settings(
                        py,
                        ssl.bind(py),
                        ssl_handshake_timeout,
                        ssl_shutdown_timeout,
                    )?;
                    transport_from_socket_server_tls(
                        py,
                        core.clone(),
                        loop_obj.clone_ref(py),
                        protocol.clone_ref(py),
                        context.clone_ref(py),
                        context_needs_run,
                        socket_obj,
                        tls,
                    )
                } else {
                    transport_from_socket(
                        py,
                        core.clone(),
                        loop_obj.clone_ref(py),
                        protocol.clone_ref(py),
                        context.clone_ref(py),
                        context_needs_run,
                        socket_obj,
                    )
                }
            })?;
            Python::attach(|py| {
                let result = PyTuple::new(py, [transport.into_any(), protocol.clone_ref(py)])?;
                Ok(result.unbind().into_any())
            })
        })
    }

    #[pyo3(signature=(transport, protocol, sslcontext, *, server_side=false, server_hostname=None, ssl_handshake_timeout=None, ssl_shutdown_timeout=None))]
    fn start_tls(
        slf: Py<Self>,
        py: Python<'_>,
        transport: Py<PyAny>,
        protocol: Py<PyAny>,
        sslcontext: Py<PyAny>,
        server_side: bool,
        server_hostname: Option<Py<PyAny>>,
        ssl_handshake_timeout: Option<f64>,
        ssl_shutdown_timeout: Option<f64>,
    ) -> PyResult<Bound<'_, PyAny>> {
        profiling::scope!("PyLoop::start_tls");
        let locals = Self::task_locals(py, &slf)?;
        let transport: Py<PyStreamTransport> = transport.extract(py)?;
        pyo3_async_runtimes::async_std::future_into_py_with_locals(py, locals, async move {
            let upgraded = async_std::task::spawn_blocking(move || -> PyResult<Py<PyAny>> {
                Python::attach(|py| {
                    let sslcontext = sslcontext.clone_ref(py);
                    let protocol = protocol.clone_ref(py);
                    let transport = transport.clone_ref(py);
                    let server_hostname = server_hostname.as_ref().map(|value| value.clone_ref(py));
                    let client_tls = if server_side {
                        None
                    } else {
                        Some(client_tls_settings(
                            py,
                            sslcontext.bind(py),
                            server_hostname.as_ref().map(|value| value.bind(py)),
                            ssl_handshake_timeout,
                            ssl_shutdown_timeout,
                        )?)
                    };
                    let server_tls = if server_side {
                        Some(server_tls_settings(
                            py,
                            sslcontext.bind(py),
                            ssl_handshake_timeout,
                            ssl_shutdown_timeout,
                        )?)
                    } else {
                        None
                    };
                    let upgraded =
                        start_tls_transport(py, transport, protocol, client_tls, server_tls)?;
                    Ok(upgraded.into_any())
                })
            })
            .await
            .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
            Ok(upgraded)
        })
    }

    #[pyo3(signature=(fd, callback, *args))]
    fn add_reader(
        &self,
        py: Python<'_>,
        fd: &Bound<'_, PyAny>,
        callback: Py<PyAny>,
        args: &Bound<'_, PyTuple>,
    ) -> PyResult<()> {
        let raw_fd = fd_ops::fileobj_to_fd(py, fd)?;
        let (context, context_needs_run) = capture_context(py, None)?;
        self.core
            .state
            .lock()
            .expect("poisoned loop state")
            .reader_keepalive
            .insert(raw_fd, fd_ops::fileobj_keepalive(fd));
        self.core
            .send_command(LoopCommand::StartReader {
                fd: raw_fd,
                callback,
                args: args.clone().unbind(),
                context,
                context_needs_run,
            })
            .map_err(Self::map_loop_error)
    }

    fn remove_reader(&self, py: Python<'_>, fd: &Bound<'_, PyAny>) -> PyResult<bool> {
        let raw_fd = fd_ops::fileobj_to_fd(py, fd)?;
        let removed = self
            .core
            .state
            .lock()
            .expect("poisoned loop state")
            .reader_keepalive
            .remove(&raw_fd)
            .is_some();
        self.core
            .send_command(LoopCommand::StopReader(raw_fd))
            .map_err(Self::map_loop_error)?;
        Ok(removed)
    }

    #[pyo3(signature=(fd, callback, *args))]
    fn add_writer(
        &self,
        py: Python<'_>,
        fd: &Bound<'_, PyAny>,
        callback: Py<PyAny>,
        args: &Bound<'_, PyTuple>,
    ) -> PyResult<()> {
        let raw_fd = fd_ops::fileobj_to_fd(py, fd)?;
        let (context, context_needs_run) = capture_context(py, None)?;
        self.core
            .state
            .lock()
            .expect("poisoned loop state")
            .writer_keepalive
            .insert(raw_fd, fd_ops::fileobj_keepalive(fd));
        self.core
            .send_command(LoopCommand::StartWriter {
                fd: raw_fd,
                callback,
                args: args.clone().unbind(),
                context,
                context_needs_run,
            })
            .map_err(Self::map_loop_error)
    }

    fn remove_writer(&self, py: Python<'_>, fd: &Bound<'_, PyAny>) -> PyResult<bool> {
        let raw_fd = fd_ops::fileobj_to_fd(py, fd)?;
        let removed = self
            .core
            .state
            .lock()
            .expect("poisoned loop state")
            .writer_keepalive
            .remove(&raw_fd)
            .is_some();
        self.core
            .send_command(LoopCommand::StopWriter(raw_fd))
            .map_err(Self::map_loop_error)?;
        Ok(removed)
    }

    fn sock_recv(
        slf: Py<Self>,
        py: Python<'_>,
        sock: Py<PyAny>,
        nbytes: usize,
    ) -> PyResult<Bound<'_, PyAny>> {
        let locals = Self::task_locals(py, &slf)?;
        let fd = fd_ops::fileobj_to_fd(py, sock.bind(py))?;
        pyo3_async_runtimes::async_std::future_into_py_with_locals(py, locals, async move {
            loop {
                match Python::attach(|py| sock.call_method1(py, "recv", (nbytes,))) {
                    Ok(value) => return Ok(value),
                    Err(err) => {
                        let retry =
                            Python::attach(|py| fd_ops::is_retryable_socket_error(py, &err))?;
                        if !retry {
                            return Err(err);
                        }
                    }
                }
                fd_ops::wait_readable(fd).await?;
            }
        })
    }

    fn sock_recv_into(
        slf: Py<Self>,
        py: Python<'_>,
        sock: Py<PyAny>,
        buf: Py<PyAny>,
    ) -> PyResult<Bound<'_, PyAny>> {
        let locals = Self::task_locals(py, &slf)?;
        let fd = fd_ops::fileobj_to_fd(py, sock.bind(py))?;
        pyo3_async_runtimes::async_std::future_into_py_with_locals(py, locals, async move {
            loop {
                match Python::attach(|py| sock.call_method1(py, "recv_into", (buf.clone_ref(py),)))
                {
                    Ok(value) => return Ok(value),
                    Err(err) => {
                        let retry =
                            Python::attach(|py| fd_ops::is_retryable_socket_error(py, &err))?;
                        if !retry {
                            return Err(err);
                        }
                    }
                }
                fd_ops::wait_readable(fd).await?;
            }
        })
    }

    fn sock_sendall(
        slf: Py<Self>,
        py: Python<'_>,
        sock: Py<PyAny>,
        data: Py<PyAny>,
    ) -> PyResult<Bound<'_, PyAny>> {
        let locals = Self::task_locals(py, &slf)?;
        let fd = fd_ops::fileobj_to_fd(py, sock.bind(py))?;
        pyo3_async_runtimes::async_std::future_into_py_with_locals(py, locals, async move {
            let total = Python::attach(|py| data.bind(py).len())?;
            let mut sent = 0usize;

            while sent < total {
                let wrote = match Python::attach(|py| -> PyResult<usize> {
                    let chunk = data.bind(py).get_item(PySlice::new(
                        py,
                        sent as isize,
                        total as isize,
                        1,
                    ))?;
                    sock.call_method1(py, "send", (chunk,))?.extract(py)
                }) {
                    Ok(wrote) => wrote,
                    Err(err) => {
                        let retry =
                            Python::attach(|py| fd_ops::is_retryable_socket_error(py, &err))?;
                        if !retry {
                            return Err(err);
                        }
                        fd_ops::wait_writable(fd).await?;
                        continue;
                    }
                };
                sent += wrote;
                if sent < total {
                    fd_ops::wait_writable(fd).await?;
                }
            }

            Ok(Python::attach(|py| py.None()))
        })
    }

    fn sock_accept(slf: Py<Self>, py: Python<'_>, sock: Py<PyAny>) -> PyResult<Bound<'_, PyAny>> {
        let locals = Self::task_locals(py, &slf)?;
        let fd = fd_ops::fileobj_to_fd(py, sock.bind(py))?;
        pyo3_async_runtimes::async_std::future_into_py_with_locals(py, locals, async move {
            loop {
                match Python::attach(|py| -> PyResult<Py<PyAny>> {
                    let accepted = sock.call_method0(py, "accept")?;
                    let client = accepted.bind(py).get_item(0)?;
                    client.call_method1("setblocking", (false,))?;
                    Ok(accepted)
                }) {
                    Ok(value) => return Ok(value),
                    Err(err) => {
                        let retry =
                            Python::attach(|py| fd_ops::is_retryable_socket_error(py, &err))?;
                        if !retry {
                            return Err(err);
                        }
                    }
                }
                fd_ops::wait_readable(fd).await?;
            }
        })
    }

    fn sock_connect(
        slf: Py<Self>,
        py: Python<'_>,
        sock: Py<PyAny>,
        address: Py<PyAny>,
    ) -> PyResult<Bound<'_, PyAny>> {
        let locals = Self::task_locals(py, &slf)?;
        pyo3_async_runtimes::async_std::future_into_py_with_locals(py, locals, async move {
            connect_socket_to_address(sock, address).await?;
            Ok(Python::attach(|py| py.None()))
        })
    }

    #[pyo3(signature=(host, port, *, family=0, r#type=0, proto=0, flags=0))]
    #[expect(
        clippy::too_many_arguments,
        reason = "Mirrors asyncio loop.getaddrinfo()"
    )]
    fn getaddrinfo(
        slf: Py<Self>,
        py: Python<'_>,
        host: Option<Py<PyAny>>,
        port: Option<Py<PyAny>>,
        family: i32,
        r#type: i32,
        proto: i32,
        flags: i32,
    ) -> PyResult<Bound<'_, PyAny>> {
        let socket = py.import("socket")?;
        let host = host.unwrap_or_else(|| py.None());
        let port = port.unwrap_or_else(|| py.None());
        let run_args = PyTuple::new(
            py,
            [
                py.None(),
                socket.getattr("getaddrinfo")?.unbind(),
                host,
                port,
                family.into_pyobject(py)?.unbind().into(),
                r#type.into_pyobject(py)?.unbind().into(),
                proto.into_pyobject(py)?.unbind().into(),
                flags.into_pyobject(py)?.unbind().into(),
            ],
        )?;
        slf.call_method1(py, "run_in_executor", run_args)
            .map(|awaitable| awaitable.into_bound(py))
    }

    #[pyo3(signature=(sockaddr, flags=0))]
    fn getnameinfo(
        slf: Py<Self>,
        py: Python<'_>,
        sockaddr: Py<PyAny>,
        flags: i32,
    ) -> PyResult<Bound<'_, PyAny>> {
        let socket = py.import("socket")?;
        let run_args = PyTuple::new(
            py,
            [
                py.None(),
                socket.getattr("getnameinfo")?.unbind(),
                sockaddr,
                flags.into_pyobject(py)?.unbind().into(),
            ],
        )?;
        slf.call_method1(py, "run_in_executor", run_args)
            .map(|awaitable| awaitable.into_bound(py))
    }

    #[pyo3(signature=(executor, func, *args))]
    fn run_in_executor<'py>(
        slf: Py<Self>,
        py: Python<'py>,
        executor: Option<Py<PyAny>>,
        func: Py<PyAny>,
        args: &Bound<'py, PyTuple>,
    ) -> PyResult<Bound<'py, PyAny>> {
        let selected_executor = if let Some(executor) = executor {
            Some(executor)
        } else {
            let core = slf.borrow(py).core.clone();
            let state = core.state.lock().expect("poisoned loop state");
            if state.executor_shutdown_called {
                return Err(PyRuntimeError::new_err("Executor shutdown has been called"));
            }
            state
                .default_executor
                .as_ref()
                .map(|value| value.clone_ref(py))
        };

        if let Some(executor) = selected_executor {
            let mut submit_items = Vec::with_capacity(args.len() + 1);
            submit_items.push(func.clone_ref(py));
            submit_items.extend(args.iter().map(|item| item.unbind()));
            let submit_args = PyTuple::new(py, submit_items)?;
            let concurrent_future = executor.call_method1(py, "submit", submit_args)?;
            let asyncio = py.import("asyncio")?;
            let kwargs = PyDict::new(py);
            kwargs.set_item("loop", Self::as_py_any(py, &slf))?;
            return asyncio
                .getattr("wrap_future")?
                .call((concurrent_future,), Some(&kwargs));
        }

        let locals = Self::task_locals(py, &slf)?;
        let args = args.clone().unbind();
        pyo3_async_runtimes::async_std::future_into_py_with_locals(py, locals, async move {
            crate::blocking::run("rsloop-run-in-executor", move || {
                Python::attach(|py| func.call1(py, args.clone_ref(py)))
            })
            .await
            .map_err(PyRuntimeError::new_err)?
        })
    }

    #[pyo3(signature=(protocol_factory, cmd, *, stdin=None, stdout=None, stderr=None, universal_newlines=false, shell=true, bufsize=0, encoding=None, errors=None, text=None, **kwargs))]
    #[expect(
        clippy::too_many_arguments,
        reason = "Mirrors asyncio loop.subprocess_shell()"
    )]
    fn subprocess_shell<'py>(
        slf: Py<Self>,
        py: Python<'py>,
        protocol_factory: Py<PyAny>,
        cmd: Py<PyAny>,
        stdin: Option<Py<PyAny>>,
        stdout: Option<Py<PyAny>>,
        stderr: Option<Py<PyAny>>,
        universal_newlines: bool,
        shell: bool,
        bufsize: i32,
        encoding: Option<Py<PyAny>>,
        errors: Option<Py<PyAny>>,
        text: Option<bool>,
        kwargs: Option<Py<PyDict>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        if !shell {
            return Err(PyRuntimeError::new_err(
                "subprocess_shell() requires shell=True",
            ));
        }
        let _ = bufsize;

        let locals = Self::task_locals(py, &slf)?;
        let loop_obj = Self::as_py_any(py, &slf);
        let core = slf.borrow(py).core.clone();
        let (context, context_needs_run) = capture_context(py, None)?;
        let subprocess = py.import("asyncio.subprocess")?;
        let stdin = stdin.unwrap_or(subprocess.getattr("PIPE")?.unbind());
        let stdout = stdout.unwrap_or(subprocess.getattr("PIPE")?.unbind());
        let stderr = stderr.unwrap_or(subprocess.getattr("PIPE")?.unbind());
        let text_config = parse_process_text_config(
            py,
            universal_newlines,
            encoding.as_ref().map(|value| value.clone_ref(py)),
            errors.as_ref().map(|value| value.clone_ref(py)),
            text,
        )?;
        let stdin_spec = parse_process_stdio(py, &stdin, false)?;
        let stdout_spec = parse_process_stdio(py, &stdout, false)?;
        let stderr_spec = parse_process_stdio(py, &stderr, true)?;
        let cmd = cmd.clone_ref(py);
        let kwargs_owned = kwargs.map(|kwargs| kwargs.clone_ref(py));

        pyo3_async_runtimes::async_std::future_into_py_with_locals(py, locals, async move {
            let protocol = Python::attach(|py| {
                call_protocol_factory(
                    py,
                    &loop_obj,
                    &context,
                    context_needs_run,
                    &protocol_factory,
                )
            })?;
            if text_config.is_some()
                && Python::attach(|py| is_asyncio_subprocess_stream_protocol(py, &protocol))?
            {
                return Err(PyValueError::new_err(
                    "text mode is not supported with asyncio.create_subprocess_shell() in rust-impl yet",
                ));
            }

            let child = Python::attach(|py| -> PyResult<_> {
                let shell_cmd = cmd.bind(py).extract::<String>()?;
                #[cfg(unix)]
                let mut command = {
                    let mut command = Command::new("/bin/sh");
                    command.arg("-c");
                    command.arg(&shell_cmd);
                    command
                };
                #[cfg(windows)]
                let mut command = {
                    let mut command = Command::new(
                        std::env::var_os("COMSPEC").unwrap_or_else(|| "cmd.exe".into()),
                    );
                    command.raw_arg(format!(" /c \"{shell_cmd}\""));
                    command
                };
                let (stdout_override, stderr_override) =
                    apply_stdio(&mut command, stdin_spec, stdout_spec, stderr_spec)?;
                let spawn_config = apply_common_process_kwargs(
                    py,
                    &mut command,
                    kwargs_owned.as_ref().map(|kwargs| kwargs.bind(py)),
                )?;
                let child = command
                    .spawn()
                    .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
                Ok((child, spawn_config, stdout_override, stderr_override))
            })?;
            let (child, spawn_config, stdout_override, stderr_override) = child;

            let transport = Python::attach(|py| {
                spawn_process_transport(
                    py,
                    ProcessTransportParams {
                        loop_core: core.clone(),
                        loop_obj: loop_obj.clone_ref(py),
                        protocol: protocol.clone_ref(py),
                        context: context.clone_ref(py),
                        context_needs_run,
                        text_config: text_config.clone().or(spawn_config.text),
                        child,
                        stdout_override,
                        stderr_override,
                    },
                )
            })?;

            Python::attach(|py| {
                let result = PyTuple::new(py, [transport.into_any(), protocol.clone_ref(py)])?;
                Ok(result.unbind().into_any())
            })
        })
    }

    #[pyo3(signature=(protocol_factory, program, *args, stdin=None, stdout=None, stderr=None, universal_newlines=false, shell=false, bufsize=0, encoding=None, errors=None, text=None, **kwargs))]
    #[expect(
        clippy::too_many_arguments,
        reason = "Mirrors asyncio loop.subprocess_exec()"
    )]
    fn subprocess_exec<'py>(
        slf: Py<Self>,
        py: Python<'py>,
        protocol_factory: Py<PyAny>,
        program: Py<PyAny>,
        args: &Bound<'py, PyTuple>,
        stdin: Option<Py<PyAny>>,
        stdout: Option<Py<PyAny>>,
        stderr: Option<Py<PyAny>>,
        universal_newlines: bool,
        shell: bool,
        bufsize: i32,
        encoding: Option<Py<PyAny>>,
        errors: Option<Py<PyAny>>,
        text: Option<bool>,
        kwargs: Option<Py<PyDict>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        if shell {
            return Err(PyRuntimeError::new_err(
                "subprocess_exec() requires shell=False",
            ));
        }
        let _ = bufsize;

        let locals = Self::task_locals(py, &slf)?;
        let loop_obj = Self::as_py_any(py, &slf);
        let core = slf.borrow(py).core.clone();
        let (context, context_needs_run) = capture_context(py, None)?;
        let subprocess = py.import("asyncio.subprocess")?;
        let stdin = stdin.unwrap_or(subprocess.getattr("PIPE")?.unbind());
        let stdout = stdout.unwrap_or(subprocess.getattr("PIPE")?.unbind());
        let stderr = stderr.unwrap_or(subprocess.getattr("PIPE")?.unbind());
        let text_config = parse_process_text_config(
            py,
            universal_newlines,
            encoding.as_ref().map(|value| value.clone_ref(py)),
            errors.as_ref().map(|value| value.clone_ref(py)),
            text,
        )?;
        let stdin_spec = parse_process_stdio(py, &stdin, false)?;
        let stdout_spec = parse_process_stdio(py, &stdout, false)?;
        let stderr_spec = parse_process_stdio(py, &stderr, true)?;
        let program = program.clone_ref(py);
        let argv = args.clone().unbind();
        let kwargs_owned = kwargs.map(|kwargs| kwargs.clone_ref(py));

        pyo3_async_runtimes::async_std::future_into_py_with_locals(py, locals, async move {
            let protocol = Python::attach(|py| {
                call_protocol_factory(
                    py,
                    &loop_obj,
                    &context,
                    context_needs_run,
                    &protocol_factory,
                )
            })?;
            if text_config.is_some()
                && Python::attach(|py| is_asyncio_subprocess_stream_protocol(py, &protocol))?
            {
                return Err(PyValueError::new_err(
                    "text mode is not supported with asyncio.create_subprocess_exec() in rust-impl yet",
                ));
            }

            let child = Python::attach(|py| -> PyResult<_> {
                let mut command = Command::new(program.bind(py).extract::<String>()?);
                for arg in argv.bind(py).iter() {
                    command.arg(arg.extract::<String>()?);
                }
                let (stdout_override, stderr_override) =
                    apply_stdio(&mut command, stdin_spec, stdout_spec, stderr_spec)?;
                let spawn_config = apply_common_process_kwargs(
                    py,
                    &mut command,
                    kwargs_owned.as_ref().map(|kwargs| kwargs.bind(py)),
                )?;
                let child = command
                    .spawn()
                    .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
                Ok((child, spawn_config, stdout_override, stderr_override))
            })?;
            let (child, spawn_config, stdout_override, stderr_override) = child;

            let transport = Python::attach(|py| {
                spawn_process_transport(
                    py,
                    ProcessTransportParams {
                        loop_core: core.clone(),
                        loop_obj: loop_obj.clone_ref(py),
                        protocol: protocol.clone_ref(py),
                        context: context.clone_ref(py),
                        context_needs_run,
                        text_config: text_config.clone().or(spawn_config.text),
                        child,
                        stdout_override,
                        stderr_override,
                    },
                )
            })?;

            Python::attach(|py| {
                let result = PyTuple::new(py, [transport.into_any(), protocol.clone_ref(py)])?;
                Ok(result.unbind().into_any())
            })
        })
    }

    fn connect_read_pipe(
        slf: Py<Self>,
        py: Python<'_>,
        protocol_factory: Py<PyAny>,
        pipe: Py<PyAny>,
    ) -> PyResult<Bound<'_, PyAny>> {
        let locals = Self::task_locals(py, &slf)?;
        let loop_obj = Self::as_py_any(py, &slf);
        let core = slf.borrow(py).core.clone();
        let (context, context_needs_run) = capture_context(py, None)?;

        pyo3_async_runtimes::async_std::future_into_py_with_locals(py, locals, async move {
            let protocol = Python::attach(|py| {
                call_protocol_factory(
                    py,
                    &loop_obj,
                    &context,
                    context_needs_run,
                    &protocol_factory,
                )
            })?;
            let transport = Python::attach(|py| {
                let _ = pipe.call_method1(py, "setblocking", (false,));
                spawn_read_pipe_transport(
                    py,
                    core.clone(),
                    loop_obj.clone_ref(py),
                    protocol.clone_ref(py),
                    context.clone_ref(py),
                    context_needs_run,
                    pipe.clone_ref(py),
                )
            })?;
            Python::attach(|py| {
                let result = PyTuple::new(py, [transport.into_any(), protocol.clone_ref(py)])?;
                Ok(result.unbind().into_any())
            })
        })
    }

    fn connect_write_pipe(
        slf: Py<Self>,
        py: Python<'_>,
        protocol_factory: Py<PyAny>,
        pipe: Py<PyAny>,
    ) -> PyResult<Bound<'_, PyAny>> {
        let locals = Self::task_locals(py, &slf)?;
        let loop_obj = Self::as_py_any(py, &slf);
        let core = slf.borrow(py).core.clone();
        let (context, context_needs_run) = capture_context(py, None)?;

        pyo3_async_runtimes::async_std::future_into_py_with_locals(py, locals, async move {
            let protocol = Python::attach(|py| {
                call_protocol_factory(
                    py,
                    &loop_obj,
                    &context,
                    context_needs_run,
                    &protocol_factory,
                )
            })?;
            let transport = Python::attach(|py| {
                let _ = pipe.call_method1(py, "setblocking", (false,));
                spawn_write_pipe_transport(
                    py,
                    TransportSpawnContext {
                        loop_core: core.clone(),
                        loop_obj: loop_obj.clone_ref(py),
                        protocol: protocol.clone_ref(py),
                        context: context.clone_ref(py),
                        context_needs_run,
                    },
                    pipe.clone_ref(py),
                    None,
                )
            })?;
            Python::attach(|py| {
                let result = PyTuple::new(py, [transport.into_any(), protocol.clone_ref(py)])?;
                Ok(result.unbind().into_any())
            })
        })
    }

    #[pyo3(signature=(sig, callback, *args))]
    fn add_signal_handler(
        slf: Py<Self>,
        py: Python<'_>,
        sig: i32,
        callback: Py<PyAny>,
        args: &Bound<'_, PyTuple>,
    ) -> PyResult<()> {
        #[cfg(not(unix))]
        {
            let _ = (slf, py, sig, callback, args);
            return Err(Self::not_implemented("add_signal_handler"));
        }
        #[cfg(unix)]
        {
            let threading = py.import("threading")?;
            let current_thread = threading.getattr("current_thread")?.call0()?;
            let main_thread = threading.getattr("main_thread")?.call0()?;
            if !current_thread.is(&main_thread) {
                return Err(PyValueError::new_err(
                    "set_wakeup_fd only works in main thread of the main interpreter",
                ));
            }

            let loop_ref = slf.borrow(py);
            let core = loop_ref.core.clone();
            drop(loop_ref);

            if sig == libc::SIGCHLD {
                return Err(PyRuntimeError::new_err(
                    "SIGCHLD is reserved for subprocess handling",
                ));
            }
            let (context, context_needs_run) = capture_context(py, None)?;

            let newly_installed = {
                let mut state = core.state.lock().expect("poisoned loop state");
                let newly_installed = !state.signal_handlers.contains_key(&sig);
                state.signal_handlers.insert(
                    sig,
                    SignalHandlerTemplate {
                        callback,
                        args: args.clone().unbind(),
                        context,
                        context_needs_run,
                    },
                );
                newly_installed
            };

            if newly_installed {
                core.send_command(LoopCommand::StartSignalWatcher(sig))
                    .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
            }
            Ok(())
        }
    }

    fn remove_signal_handler(slf: Py<Self>, py: Python<'_>, sig: i32) -> PyResult<bool> {
        let loop_ref = slf.borrow(py);
        let core = loop_ref.core.clone();
        drop(loop_ref);

        let removed = {
            let mut state = core.state.lock().expect("poisoned loop state");
            let removed = state.signal_handlers.remove(&sig).is_some();
            if removed {
                state.previous_signal_handlers.remove(&sig);
            }
            removed
        };
        if removed {
            core.send_command(LoopCommand::StopSignalWatcher(sig))
                .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
        }
        Ok(removed)
    }

    fn shutdown_asyncgens(slf: Py<Self>, py: Python<'_>) -> PyResult<Bound<'_, PyAny>> {
        {
            let core = slf.borrow(py).core.clone();
            core.state
                .lock()
                .expect("poisoned loop state")
                .asyncgens_shutdown_called = true;
        }

        let locals = Self::task_locals(py, &slf)?;
        pyo3_async_runtimes::async_std::future_into_py_with_locals(py, locals, async move {
            Ok(Python::attach(|py| py.None()))
        })
    }

    #[pyo3(signature=(timeout=None))]
    fn shutdown_default_executor(
        slf: Py<Self>,
        py: Python<'_>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'_, PyAny>> {
        let executor = {
            let core = slf.borrow(py).core.clone();
            let mut state = core.state.lock().expect("poisoned loop state");
            state.executor_shutdown_called = true;
            state.default_executor.take()
        };
        let executor_nowait = if timeout.is_some() {
            executor.as_ref().map(|value| value.clone_ref(py))
        } else {
            None
        };

        let locals = Self::task_locals(py, &slf)?;
        pyo3_async_runtimes::async_std::future_into_py_with_locals(py, locals, async move {
            if let Some(executor) = executor {
                let wait_forever =
                    timeout.is_none() || timeout.is_some_and(|value| value.is_infinite());
                if wait_forever {
                    crate::blocking::run("rsloop-shutdown-default-executor", move || {
                        Python::attach(|py| -> PyResult<()> {
                            executor.call_method1(py, "shutdown", (true,))?;
                            Ok(())
                        })
                    })
                    .await
                    .map_err(PyRuntimeError::new_err)??;
                } else {
                    let timeout_value = timeout.expect("timeout checked above");
                    let (tx, rx) = futures::channel::oneshot::channel();
                    std::thread::Builder::new()
                        .name("rsloop-shutdown-default-executor".to_owned())
                        .spawn(move || {
                            let result = Python::attach(|py| -> PyResult<()> {
                                executor.call_method1(py, "shutdown", (true,))?;
                                Ok(())
                            });
                            let _ = tx.send(result);
                        })
                        .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;

                    let timed_out = if timeout_value.is_finite() && timeout_value > 0.0 {
                        match async_std::future::timeout(
                            Duration::from_secs_f64(timeout_value),
                            async move {
                                rx.await.map_err(|_| {
                                    PyRuntimeError::new_err(
                                        "default executor shutdown worker dropped",
                                    )
                                })?
                            },
                        )
                        .await
                        {
                            Ok(result) => {
                                result?;
                                false
                            }
                            Err(_) => true,
                        }
                    } else {
                        true
                    };

                    if timed_out {
                        Python::attach(|py| warn_default_executor_timeout(py, timeout_value))?;
                        if let Some(executor_nowait) = executor_nowait {
                            crate::blocking::run(
                                "rsloop-shutdown-default-executor-nowait",
                                move || {
                                    Python::attach(|py| -> PyResult<()> {
                                        executor_nowait.call_method1(py, "shutdown", (false,))?;
                                        Ok(())
                                    })
                                },
                            )
                            .await
                            .map_err(PyRuntimeError::new_err)??;
                        }
                    }
                }
            }

            Ok(Python::attach(|py| py.None()))
        })
    }
}

#[pyfunction]
pub fn new_event_loop(py: Python<'_>) -> PyResult<Py<PyLoop>> {
    Py::new(py, PyLoop::new())
}

#[pyfunction]
pub fn future_done_stop(loop_obj: &Bound<'_, PyAny>, future: &Bound<'_, PyAny>) -> PyResult<()> {
    if !future.call_method0("cancelled")?.extract::<bool>()? {
        let exc = future.call_method0("exception")?;
        if !exc.is_none()
            && (exc.is_instance_of::<pyo3::exceptions::PySystemExit>()
                || exc.is_instance_of::<pyo3::exceptions::PyKeyboardInterrupt>())
        {
            return Ok(());
        }
    }

    loop_obj.call_method0("stop")?;
    Ok(())
}

#[pyfunction]
pub fn signal_bridge(
    py: Python<'_>,
    loop_obj: &Bound<'_, PyAny>,
    callback: Py<PyAny>,
    args: Py<PyTuple>,
    context: Py<PyAny>,
    _signum: i32,
    _frame: Py<PyAny>,
) -> PyResult<()> {
    let mut call_items = Vec::with_capacity(args.bind(py).len() + 1);
    call_items.push(callback);
    call_items.extend(args.bind(py).iter().map(|item| item.unbind()));
    let call_args = PyTuple::new(py, call_items)?;
    let kwargs = PyDict::new(py);
    kwargs.set_item("context", context)?;
    loop_obj.call_method("call_soon_threadsafe", call_args, Some(&kwargs))?;
    Ok(())
}
