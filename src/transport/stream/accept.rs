//! Accepting connections and turning them into transports.
//!
//! Each listener gets one accept loop. The default runs on the loop runtime as
//! an async task (`run_*_accept_task`) so it stops cleanly when the socket
//! closes; `BlockingAcceptLoop` is the fallback for platforms or listeners the
//! runtime cannot drive, running on a `WorkerThread` instead. Windows fans a
//! TCP listener out over several IOCP lanes, since one outstanding `AcceptEx`
//! per listener limits connection setup rate.
//!
//! Whichever loop accepted it, the socket is handed to the loop thread before
//! the protocol is created: `schedule_accepted_transport` posts it as loop work
//! and `spawn_accepted_transport_with_py` builds the transport there, under the
//! GIL. TLS servers additionally take a handshake slot first, so a handshake
//! flood is shed here rather than deeper in.

use std::io;
use std::net::{TcpListener as StdTcpListener, TcpStream as StdTcpStream};
#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd, IntoRawFd};
#[cfg(unix)]
use std::os::unix::net::{UnixListener as StdUnixListener, UnixStream as StdUnixStream};
#[cfg(windows)]
use std::os::windows::io::IntoRawSocket;
#[cfg(windows)]
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use crate::vibeio::net::TcpListener as VibeTcpListener;
#[cfg(unix)]
use crate::vibeio::net::UnixListener as VibeUnixListener;
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;

use super::io_targets::StreamKind;
#[cfg(windows)]
use super::platform::from_owned_raw_socket;
use super::platform::tcp_listener_raw_fd;
use super::poll::poll_read_ready;
use super::socket_transport::spawn_tcp_transport;
#[cfg(unix)]
use super::socket_transport::spawn_unix_transport;
use super::tls_transport::spawn_tls_server_transport;
use super::tuning::max_pending_tls_handshakes;
use super::{AcceptedStream, PyStreamTransport, ServerCore, ServerListener, TransportSpawnContext};
use crate::engine::{LoopCommand, LoopTransportCommand};
#[cfg(unix)]
use crate::fd_ops;
use crate::transport::tls::ServerTlsSettings;

pub(super) fn configure_accepted_tcp_stream(
    server: &Arc<ServerCore>,
    stream: &StdTcpStream,
    message: &str,
) -> bool {
    if let Err(err) = stream.set_nonblocking(true) {
        server.report_error(PyRuntimeError::new_err(err.to_string()), message);
        return false;
    }
    if let Err(err) = stream.set_nodelay(true) {
        server.report_error(PyRuntimeError::new_err(err.to_string()), message);
        return false;
    }
    true
}

#[cfg(unix)]
pub(super) fn configure_accepted_unix_stream(
    server: &Arc<ServerCore>,
    stream: &StdUnixStream,
    message: &str,
) -> bool {
    if let Err(err) = stream.set_nonblocking(true) {
        server.report_error(PyRuntimeError::new_err(err.to_string()), message);
        return false;
    }
    true
}

pub(super) fn report_server_io_error(server: &ServerCore, err: io::Error, message: &str) {
    if !server.is_closed() {
        server.report_error(PyRuntimeError::new_err(err.to_string()), message);
    }
}

pub(crate) struct BlockingAcceptLoop<L> {
    server: Arc<ServerCore>,
    listener: L,
    stop: Arc<AtomicBool>,
}

impl<L> BlockingAcceptLoop<L> {
    pub(crate) fn new(server: Arc<ServerCore>, listener: L, stop: Arc<AtomicBool>) -> Self {
        Self {
            server,
            listener,
            stop,
        }
    }
}

pub(super) fn server_spawn_context(
    py: Python<'_>,
    server: &Arc<ServerCore>,
    protocol: Py<PyAny>,
) -> TransportSpawnContext {
    TransportSpawnContext::new(
        py,
        Arc::clone(&server.loop_core),
        &server.loop_obj,
        protocol,
        &server.context,
        server.context_needs_run,
    )
}

pub(super) fn server_tls_settings(py: Python<'_>, tls: &ServerTlsSettings) -> ServerTlsSettings {
    ServerTlsSettings {
        config: Arc::clone(&tls.config),
        handshake_timeout: tls.handshake_timeout,
        shutdown_timeout: tls.shutdown_timeout,
        ssl_context: tls.ssl_context.clone_ref(py),
    }
}

pub(super) fn spawn_accepted_tcp_transport(
    py: Python<'_>,
    server: &Arc<ServerCore>,
    stream: StdTcpStream,
) -> PyResult<Py<PyStreamTransport>> {
    let protocol = server.create_protocol_with_py(py)?;
    let spawn_context = server_spawn_context(py, server, protocol);
    let server_ref = Some(Arc::downgrade(server));
    if let Some(tls) = server.tls.as_ref() {
        spawn_tls_server_transport(
            py,
            spawn_context,
            StreamKind::Tcp(stream),
            server_tls_settings(py, tls),
            server_ref,
            true,
        )
    } else {
        spawn_tcp_transport(py, spawn_context, stream, server_ref)
    }
}

