//! Async signal utilities integrated with `vibeio`.
//!
//! This module provides cross-platform signal handling:
//! - `ctrl_c()` provides a future that resolves when Ctrl-C is received.
//! - On Unix, `Signal` and `signal()` allow listening for specific signals.
//!
//! # Examples
//! ```ignore
//! // Wait for Ctrl-C (cross-platform)
//! vibeio::signal::ctrl_c()?.await?;
//!
//! // Wait for SIGTERM (Unix only)
//! # #[cfg(unix)]
//! let mut sig = vibeio::signal::signal(vibeio::signal::SignalKind::terminate())?;
//! sig.recv().await?;
//! ```
//!
//! # Implementation notes
//! - On Unix, signals are delivered via a dedicated dispatch thread that reads
//!   from a pipe and wakes registered wakers.
//! - On Windows, Ctrl-C is handled via `SetConsoleCtrlHandler`.
//! - Multiple listeners can register for the same signal; all will be woken.

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

// Execute Windows listener bookkeeping tests on Unix as well. Only console
// registration/FFI is Windows-gated; the tested state machine is not a copy.
#[cfg(all(test, not(windows)))]
#[path = "windows.rs"]
mod windows_state_tests;

#[cfg(unix)]
// Public runtime API; not every embedding uses this re-export.
#[allow(unused_imports)]
pub use unix::{CtrlC, Signal, SignalKind, ctrl_c, signal};
#[cfg(windows)]
// Public runtime API; not every embedding uses this re-export.
#[allow(unused_imports)]
pub use windows::{CtrlC, ctrl_c};
