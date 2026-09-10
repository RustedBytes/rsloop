//! Outgoing connections, accepted sockets, and TLS upgrades.

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;

use super::PyLoop;
use super::socket_connect::connect_socket_to_address;
use super::sockets::{build_stream_socket, resolve_stream_addrinfos};
use super::spawn_env::{LoopSpawnEnv, transport_protocol_pair};
use super::tls_params::TlsParams;
use crate::transport::stream::{
    PyStreamTransport, prepare_start_tls_transport, start_tls_transport, transport_from_socket,
    transport_from_socket_server_tls, transport_from_socket_tls,
};

pub(super) struct CreateConnectionParams {
    pub(super) protocol_factory: Py<PyAny>,
    pub(super) host: Option<Py<PyAny>>,
    pub(super) port: Option<Py<PyAny>>,
    pub(super) sock: Option<Py<PyAny>>,
    pub(super) local_addr: Option<Py<PyAny>>,
    pub(super) family: i32,
    pub(super) proto: i32,
    pub(super) flags: i32,
    pub(super) tls: TlsParams,
}

pub(super) struct CreateUnixConnectionParams {
    pub(super) protocol_factory: Py<PyAny>,
    pub(super) path: Option<Py<PyAny>>,
    pub(super) sock: Option<Py<PyAny>>,
    pub(super) tls: TlsParams,
}

pub(super) fn create_connection<'py>(
    slf: Py<PyLoop>,
    py: Python<'py>,
    params: CreateConnectionParams,
) -> PyResult<Bound<'py, PyAny>> {
    let CreateConnectionParams {
        protocol_factory,
        host,
        port,
        sock,
        local_addr,
        family,
        proto,
        flags,
        tls,
    } = params;
    tls.validate()?;

    let locals = PyLoop::task_locals(py, &slf)?;
    let env = LoopSpawnEnv::capture(py, &slf)?;

    pyo3_async_runtimes::async_std::future_into_py_with_locals(py, locals, async move {
        let protocol = Python::attach(|py| env.call_protocol_factory(py, &protocol_factory))?;

        let socket_obj = if let Some(sock) = sock {
            Python::attach(|py| -> PyResult<Py<PyAny>> {
                sock.call_method1(py, "setblocking", (false,))?;
                Ok(sock.clone_ref(py))
            })?
        } else {
            connect_first_reachable_address(host, port, local_addr, family, proto, flags).await?
        };

        finish_client_connection(&env, &protocol, socket_obj, &tls).await
    })
}

/// Walks the resolved addresses in order, returning the first socket that
/// connects and reporting the last failure when none do.
async fn connect_first_reachable_address(
    host: Option<Py<PyAny>>,
    port: Option<Py<PyAny>>,
    local_addr: Option<Py<PyAny>>,
    family: i32,
    proto: i32,
    flags: i32,
) -> PyResult<Py<PyAny>> {
    let addrinfos =
        Python::attach(|py| resolve_stream_addrinfos(py, host, port, family, proto, flags))?;
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
        last_error.unwrap_or_else(|| PyRuntimeError::new_err("failed to connect socket"))
    })
}

/// Wraps a connected socket in a transport and returns the `(transport,
/// protocol)` pair. The TLS handshake runs on a blocking worker because it can
/// block on peer I/O.
async fn finish_client_connection(
    env: &LoopSpawnEnv,
    protocol: &Py<PyAny>,
    socket_obj: Py<PyAny>,
    tls: &TlsParams,
) -> PyResult<Py<PyAny>> {
    let transport = if tls.is_enabled() {
        let (spawn_context, settings) = Python::attach(|py| {
            let settings = tls
                .client_settings(py)?
                .expect("client settings exist when ssl is set");
            Ok::<_, PyErr>((env.spawn_context(py, protocol), settings))
        })?;
        async_std::task::spawn_blocking(move || {
            Python::attach(|py| transport_from_socket_tls(py, spawn_context, socket_obj, settings))
        })
        .await?
    } else {
        Python::attach(|py| transport_from_socket(py, env.spawn_context(py, protocol), socket_obj))?
    };

    Python::attach(|py| transport_protocol_pair(py, transport.into_any(), protocol))
}

pub(super) fn create_connection_transport(
    slf: Py<PyLoop>,
    py: Python<'_>,
    protocol_factory: Py<PyAny>,
    sock: Py<PyAny>,
    tls: TlsParams,
) -> PyResult<Py<PyAny>> {
    tls.validate()?;

    let env = LoopSpawnEnv::capture(py, &slf)?;
    let protocol = env.call_protocol_factory(py, &protocol_factory)?;
    sock.call_method1(py, "setblocking", (false,))?;
    let socket_obj = sock.clone_ref(py);
    let transport = if let Some(settings) = tls.client_settings(py)? {
        transport_from_socket_tls(py, env.spawn_context(py, &protocol), socket_obj, settings)
    } else {
        transport_from_socket(py, env.spawn_context(py, &protocol), socket_obj)
    }?;

    transport_protocol_pair(py, transport.into_any(), &protocol)
}