#[cfg(unix)]
pub(super) fn spawn_accepted_unix_transport(
    py: Python<'_>,
    server: &Arc<ServerCore>,
    stream: StdUnixStream,
) -> PyResult<Py<PyStreamTransport>> {
    let protocol = server.create_protocol_with_py(py)?;
    let spawn_context = server_spawn_context(py, server, protocol);
    let server_ref = Some(Arc::downgrade(server));
    if let Some(tls) = server.tls.as_ref() {
        spawn_tls_server_transport(
            py,
            spawn_context,
            StreamKind::Unix(stream),
            server_tls_settings(py, tls),
            server_ref,
            true,
        )
    } else {
        spawn_unix_transport(py, spawn_context, stream, server_ref)
    }
}

pub(crate) fn spawn_accepted_transport_with_py(
    py: Python<'_>,
    server: &Arc<ServerCore>,
    stream: AcceptedStream,
) -> PyResult<Py<PyStreamTransport>> {
    match stream {
        AcceptedStream::Tcp(stream) => spawn_accepted_tcp_transport(py, server, stream),
        #[cfg(unix)]
        AcceptedStream::Unix(stream) => spawn_accepted_unix_transport(py, server, stream),
    }
}

pub(super) fn schedule_accepted_transport(
    server: &Arc<ServerCore>,
    stream: AcceptedStream,
    message: &str,
) {
    if server.tls.is_some() {
        let Some(pending) = server.reserve_tls_handshake() else {
            if !server.is_closed() && !server.tls_overload_reported.swap(true, Ordering::AcqRel) {
                server.report_error(
                    PyRuntimeError::new_err(format!(
                        "pending TLS handshake limit ({}) reached",
                        max_pending_tls_handshakes()
                    )),
                    "TLS server is overloaded",
                );
            }
            return;
        };
        let server = Arc::clone(server);
        let message = message.to_owned();
        drop(async_std::task::spawn_blocking(move || {
            let _pending = pending;
            if server.is_closed() {
                return;
            }
            let result =
                Python::try_attach(|py| spawn_accepted_transport_with_py(py, &server, stream));
            if let Some(Err(err)) = result
                && !server.is_closed()
            {
                server.report_error(err, &message);
            }
        }));
        return;
    }

    if let Err(err) = server.loop_core.send_command(LoopCommand::Transport(
        LoopTransportCommand::ServerAccepted {
            server: Arc::clone(server),
            stream,
        },
    )) {
        server.report_error(PyRuntimeError::new_err(err.to_string()), message);
    }
}

