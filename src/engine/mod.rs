//! Event-loop engine shared by the Python bindings and transport adapters.

mod callbacks;
mod commands;
mod dispatcher;
mod loop_core;
mod timer_entry;

pub use callbacks::{PyHandle, PyTimerHandle, ReadyCallback};
pub use commands::{
    LoopCommand, LoopFutureCommand, LoopIoCommand, LoopRunCommand, LoopSignalCommand,
    LoopTransportCommand,
};
pub use loop_core::LoopCore;

pub(crate) use callbacks::CallbackArgs;
pub(crate) use callbacks::CallbackKind;
#[cfg(unix)]
pub(crate) use loop_core::SignalHandlerTemplate;
pub(crate) use loop_core::{FdWatch, LoopCoreError};