pub(super) fn create_unix_connection<'py>(
    slf: Py<PyLoop>,
    py: Python<'py>,
    params: CreateUnixConnectionParams,
) -> PyResult<Bound<'py, PyAny>> {
    let CreateUnixConnectionParams {
        protocol_factory,
        path,
        sock,
        tls,
    } = params;
    tls.validate()?;
    #[cfg(not(unix))]
    {
        let _ = (slf, py, protocol_factory, path, sock);
        Err(PyLoop::not_implemented("create_unix_connection"))
    }
    #[cfg(unix)]
    {
        let locals = PyLoop::task_locals(py, &slf)?;
        let env = LoopSpawnEnv::capture(py, &slf)?;

        pyo3_async_runtimes::async_std::future_into_py_with_locals(py, locals, async move {
            let protocol = Python::attach(|py| env.call_protocol_factory(py, &protocol_factory))?;

            let socket_obj = if let Some(sock) = sock {
                Python::attach(|py| -> PyResult<Py<PyAny>> {
                    sock.call_method1(py, "setblocking", (false,))?;
                    Ok(sock.clone_ref(py))
                })?
            } else {
                let socket_obj = Python::attach(super::sockets::build_unix_client_socket)?;
                let address = path.ok_or_else(|| {
                    PyRuntimeError::new_err("path is required when sock is not provided")
                })?;
                let socket_for_connect = Python::attach(|py| socket_obj.clone_ref(py));
                connect_socket_to_address(socket_for_connect, address).await?;
                socket_obj
            };

            finish_client_connection(&env, &protocol, socket_obj, &tls).await
        })
    }
}

pub(super) fn connect_accepted_socket<'py>(
    slf: Py<PyLoop>,
    py: Python<'py>,
    protocol_factory: Py<PyAny>,
    sock: Py<PyAny>,
    tls: TlsParams,
) -> PyResult<Bound<'py, PyAny>> {
    tls.validate()?;

    let locals = PyLoop::task_locals(py, &slf)?;
    let env = LoopSpawnEnv::capture(py, &slf)?;

    pyo3_async_runtimes::async_std::future_into_py_with_locals(py, locals, async move {
        let protocol = Python::attach(|py| env.call_protocol_factory(py, &protocol_factory))?;
        let socket_obj = Python::attach(|py| -> PyResult<Py<PyAny>> {
            sock.call_method1(py, "setblocking", (false,))?;
            Ok(sock.clone_ref(py))
        })?;
        let transport = Python::attach(|py| {
            if let Some(settings) = tls.server_settings(py)? {
                transport_from_socket_server_tls(
                    py,
                    env.spawn_context(py, &protocol),
                    socket_obj,
                    settings,
                )
            } else {
                transport_from_socket(py, env.spawn_context(py, &protocol), socket_obj)
            }
        })?;
        Python::attach(|py| transport_protocol_pair(py, transport.into_any(), &protocol))
    })
}

pub(super) fn start_tls<'py>(
    slf: Py<PyLoop>,
    py: Python<'py>,
    transport: Py<PyAny>,
    protocol: Py<PyAny>,
    tls: TlsParams,
    server_side: bool,
) -> PyResult<Bound<'py, PyAny>> {
    let locals = PyLoop::task_locals(py, &slf)?;
    let locals_for_barrier = locals.clone();
    let transport: Py<PyStreamTransport> = transport.extract(py)?;
    let client_tls = if server_side {
        None
    } else {
        tls.client_settings(py)?
    };
    let server_tls = if server_side {
        tls.server_settings(py)?
    } else {
        None
    };
    // Stop plaintext I/O while still on the calling loop turn. The Rust
    // future below yields once before either peer can start its handshake.
    let prepared = prepare_start_tls_transport(py, transport, protocol)?;
    pyo3_async_runtimes::async_std::future_into_py_with_locals(py, locals, async move {
        let barrier = Python::attach(|py| {
            let sleep = py.import("asyncio")?.getattr("sleep")?.call1((0,))?;
            pyo3_async_runtimes::into_future_with_locals(&locals_for_barrier, sleep)
        })?;
        let _ = barrier.await?;

        let upgraded = async_std::task::spawn_blocking(move || -> PyResult<Py<PyAny>> {
            Python::attach(|py| {
                let upgraded = start_tls_transport(py, prepared, client_tls, server_tls)?;
                Ok(upgraded.into_any())
            })
        })
        .await
        .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
        Ok(upgraded)
    })
}
