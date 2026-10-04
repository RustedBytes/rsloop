//! Inputs and owned listener values used when creating stream servers.

use std::net::{TcpListener, TcpStream};
#[cfg(unix)]
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::PathBuf;
use std::sync::Arc;

use pyo3::prelude::*;

use crate::engine::LoopCore;
use crate::transport::tls::ServerTlsSettings;

pub enum ServerListener {
    Tcp(TcpListener),
    #[cfg(unix)]
    Unix(UnixListener),
}

pub enum AcceptedStream {
    Tcp(TcpStream),
    #[cfg(unix)]
    Unix(UnixStream),
}

/// Python and loop state carried from the binding layer into a new transport.
pub struct TransportSpawnContext {
    pub loop_core: Arc<LoopCore>,
    pub loop_obj: Py<PyAny>,
    pub protocol: Py<PyAny>,
    pub context: Py<PyAny>,
    pub context_needs_run: bool,
}

impl TransportSpawnContext {
    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "TransportSpawnContext")
    )]
    pub fn new(
        py: Python<'_>,
        loop_core: Arc<LoopCore>,
        loop_obj: &Py<PyAny>,
        protocol: Py<PyAny>,
        context: &Py<PyAny>,
        context_needs_run: bool,
    ) -> Self {
        Self {
            loop_core,
            loop_obj: loop_obj.clone_ref(py),
            protocol,
            context: context.clone_ref(py),
            context_needs_run,
        }
    }
}

/// Fully resolved inputs used to construct a `ServerCore`.
pub struct ServerCreateParams {
    pub loop_core: Arc<LoopCore>,
    pub loop_obj: Py<PyAny>,
    pub protocol_factory: Py<PyAny>,
    pub context: Py<PyAny>,
    pub context_needs_run: bool,
    pub sockets: Vec<Py<PyAny>>,
    pub listeners: Vec<ServerListener>,
    pub cleanup_path: Option<PathBuf>,
    pub tls: Option<Arc<ServerTlsSettings>>,
}

impl ServerCreateParams {
    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "ServerCreateParams")
    )]
    pub fn new(
        spawn_context: TransportSpawnContext,
        sockets: Vec<Py<PyAny>>,
        listeners: Vec<ServerListener>,
    ) -> Self {
        let TransportSpawnContext {
            loop_core,
            loop_obj,
            protocol,
            context,
            context_needs_run,
        } = spawn_context;

        Self {
            loop_core,
            loop_obj,
            protocol_factory: protocol,
            context,
            context_needs_run,
            sockets,
            listeners,
            cleanup_path: None,
            tls: None,
        }
    }

    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "ServerCreateParams")
    )]
    #[cfg(unix)]
    pub fn with_cleanup_path(mut self, cleanup_path: Option<PathBuf>) -> Self {
        self.cleanup_path = cleanup_path;
        self
    }

    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "ServerCreateParams")
    )]
    pub fn with_tls(mut self, tls: Option<Arc<ServerTlsSettings>>) -> Self {
        self.tls = tls;
        self
    }
}