pub(super) fn run_tcp_accept_loop(params: BlockingAcceptLoop<StdTcpListener>) {
    let BlockingAcceptLoop {
        server,
        listener,
        stop,
    } = params;
    loop {
        if stop.load(Ordering::Acquire) || server.is_closed() {
            return;
        }

        match poll_read_ready(tcp_listener_raw_fd(&listener)) {
            Ok(false) => continue,
            Ok(true) => {}
            Err(err) => {
                report_server_io_error(&server, err, "TCP server accept failed");
                return;
            }
        }

        if stop.load(Ordering::Acquire) || server.is_closed() {
            return;
        }

        loop {
            match listener.accept() {
                Ok((stream, _addr)) => {
                    if !configure_accepted_tcp_stream(
                        &server,
                        &stream,
                        "failed to configure TCP connection",
                    ) {
                        continue;
                    }
                    schedule_accepted_transport(
                        &server,
                        AcceptedStream::Tcp(stream),
                        "failed to accept TCP connection",
                    );
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(err) => {
                    report_server_io_error(&server, err, "TCP server accept failed");
                    return;
                }
            }
        }
    }
}
pub(crate) async fn run_server_accept_task(server: Arc<ServerCore>, listener: ServerListener) {
    match listener {
        ServerListener::Tcp(listener) => run_tcp_accept_task(server, listener).await,
        #[cfg(unix)]
        ServerListener::Unix(listener) => run_unix_accept_task(server, listener).await,
    }
}

pub(super) async fn run_tcp_accept_task(server: Arc<ServerCore>, listener: StdTcpListener) {
    #[cfg(windows)]
    {
        run_windows_tcp_accept_pool(server, listener).await;
    }

    #[cfg(not(windows))]
    run_tcp_accept_lane(server, listener).await;
}

#[cfg(windows)]
pub(super) struct WindowsAcceptPool(Vec<crate::vibeio::JoinHandle<()>>);

#[cfg(windows)]
impl Drop for WindowsAcceptPool {
    fn drop(&mut self) {
        for lane in self.0.drain(..) {
            lane.cancel();
        }
    }
}

#[cfg(windows)]
pub(super) async fn run_windows_tcp_accept_pool(server: Arc<ServerCore>, listener: StdTcpListener) {
    let lane_count = std::thread::available_parallelism()
        .map_or(4, usize::from)
        .saturating_mul(2)
        .clamp(4, 32);
    let listener = match VibeTcpListener::from_std(listener) {
        Ok(listener) => Rc::new(listener),
        Err(err) => {
            report_server_io_error(&server, err, "TCP server accept failed");
            return;
        }
    };
    let lanes = (0..lane_count)
        .map(|_| {
            crate::vibeio::spawn(run_windows_tcp_accept_lane(
                Arc::clone(&server),
                Rc::clone(&listener),
            ))
        })
        .collect();
    let _pool = WindowsAcceptPool(lanes);
    std::future::pending::<()>().await;
}

#[cfg(windows)]
pub(super) async fn run_windows_tcp_accept_lane(
    server: Arc<ServerCore>,
    listener: Rc<VibeTcpListener>,
) {
    loop {
        if server.is_closed() {
            return;
        }

        match listener.accept().await {
            Ok((stream, _addr)) => {
                let raw = stream.into_raw_socket();
                let stream = from_owned_raw_socket::<StdTcpStream>(raw);
                if !configure_accepted_tcp_stream(
                    &server,
                    &stream,
                    "failed to configure TCP connection",
                ) {
                    continue;
                }
                schedule_accepted_transport(
                    &server,
                    AcceptedStream::Tcp(stream),
                    "failed to accept TCP connection",
                );
            }
            Err(err) => {
                report_server_io_error(&server, err, "TCP server accept failed");
                return;
            }
        }
    }
}

#[cfg(not(windows))]
pub(super) async fn run_tcp_accept_lane(server: Arc<ServerCore>, listener: StdTcpListener) {
    let listener = match VibeTcpListener::from_std(listener) {
        Ok(listener) => listener,
        Err(err) => {
            report_server_io_error(&server, err, "TCP server accept failed");
            return;
        }
    };

    loop {
        if server.is_closed() {
            return;
        }

        match listener.accept().await {
            Ok((stream, _addr)) => {
                // SAFETY: `into_raw_fd` transfers sole ownership to `StdTcpStream`.
                let stream = unsafe { StdTcpStream::from_raw_fd(stream.into_raw_fd()) };
                if !configure_accepted_tcp_stream(
                    &server,
                    &stream,
                    "failed to configure TCP connection",
                ) {
                    continue;
                }
                schedule_accepted_transport(
                    &server,
                    AcceptedStream::Tcp(stream),
                    "failed to accept TCP connection",
                );
            }
            Err(err) => {
                report_server_io_error(&server, err, "TCP server accept failed");
                return;
            }
        }
    }
}

#[cfg(unix)]
pub(super) fn run_unix_accept_loop(params: BlockingAcceptLoop<StdUnixListener>) {
    let BlockingAcceptLoop {
        server,
        listener,
        stop,
    } = params;
    loop {
        if stop.load(Ordering::Acquire) || server.is_closed() {
            return;
        }

        match poll_read_ready(fd_ops::RawFd::from(listener.as_raw_fd())) {
            Ok(false) => continue,
            Ok(true) => {}
            Err(err) => {
                report_server_io_error(&server, err, "Unix server accept failed");
                return;
            }
        }

        if stop.load(Ordering::Acquire) || server.is_closed() {
            return;
        }

        loop {
            match listener.accept() {
                Ok((stream, _addr)) => {
                    if !configure_accepted_unix_stream(
                        &server,
                        &stream,
                        "failed to configure Unix connection",
                    ) {
                        continue;
                    }
                    schedule_accepted_transport(
                        &server,
                        AcceptedStream::Unix(stream),
                        "failed to accept Unix connection",
                    );
                }
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => break,
                Err(err) => {
                    report_server_io_error(&server, err, "Unix server accept failed");
                    return;
                }
            }
        }
    }
}

#[cfg(unix)]
pub(super) async fn run_unix_accept_task(server: Arc<ServerCore>, listener: StdUnixListener) {
    let listener = match VibeUnixListener::from_std(listener) {
        Ok(listener) => listener,
        Err(err) => {
            report_server_io_error(&server, err, "Unix server accept failed");
            return;
        }
    };

    loop {
        if server.is_closed() {
            return;
        }

        match listener.accept().await {
            Ok((stream, _addr)) => {
                // SAFETY: `into_raw_fd` transfers sole ownership to `StdUnixStream`.
                let stream = unsafe { StdUnixStream::from_raw_fd(stream.into_raw_fd()) };
                if !configure_accepted_unix_stream(
                    &server,
                    &stream,
                    "failed to configure Unix connection",
                ) {
                    continue;
                }
                schedule_accepted_transport(
                    &server,
                    AcceptedStream::Unix(stream),
                    "failed to accept Unix connection",
                );
            }
            Err(err) => {
                report_server_io_error(&server, err, "Unix server accept failed");
                return;
            }
        }
    }
}
