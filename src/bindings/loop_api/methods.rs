//! The loop's Python API surface.
//!
//! `PyO3` allows one `#[pymethods]` block per class unless the
//! `multiple-pymethods` feature is enabled, so every method the loop exposes is
//! declared here. That keeps the signatures — which is what `asyncio`
//! compatibility is judged against — readable as a single list; anything longer
//! than a few lines lives in the topic module named at the call.

use std::time::Duration;

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyTuple};

use super::connections::{self, CreateConnectionParams, CreateUnixConnectionParams};
use super::executor::{self, AddrInfoRequest};
use super::process_spawn::{
    self, SubprocessParams, exec_command, parse_process_text_config, shell_command,
};
use super::process_stdio::{ProcessStdioSpecs, default_stdio_pipe};
use super::servers::{self, CreateServerParams, CreateUnixServerParams};
use super::sockets::TcpServerSocketOptions;
use super::tasks::{self, TaskOptions};
use super::tls_params::TlsParams;
use super::{
    MAX_TIMER_DELAY_SECS, PyLoop, asyncgens, lifecycle, pipes, signals, sock_ops, watchers,
};
use crate::engine::{CallbackKind, LoopCore, PyTimerHandle};

#[pymethods]
impl PyLoop {
    #[new]
    pub(super) fn new() -> Self {
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
        // Clamp so math.inf / oversized delays never panic
        // Duration::from_secs_f64 (issue #48); negatives fire ASAP.
        if delay.is_nan() {
            return Err(PyValueError::new_err("delay cannot be NaN"));
        }
        let delay = delay.clamp(0.0, MAX_TIMER_DELAY_SECS);
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
        // call_later clamps negatives and rejects NaN; don't mask either here.
        self.call_later(py, when - self.time(), callback, args, context)
    }

    fn time(&self) -> f64 {
        self.core.time()
    }

    fn stop(&self) -> PyResult<()> {
        self.core.schedule_stop().map_err(Self::map_loop_error)
    }

