//! `create_server` / `create_unix_server`.
//!
//! Both resolve their listening sockets through Python's `socket` module (so an
//! explicitly supplied `sock` and a host/port pair take the same path), hand the
//! descriptors to the Rust listeners, and only then start accepting — deferred
//! when the caller passes `start_serving=False`.

use pyo3::prelude::*;

use super::PyLoop;
#[cfg(unix)]
use super::sockets::build_unix_server_socket;
use super::sockets::{
    TcpServerSocketOptions, build_tcp_server_sockets, listener_sources_from_sockets,
};
use super::spawn_env::LoopSpawnEnv;
use super::tls_params::TlsParams;
use crate::transport::stream::{PyServer, ServerCreateParams, create_server as create_py_server};

pub(super) struct CreateServerParams {
    pub(super) protocol_factory: Py<PyAny>,
    pub(super) host: Option<Py<PyAny>>,
    pub(super) port: Option<Py<PyAny>>,
    pub(super) sock: Option<Py<PyAny>>,
    pub(super) tls: TlsParams,
    pub(super) socket_options: TcpServerSocketOptions,
    pub(super) start_serving: bool,
}

pub(super) struct CreateUnixServerParams {
    pub(super) protocol_factory: Py<PyAny>,
    pub(super) path: Option<Py<PyAny>>,
    pub(super) sock: Option<Py<PyAny>>,
    pub(super) backlog: i32,
    pub(super) tls: TlsParams,
    pub(super) start_serving: bool,
    pub(super) cleanup_socket: bool,
}

pub(super) fn create_server<'py>(
    slf: Py<PyLoop>,
    py: Python<'py>,
    params: CreateServerParams,
) -> PyResult<Bound<'py, PyAny>> {
    let CreateServerParams {
        protocol_factory,
        host,
        port,
        sock,
        tls,
        socket_options,
        start_serving,
    } = params;
    tls.validate()?;
    let tls_settings = tls.shared_server_settings(py)?;

    let locals = PyLoop::task_locals(py, &slf)?;
    let env = LoopSpawnEnv::capture(py, &slf)?;
    let backlog = socket_options.backlog;

    pyo3_async_runtimes::async_std::future_into_py_with_locals(py, locals, async move {
        let sockets = Python::attach(|py| -> PyResult<Vec<Py<PyAny>>> {
            if let Some(sock) = &sock {
                sock.call_method1(py, "listen", (backlog,))?;
                sock.call_method1(py, "setblocking", (false,))?;
                return Ok(vec![sock.clone_ref(py)]);
            }
            build_tcp_server_sockets(py, host, port, socket_options)
        })?;

        let server = Python::attach(|py| -> PyResult<Py<PyServer>> {
            let listeners = listener_sources_from_sockets(py, &sockets)?;
            let server_sockets = sockets
                .iter()
                .map(|socket| socket.clone_ref(py))
                .collect::<Vec<_>>();
            let server = create_py_server(
                py,
                ServerCreateParams::new(
                    env.spawn_context(py, &protocol_factory),
                    server_sockets,
                    listeners,
                )
                .with_tls(tls_settings),
            )?;
            if start_serving {
                server.borrow(py).core.spawn_accept_tasks();
            }
            Ok(server)
        })?;

        Ok(Python::attach(|py| server.into_any().clone_ref(py)))
    })
}

pub(super) fn create_unix_server<'py>(
    slf: Py<PyLoop>,
    py: Python<'py>,
    params: CreateUnixServerParams,
) -> PyResult<Bound<'py, PyAny>> {
    let CreateUnixServerParams {
        protocol_factory,
        path,
        sock,
        backlog,
        tls,
        start_serving,
        cleanup_socket,
    } = params;
    tls.validate()?;
    #[cfg(not(unix))]
    {
        let _ = (
            slf,
            py,
            protocol_factory,
            path,
            sock,
            backlog,
            start_serving,
            cleanup_socket,
        );
        Err(PyLoop::not_implemented("create_unix_server"))
    }
    #[cfg(unix)]
    {
        let tls_settings = tls.shared_server_settings(py)?;

        let locals = PyLoop::task_locals(py, &slf)?;
        let env = LoopSpawnEnv::capture(py, &slf)?;

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
                    ServerCreateParams::new(
                        env.spawn_context(py, &protocol_factory),
                        sockets,
                        listeners,
                    )
                    .with_cleanup_path(cleanup_path)
                    .with_tls(tls_settings),
                )?;
                if start_serving {
                    server.borrow(py).core.spawn_accept_tasks();
                }
                Ok(server)
            })?;

            Ok(Python::attach(|py| server.into_any().clone_ref(py)))
        })
    }
}
