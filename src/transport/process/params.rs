//! Inputs the binding layer assembles before a subprocess transport exists.
//!
//! `ProcessTransportParams` is built up across `loop_api::process_spawn`, which
//! resolves the Python arguments, and `loop_api::process_stdio`, which may hand
//! back its own readers — hence `stdout_override` / `stderr_override`, used when
//! a pipe had to be created outside `std::process::Command` (for example
//! `stderr=STDOUT`, where one reader feeds both descriptors).
//!
//! `ProcessTextConfig` is present only for `universal_newlines`/`text` mode; it
//! travels to the stdin pipe transport as extra info so writes are encoded the
//! way Python expects.

use std::io::Read;
use std::process::Child;
use std::sync::Arc;

use pyo3::prelude::*;

use crate::engine::LoopCore;
use crate::transport::stream::TransportSpawnContext;

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

impl ProcessTransportParams {
    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "ProcessTransportParams")
    )]
    pub fn new(spawn_context: TransportSpawnContext, child: Child) -> Self {
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
            protocol,
            context,
            context_needs_run,
            text_config: None,
            child,
            stdout_override: None,
            stderr_override: None,
        }
    }

    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "ProcessTransportParams")
    )]
    pub fn with_text_config(mut self, text_config: Option<ProcessTextConfig>) -> Self {
        self.text_config = text_config;
        self
    }

    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "ProcessTransportParams")
    )]
    pub fn with_stdio_overrides(
        mut self,
        stdout_override: Option<BoxedProcessReader>,
        stderr_override: Option<BoxedProcessReader>,
    ) -> Self {
        self.stdout_override = stdout_override;
        self.stderr_override = stderr_override;
        self
    }
}