    fn close(&self, py: Python<'_>) -> PyResult<()> {
        lifecycle::close(self, py)
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

    fn run_forever(slf: Py<Self>, py: Python<'_>) -> PyResult<()> {
        lifecycle::run_forever(slf, py)
    }

    fn run_until_complete(slf: Py<Self>, py: Python<'_>, future: Py<PyAny>) -> PyResult<Py<PyAny>> {
        lifecycle::run_until_complete(slf, py, future)
    }

    fn create_future(slf: Py<Self>, py: Python<'_>) -> PyResult<Py<PyAny>> {
        tasks::create_future(slf, py)
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
        tasks::create_task(
            slf,
            py,
            coro,
            TaskOptions {
                name,
                context,
                eager_start,
                kwargs,
            },
        )
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

    fn get_task_factory(&self, py: Python<'_>) -> Option<Py<PyAny>> {
        self.core
            .state
            .lock()
            .expect("poisoned loop state")
            .task_factory
            .as_ref()
            .map(|factory| factory.clone_ref(py))
    }

    fn default_exception_handler(&self, py: Python<'_>, context: Py<PyAny>) -> PyResult<()> {
        self.core.default_exception_handler(py, context)
    }

    fn get_exception_handler(&self, py: Python<'_>) -> Option<Py<PyAny>> {
        self.core
            .state
            .lock()
            .expect("poisoned loop state")
            .exception_handler
            .as_ref()
            .map(|handler| handler.clone_ref(py))
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

    #[getter]
    fn slow_callback_duration(&self) -> f64 {
        self.core
            .state
            .lock()
            .expect("poisoned loop state")
            .slow_callback_duration
    }

    #[setter(slow_callback_duration)]
    fn set_slow_callback_duration(&self, value: f64) {
        self.core
            .state
            .lock()
            .expect("poisoned loop state")
            .slow_callback_duration = value;
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
        servers::create_server(
            slf,
            py,
            CreateServerParams {
                protocol_factory,
                host,
                port,
                sock,
                tls: TlsParams::without_hostname(ssl, ssl_handshake_timeout, ssl_shutdown_timeout),
                socket_options: TcpServerSocketOptions {
                    family,
                    flags,
                    backlog,
                    reuse_address,
                    reuse_port,
                    keep_alive,
                },
                start_serving,
            },
        )
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
        let _ = (happy_eyeballs_delay, interleave, all_errors);
        connections::create_connection(
            slf,
            py,
            CreateConnectionParams {
                protocol_factory,
                host,
                port,
                sock,
                local_addr,
                family,
                proto,
                flags,
                tls: TlsParams {
                    ssl,
                    server_hostname,
                    handshake_timeout: ssl_handshake_timeout,
                    shutdown_timeout: ssl_shutdown_timeout,
                },
            },
        )
    }

    #[pyo3(signature=(protocol_factory, sock, *, ssl=None, server_hostname=None, ssl_handshake_timeout=None, ssl_shutdown_timeout=None))]
    #[allow(clippy::too_many_arguments)]
    fn _create_connection_transport(
        slf: Py<Self>,
        py: Python<'_>,
        protocol_factory: Py<PyAny>,
        sock: Py<PyAny>,
        ssl: Option<Py<PyAny>>,
        server_hostname: Option<Py<PyAny>>,
        ssl_handshake_timeout: Option<f64>,
        ssl_shutdown_timeout: Option<f64>,
    ) -> PyResult<Py<PyAny>> {
        connections::create_connection_transport(
            slf,
            py,
            protocol_factory,
            sock,
            TlsParams {
                ssl,
                server_hostname,
                handshake_timeout: ssl_handshake_timeout,
                shutdown_timeout: ssl_shutdown_timeout,
            },
        )
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
        servers::create_unix_server(
            slf,
            py,
            CreateUnixServerParams {
                protocol_factory,
                path,
                sock,
                backlog,
                tls: TlsParams::without_hostname(ssl, ssl_handshake_timeout, ssl_shutdown_timeout),
                start_serving,
                cleanup_socket,
            },
        )
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
        connections::create_unix_connection(
            slf,
            py,
            CreateUnixConnectionParams {
                protocol_factory,
                path,
                sock,
                tls: TlsParams {
                    ssl,
                    server_hostname,
                    handshake_timeout: ssl_handshake_timeout,
                    shutdown_timeout: ssl_shutdown_timeout,
                },
            },
        )
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
        connections::connect_accepted_socket(
            slf,
            py,
            protocol_factory,
            sock,
            TlsParams::without_hostname(ssl, ssl_handshake_timeout, ssl_shutdown_timeout),
        )
    }

    #[pyo3(signature=(transport, protocol, sslcontext, *, server_side=false, server_hostname=None, ssl_handshake_timeout=None, ssl_shutdown_timeout=None))]
    #[expect(
        clippy::too_many_arguments,
        reason = "Mirrors asyncio loop.start_tls()"
    )]
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
        connections::start_tls(
            slf,
            py,
            transport,
            protocol,
            TlsParams {
                ssl: Some(sslcontext),
                server_hostname,
                handshake_timeout: ssl_handshake_timeout,
                shutdown_timeout: ssl_shutdown_timeout,
            },
            server_side,
        )
    }

    #[pyo3(signature=(fd, callback, *args))]
    fn add_reader(
        &self,
        py: Python<'_>,
        fd: &Bound<'_, PyAny>,
        callback: Py<PyAny>,
        args: &Bound<'_, PyTuple>,
    ) -> PyResult<()> {
        watchers::add_reader(self, py, fd, callback, args)
    }

    fn remove_reader(&self, py: Python<'_>, fd: &Bound<'_, PyAny>) -> PyResult<bool> {
        watchers::remove_reader(self, py, fd)
    }

