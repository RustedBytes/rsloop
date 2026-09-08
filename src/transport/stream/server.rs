//! Creating a `Server` from listening sockets.
//!
//! The binding layer resolves addresses and builds the listeners through
//! Python's `socket` module; by the time it reaches here the sockets exist and
//! only need to be wrapped in a `ServerCore`. Accepting does not start until
//! `spawn_accept_tasks`, so `create_server(start_serving=False)` is just a
//! `ServerCore` with idle listeners.

#[cfg(unix)]
use std::fs;
#[cfg(unix)]
use std::io;
use std::net::TcpListener as StdTcpListener;
#[cfg(unix)]
use std::os::unix::net::UnixListener as StdUnixListener;
use std::sync::atomic::{AtomicBool, AtomicUsize};
use std::sync::{Arc, Mutex};

use pyo3::prelude::*;

use super::{PyServer, ServerCore, ServerCreateParams, ServerListener, ServerState};
use crate::async_event::AsyncEvent;

pub fn create_server(py: Python<'_>, params: ServerCreateParams) -> PyResult<Py<PyServer>> {
    crate::profile_scope!("stream.create_server");
    let ServerCreateParams {
        loop_core,
        loop_obj,
        protocol_factory,
        context,
        context_needs_run,
        sockets,
        listeners,
        cleanup_path,
        tls,
    } = params;
    let accept_tasks = Vec::with_capacity(listeners.len());
    Py::new(
        py,
        PyServer {
            core: Arc::new(ServerCore {
                loop_core,
                loop_obj,
                protocol_factory,
                context,
                context_needs_run,
                sockets,
                state: Mutex::new(ServerState {
                    closed: false,
                    serving: false,
                    listeners,
                }),
                accept_tasks: Mutex::new(accept_tasks),
                accept_fds: Mutex::new(Vec::new()),
                active_connections: AtomicUsize::new(0),
                pending_tls_handshakes: AtomicUsize::new(0),
                tls_overload_reported: AtomicBool::new(false),
                closed_notify: AsyncEvent::new(),
                cleanup_path,
                tls,
            }),
        },
    )
}

pub fn tcp_server_listener(listener: StdTcpListener) -> ServerListener {
    ServerListener::Tcp(listener)
}

#[cfg(unix)]
pub fn unix_server_listener(listener: StdUnixListener) -> ServerListener {
    ServerListener::Unix(listener)
}
#[cfg(unix)]
pub fn remove_unix_socket_if_present(path: impl AsRef<std::path::Path>) -> io::Result<()> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(()),
        Err(err) => Err(err),
    }
}
