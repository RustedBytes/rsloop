//! Utility types and functions for `vibeio`.
//!
//! This module provides supporting infrastructure for the library:
//! - `AsyncWrap`: a wrapper that adapts `AsyncRead`/`AsyncWrite` implementations
//!   to the `tokio::io` traits, enabling interoperability with tokio-based code.
//! - `supports_completion`: check if the current driver supports completion-based I/O.
//! - `supports_io_uring`: check if the system supports io_uring with required operations.

mod async_wrap;

pub use async_wrap::*;

use crate::current_driver;
#[cfg(target_os = "linux")]
use std::sync::OnceLock;

/// Check if the current driver supports completion-based I/O operations.
///
/// This function returns `true` when the runtime is using a driver that can
/// handle completion-based operations (e.g., io_uring with completion queues).
#[inline]
pub fn supports_completion() -> bool {
    current_driver().is_some_and(|driver| driver.supports_completion())
}

/// Check if the system supports io_uring with required operations.
///
/// This function performs a runtime probe to verify that io_uring is available
/// and supports the operations needed by the library (accept, connect, poll,
/// timeout, read, write, etc.). On non-Linux platforms, this always returns `false`.
///
/// The result is cached for the lifetime of the program.
#[inline]
pub fn supports_io_uring() -> bool {
    #[cfg(target_os = "linux")]
    {
        static SUPPORTED: OnceLock<bool> = OnceLock::new();
        *SUPPORTED.get_or_init(detect_io_uring_support)
    }

    #[cfg(not(target_os = "linux"))]
    {
        false
    }
}

#[cfg(target_os = "linux")]
fn detect_io_uring_support() -> bool {
    use io_uring::opcode;

    let ring = match build_probe_ring() {
        Ok(ring) => ring,
        Err(_) => return false,
    };

    let mut probe = io_uring::Probe::new();
    if ring.submitter().register_probe(&mut probe).is_err() {
        return false;
    }

    let mut required_ops = Vec::new();
    required_ops.extend_from_slice(&[
        opcode::Accept::CODE,
        opcode::Connect::CODE,
        opcode::PollAdd::CODE,
        opcode::Timeout::CODE,
        opcode::Read::CODE,
        opcode::Readv::CODE,
        opcode::Recv::CODE,
        opcode::RecvMsg::CODE,
        opcode::Send::CODE,
        opcode::SendMsg::CODE,
        opcode::Write::CODE,
        opcode::Writev::CODE,
    ]);

    #[cfg(feature = "fs")]
    required_ops.extend_from_slice(&[
        opcode::OpenAt::CODE,
        opcode::Fsync::CODE,
        opcode::MkDirAt::CODE,
        opcode::RenameAt::CODE,
        opcode::LinkAt::CODE,
        opcode::SymlinkAt::CODE,
        opcode::UnlinkAt::CODE,
    ]);

    #[cfg(all(feature = "fs", any(target_env = "gnu", musl_v1_2_3)))]
    required_ops.push(opcode::Statx::CODE);

    #[cfg(feature = "splice")]
    required_ops.push(opcode::Splice::CODE);

    required_ops.iter().all(|op| probe.is_supported(*op))
}

#[cfg(target_os = "linux")]
fn build_probe_ring() -> std::io::Result<io_uring::IoUring> {
    let mut builder = io_uring::IoUring::builder();
    builder
        .setup_single_issuer()
        .setup_coop_taskrun()
        .setup_taskrun_flag()
        .setup_submit_all();

    match builder.build(2) {
        Ok(ring) => Ok(ring),
        Err(_) => io_uring::IoUring::new(2),
    }
}