    #[pyo3(signature=(fd, callback, *args))]
    fn add_writer(
        &self,
        py: Python<'_>,
        fd: &Bound<'_, PyAny>,
        callback: Py<PyAny>,
        args: &Bound<'_, PyTuple>,
    ) -> PyResult<()> {
        watchers::add_writer(self, py, fd, callback, args)
    }

    fn remove_writer(&self, py: Python<'_>, fd: &Bound<'_, PyAny>) -> PyResult<bool> {
        watchers::remove_writer(self, py, fd)
    }

    fn sock_recv(
        slf: Py<Self>,
        py: Python<'_>,
        sock: Py<PyAny>,
        nbytes: usize,
    ) -> PyResult<Bound<'_, PyAny>> {
        sock_ops::sock_recv(slf, py, sock, nbytes)
    }

    fn sock_recv_into(
        slf: Py<Self>,
        py: Python<'_>,
        sock: Py<PyAny>,
        buf: Py<PyAny>,
    ) -> PyResult<Bound<'_, PyAny>> {
        sock_ops::sock_recv_into(slf, py, sock, buf)
    }

    fn sock_sendall(
        slf: Py<Self>,
        py: Python<'_>,
        sock: Py<PyAny>,
        data: Py<PyAny>,
    ) -> PyResult<Bound<'_, PyAny>> {
        sock_ops::sock_sendall(slf, py, sock, data)
    }

    fn sock_accept(slf: Py<Self>, py: Python<'_>, sock: Py<PyAny>) -> PyResult<Bound<'_, PyAny>> {
        sock_ops::sock_accept(slf, py, sock)
    }

    fn sock_connect(
        slf: Py<Self>,
        py: Python<'_>,
        sock: Py<PyAny>,
        address: Py<PyAny>,
    ) -> PyResult<Bound<'_, PyAny>> {
        sock_ops::sock_connect(slf, py, sock, address)
    }

    fn _sock_connect_fast<'py>(
        slf: Py<Self>,
        py: Python<'py>,
        sock: Py<PyAny>,
        address: Py<PyAny>,
    ) -> PyResult<Bound<'py, PyAny>> {
        sock_ops::sock_connect_fast(slf, py, sock, address)
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
        executor::getaddrinfo(
            slf,
            py,
            AddrInfoRequest {
                host,
                port,
                family,
                sock_type: r#type,
                proto,
                flags,
            },
        )
    }

    #[pyo3(signature=(sockaddr, flags=0))]
    fn getnameinfo(
        slf: Py<Self>,
        py: Python<'_>,
        sockaddr: Py<PyAny>,
        flags: i32,
    ) -> PyResult<Bound<'_, PyAny>> {
        executor::getnameinfo(slf, py, sockaddr, flags)
    }

    #[pyo3(signature=(executor, func, *args))]
    fn run_in_executor<'py>(
        slf: Py<Self>,
        py: Python<'py>,
        executor: Option<Py<PyAny>>,
        func: Py<PyAny>,
        args: &Bound<'py, PyTuple>,
    ) -> PyResult<Bound<'py, PyAny>> {
        super::executor::run_in_executor(slf, py, executor, func, args)
    }

    #[pyo3(signature=(protocol_factory, cmd, *, stdin=default_stdio_pipe(), stdout=default_stdio_pipe(), stderr=default_stdio_pipe(), universal_newlines=false, shell=true, bufsize=0, encoding=None, errors=None, text=None, **kwargs))]
    #[expect(
        clippy::too_many_arguments,
        reason = "Mirrors asyncio loop.subprocess_shell()"
    )]
    fn subprocess_shell<'py>(
        slf: Py<Self>,
        py: Python<'py>,
        protocol_factory: Py<PyAny>,
        cmd: Py<PyAny>,
        stdin: Py<PyAny>,
        stdout: Py<PyAny>,
        stderr: Py<PyAny>,
        universal_newlines: bool,
        shell: bool,
        bufsize: i32,
        encoding: Option<Py<PyAny>>,
        errors: Option<Py<PyAny>>,
        text: Option<bool>,
        kwargs: Option<Py<PyDict>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        if !shell {
            return Err(PyValueError::new_err(
                "subprocess_shell() requires shell=True",
            ));
        }
        let _ = bufsize;

        // Text config is validated before the stdio markers, matching the order
        // callers see when both are wrong.
        let text_config =
            parse_process_text_config(py, universal_newlines, encoding, errors, text)?;
        let params = SubprocessParams {
            protocol_factory,
            specs: ProcessStdioSpecs::parse(py, &stdin, &stdout, &stderr)?,
            text_config,
            kwargs,
            api_name: "create_subprocess_shell",
        };
        process_spawn::spawn_subprocess(&slf, py, params, move |py| shell_command(py, &cmd))
    }

    #[pyo3(signature=(protocol_factory, program, *args, stdin=default_stdio_pipe(), stdout=default_stdio_pipe(), stderr=default_stdio_pipe(), universal_newlines=false, shell=false, bufsize=0, encoding=None, errors=None, text=None, **kwargs))]
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
        stdin: Py<PyAny>,
        stdout: Py<PyAny>,
        stderr: Py<PyAny>,
        universal_newlines: bool,
        shell: bool,
        bufsize: i32,
        encoding: Option<Py<PyAny>>,
        errors: Option<Py<PyAny>>,
        text: Option<bool>,
        kwargs: Option<Py<PyDict>>,
    ) -> PyResult<Bound<'py, PyAny>> {
        if shell {
            return Err(PyValueError::new_err(
                "subprocess_exec() requires shell=False",
            ));
        }
        let _ = bufsize;

        let text_config =
            parse_process_text_config(py, universal_newlines, encoding, errors, text)?;
        let params = SubprocessParams {
            protocol_factory,
            specs: ProcessStdioSpecs::parse(py, &stdin, &stdout, &stderr)?,
            text_config,
            kwargs,
            api_name: "create_subprocess_exec",
        };
        let argv = args.clone().unbind();
        process_spawn::spawn_subprocess(&slf, py, params, move |py| {
            exec_command(py, &program, &argv)
        })
    }

    fn connect_read_pipe(
        slf: Py<Self>,
        py: Python<'_>,
        protocol_factory: Py<PyAny>,
        pipe: Py<PyAny>,
    ) -> PyResult<Bound<'_, PyAny>> {
        pipes::connect_read_pipe(slf, py, protocol_factory, pipe)
    }

    fn connect_write_pipe(
        slf: Py<Self>,
        py: Python<'_>,
        protocol_factory: Py<PyAny>,
        pipe: Py<PyAny>,
    ) -> PyResult<Bound<'_, PyAny>> {
        pipes::connect_write_pipe(slf, py, protocol_factory, pipe)
    }

    #[pyo3(signature=(sig, callback, *args))]
    fn add_signal_handler(
        slf: Py<Self>,
        py: Python<'_>,
        sig: i32,
        callback: Py<PyAny>,
        args: &Bound<'_, PyTuple>,
    ) -> PyResult<()> {
        signals::add_signal_handler(slf, py, sig, callback, args)
    }

    fn remove_signal_handler(slf: Py<Self>, py: Python<'_>, sig: i32) -> PyResult<bool> {
        signals::remove_signal_handler(slf, py, sig)
    }

    fn shutdown_asyncgens(slf: Py<Self>, py: Python<'_>) -> PyResult<Bound<'_, PyAny>> {
        asyncgens::shutdown_asyncgens(slf, py)
    }

    #[pyo3(signature=(timeout=None))]
    fn shutdown_default_executor(
        slf: Py<Self>,
        py: Python<'_>,
        timeout: Option<f64>,
    ) -> PyResult<Bound<'_, PyAny>> {
        executor::shutdown_default_executor(slf, py, timeout)
    }
}
