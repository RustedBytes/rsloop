#![cfg_attr(docsrs, feature(doc_cfg))]
#![deny(unsafe_op_in_unsafe_fn)]
// Local safety contracts are required across supported platforms. This lint
// does not replace the lifecycle audits tracked in docs/vibeio-cleanup.md.
#![warn(clippy::undocumented_unsafe_blocks)]

//! # vibeio
//!
//! A high-performance, cross-platform asynchronous runtime for Rust.
//!
//! `vibeio` provides an efficient I/O event loop that leverages the best available driver for each operating system:
//!
//! - **Linux** - uses `io_uring` for true asynchronous I/O.
//! - **Windows** - uses I/O Completion Ports (IOCP) for scalable I/O.
//! - **macOS / BSD / Others** - uses `kqueue` or `epoll` via `mio` for event notification.
//!
//! ## Core features
//!
//! - **Networking** - asynchronous TCP, UDP, and Unix Domain Sockets.
//! - **File system** - asynchronous file operations.
//! - **Timers** - efficient timer and sleep functionality.
//! - **Signals** - handling of OS signals.
//! - **Process management** - spawning and managing child processes.
//! - **Blocking tasks** - offload CPU-intensive or blocking operations to a thread pool.
//!
//! ## Concurrency model: thread-per-core
//!
//! `vibeio` is designed as a **single-threaded** runtime. To utilize multiple cores, you should employ a **thread-per-core** architecture, where a separate `Runtime` is pinned to each processor core. This approach minimizes synchronization overhead and maximizes cache locality.
//!
//! Shared state can be communicated between runtimes using message passing (e.g., channels) or shared atomic structures, but I/O resources are typically owned by the thread that created them.
//!
//! ## Getting started
//!
//! This is rsloop's embedded runtime, accessed internally as `crate::vibeio`.
//! It is not a separately published package or part of rsloop's public Rust API.
//! Build and test it from the repository root; installing the upstream crate
//! does not provide this implementation. See `docs/development.md` for commands.
//!
//! ## Feature flags
//!
//! Networking and timers are always compiled. The following Cargo features are
//! opt-in and disabled in default rsloop wheels. `--all-features` compiles all
//! applicable platform modules; enabling a feature does not select a driver or
//! install signal handlers until the corresponding API is used.
//!
//! - `fs` - enables asynchronous file system operations.
//! - `signal` - enables signal handling.
//! - `process` - enables child process management.
//! - `pipe` - enables pipe support.
//! - `stdio` - enables standard I/O support.
//! - `splice` - enables splice support (Linux).
//! - `blocking-default` - enables the default blocking thread pool.

pub mod blocking;
mod builder;
mod driver;
mod executor;
mod fd_inner;
#[cfg(feature = "fs")]
pub mod fs;
pub mod io;
pub mod net;
mod op;
#[cfg(feature = "process")]
pub mod process;
#[cfg(feature = "signal")]
pub mod signal;
mod task;
#[cfg(test)]
mod test_support;
pub mod time;
mod timer;
pub mod util;

pub use crate::vibeio::builder::*;
// Public runtime API; not every embedding uses this re-export.
#[allow(unused_imports)]
pub use crate::vibeio::driver::RegistrationMode;
pub use crate::vibeio::executor::*;

// Embedding-only readiness plumbing; standalone runtime checks do not use it.
#[allow(unused_imports)]
pub(crate) use fd_inner::InnerRawHandle;
#[cfg(windows)]
#[allow(unused_imports)]
pub(crate) use fd_inner::RawOsHandle;
#[allow(unused_imports)]
pub(crate) use op::ReadinessOp;
