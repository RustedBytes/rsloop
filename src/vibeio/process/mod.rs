//! Process utilities for spawning and managing child processes.
//!
//! This module provides async-aware wrappers around `std::process::Command` and
//! `std::process::Child`, with driver-backed pipe I/O and offloaded blocking
//! operations when configured inside a runtime.
//!
//! Key types:
//! - `Command`: an async-aware builder for spawning child processes.
//! - `Child`: represents a running child process with async `wait()` and
//!   synchronous `kill()` and `try_wait()` methods.
//! - `ChildStdin`, `ChildStdout`, `ChildStderr`: async-aware stdio streams.
//!
//! Implementation notes:
//! - On Unix, the module uses `mio`/`io_uring` drivers to register child process
//!   file descriptors for async I/O when possible. Falls back to a blocking pool
//!   when the driver is unavailable or registration fails.
//! - Child drop retains reaping ownership through a runtime reaper when available
//!   or a background-thread fallback otherwise.
//! - Construction and child waiting can run outside a runtime. Inside a runtime,
//!   stdio's blocking fallback and Command::status/output require a blocking
//!   pool. Outside a runtime, those fallback operations execute synchronously
//!   when polled. Command::spawn always invokes std's synchronous spawn.
//!
//! # Cancellation of blocking operations
//!
//! An offloaded operation owns its stream or command until its worker finishes.
//! Dropping the pending future does not stop the worker or restore that object
//! to its wrapper. The wrapper remains consumed; operations return a closed or
//! consumed error, and infallible command configuration/accessors may panic.
//! A future dropped before its first poll has not transferred ownership.
//! This limitation is distinct from Unix driver-backed pipe cancellation.

mod reaper;

use reaper::ZombieReaper;
pub(crate) use reaper::{ZombieReaperMessage, start_zombie_reaper};

use std::ffi::OsStr;
#[cfg(unix)]
use std::future::poll_fn;
use std::io::{self, Read, Write};

#[cfg(unix)]
use mio::Interest;

#[cfg(unix)]
use std::os::fd::{AsRawFd, IntoRawFd, RawFd};
#[cfg(windows)]
use std::os::windows::io::{AsRawHandle, IntoRawHandle, RawHandle};

#[cfg(unix)]
use crate::vibeio::driver::RegistrationMode;
use crate::vibeio::executor::current_driver;
#[cfg(unix)]
use crate::vibeio::fd_inner::InnerRawHandle;
use crate::vibeio::io::{AsyncRead, AsyncWrite, IoBuf, IoBufMut, iobuf_to_slice, read_into_buf};
#[cfg(unix)]
use crate::vibeio::op::{ReadOp, WriteOp};

pub use std::process::{ExitStatus, Output, Stdio};

#[cfg(unix)]
enum ChildIo {
    Async(InnerRawHandle),
    Blocking,
}

#[cfg(windows)]
enum ChildIo {
    Blocking,
}

#[inline]
fn stdio_closed_error() -> io::Error {
    io::Error::new(io::ErrorKind::BrokenPipe, "child stdio is closed")
}

#[inline]
fn command_consumed_error() -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, "command has been consumed")
}

#[inline]
fn child_consumed_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::InvalidInput,
        "child process has been consumed",
    )
}

#[inline]
fn blocking_pool_io_error() -> io::Error {
    io::Error::other("can't spawn blocking task for process I/O")
}

#[cfg(unix)]
#[inline]
fn make_child_io(fd: RawFd, interest: Interest) -> io::Result<ChildIo> {
    if let Some(driver) = current_driver() {
        match InnerRawHandle::new_with_driver_and_mode(
            &driver,
            fd,
            interest,
            RegistrationMode::Completion,
        ) {
            Ok(handle) => {
                crate::vibeio::fd_inner::set_nonblocking(fd, !handle.uses_completion())?;
                Ok(ChildIo::Async(handle))
            }
            Err(_) => Ok(ChildIo::Blocking),
        }
    } else {
        Ok(ChildIo::Blocking)
    }
}

#[inline]
async fn read_in_blocking_pool<R, B>(inner: R, buf: B) -> (io::Result<usize>, R, B)
where
    R: Read + Send + 'static,
    B: IoBufMut,
{
    let (result, (inner, buf)) =
        crate::vibeio::blocking::with_buffer((inner, buf), |(inner, buf)| {
            read_into_buf(buf, |slice| inner.read(slice))
        })
        .await;
    (
        result.unwrap_or_else(|_| Err(blocking_pool_io_error())),
        inner,
        buf,
    )
}

#[inline]
async fn write_in_blocking_pool<W, B>(inner: W, buf: B) -> (io::Result<usize>, W, B)
where
    W: Write + Send + 'static,
    B: IoBuf,
{
    let (result, (inner, buf)) =
        crate::vibeio::blocking::with_buffer((inner, buf), |(inner, buf)| {
            inner.write(iobuf_to_slice(buf))
        })
        .await;
    (
        result.unwrap_or_else(|_| Err(blocking_pool_io_error())),
        inner,
        buf,
    )
}

/// Async-aware child process stdin stream.
///
/// This type wraps `std::process::ChildStdin` and implements `AsyncWrite`
/// to allow writing to a child process's standard input without blocking
/// the executor.
///
/// # Examples
/// See "Child-process pipes and exit status" in
/// `tools/vibeio-check/EXAMPLES.md` for a Unix-gated executable example with
/// checked I/O results, explicit flushing and concurrent pipe draining.
pub struct ChildStdin {
    inner: Option<std::process::ChildStdin>,
    #[allow(dead_code)]
    io: ChildIo,
}

/// Async-aware child process stdout stream.
///
/// This type wraps `std::process::ChildStdout` and implements `AsyncRead`
/// to allow reading from a child process's standard output without blocking
/// the executor.
///
/// # Examples
/// See "Child-process pipes and exit status" in
/// `tools/vibeio-check/EXAMPLES.md` for a Unix-gated executable example with
/// checked I/O results, explicit flushing and concurrent pipe draining.
pub struct ChildStdout {
    inner: Option<std::process::ChildStdout>,
    #[allow(dead_code)]
    io: ChildIo,
}

/// Async-aware child process stderr stream.
///
/// This type wraps `std::process::ChildStderr` and implements `AsyncRead`
/// to allow reading from a child process's standard error without blocking
/// the executor.
///
/// # Examples
/// See "Child-process pipes and exit status" in
/// `tools/vibeio-check/EXAMPLES.md` for a Unix-gated executable example with
/// checked I/O results, explicit flushing and concurrent pipe draining.
pub struct ChildStderr {
    inner: Option<std::process::ChildStderr>,
    #[allow(dead_code)]
    io: ChildIo,
}

impl ChildStdin {
    /// Create a new `ChildStdin` from a standard library `ChildStdin`.
    #[inline]
    pub(crate) fn from_std(inner: std::process::ChildStdin) -> io::Result<Self> {
        #[cfg(unix)]
        let io = make_child_io(inner.as_raw_fd(), Interest::WRITABLE)?;
        #[cfg(windows)]
        let io = ChildIo::Blocking;

        Ok(Self {
            inner: Some(inner),
            io,
        })
    }

    /// Consume this `ChildStdin` and return the underlying `std::process::ChildStdin`.
    #[inline]
    pub fn into_std(mut self) -> std::process::ChildStdin {
        self.inner.take().expect("child stdin is already taken")
    }

    #[inline]
    fn drop_handle(&mut self) {
        // Deregister before the standard stream closes its descriptor.
        // Replacing the state also prevents a second drop during field cleanup.
        self.io = ChildIo::Blocking;
    }
}

impl ChildStdout {
    /// Create a new `ChildStdout` from a standard library `ChildStdout`.
    #[inline]
    pub(crate) fn from_std(inner: std::process::ChildStdout) -> io::Result<Self> {
        #[cfg(unix)]
        let io = make_child_io(inner.as_raw_fd(), Interest::READABLE)?;
        #[cfg(windows)]
        let io = ChildIo::Blocking;

        Ok(Self {
            inner: Some(inner),
            io,
        })
    }

    /// Consume this `ChildStdout` and return the underlying `std::process::ChildStdout`.
    #[inline]
    pub fn into_std(mut self) -> std::process::ChildStdout {
        self.inner.take().expect("child stdout is already taken")
    }

    #[inline]
    fn drop_handle(&mut self) {
        // Deregister before the standard stream closes its descriptor.
        // Replacing the state also prevents a second drop during field cleanup.
        self.io = ChildIo::Blocking;
    }
}

impl ChildStderr {
    /// Create a new `ChildStderr` from a standard library `ChildStderr`.
    #[inline]
    pub(crate) fn from_std(inner: std::process::ChildStderr) -> io::Result<Self> {
        #[cfg(unix)]
        let io = make_child_io(inner.as_raw_fd(), Interest::READABLE)?;
        #[cfg(windows)]
        let io = ChildIo::Blocking;

        Ok(Self {
            inner: Some(inner),
            io,
        })
    }

    /// Consume this `ChildStderr` and return the underlying `std::process::ChildStderr`.
    #[inline]
    pub fn into_std(mut self) -> std::process::ChildStderr {
        self.inner.take().expect("child stderr is already taken")
    }

    #[inline]
    fn drop_handle(&mut self) {
        // Deregister before the standard stream closes its descriptor.
        // Replacing the state also prevents a second drop during field cleanup.
        self.io = ChildIo::Blocking;
    }
}

impl Drop for ChildStdin {
    #[inline]
    fn drop(&mut self) {
        self.drop_handle();
    }
}

impl Drop for ChildStdout {
    #[inline]
    fn drop(&mut self) {
        self.drop_handle();
    }
}

impl Drop for ChildStderr {
    #[inline]
    fn drop(&mut self) {
        self.drop_handle();
    }
}

impl AsyncWrite for ChildStdin {
    #[inline]
    async fn write<B: IoBuf>(&mut self, buf: B) -> (Result<usize, io::Error>, B) {
        if buf.buf_len() == 0 {
            return (Ok(0), buf);
        }

        #[cfg(unix)]
        if let ChildIo::Async(handle) = &self.io {
            let mut op = WriteOp::new(handle, buf);
            let result = poll_fn(|cx| handle.poll_op(cx, &mut op)).await;
            return (result, op.take_bufs());
        }

        if current_driver().is_some() {
            let inner = match self.inner.take() {
                Some(inner) => inner,
                None => return (Err(stdio_closed_error()), buf),
            };
            let (result, inner, buf) = write_in_blocking_pool(inner, buf).await;
            self.inner = Some(inner);
            (result, buf)
        } else {
            let inner = match self.inner.as_mut() {
                Some(inner) => inner,
                None => return (Err(stdio_closed_error()), buf),
            };
            let temp_slice = iobuf_to_slice(&buf);
            (inner.write(temp_slice), buf)
        }
    }

    #[inline]
    async fn flush(&mut self) -> Result<(), io::Error> {
        #[cfg(unix)]
        if let ChildIo::Async(_) = &self.io {
            return Ok(());
        }

        if current_driver().is_some() {
            let inner = match self.inner.take() {
                Some(inner) => inner,
                None => return Err(stdio_closed_error()),
            };
            let (result, inner) = crate::vibeio::blocking::with_buffer(inner, Write::flush).await;
            self.inner = Some(inner);
            result.unwrap_or_else(|_| Err(blocking_pool_io_error()))
        } else {
            let inner = self.inner.as_mut().ok_or_else(stdio_closed_error)?;
            inner.flush()
        }
    }
}

impl AsyncRead for ChildStdout {
    #[inline]
    async fn read<B: IoBufMut>(&mut self, buf: B) -> (Result<usize, io::Error>, B) {
        if buf.buf_capacity() == 0 {
            return (Ok(0), buf);
        }

        #[cfg(unix)]
        if let ChildIo::Async(handle) = &self.io {
            let mut op = ReadOp::new(handle, buf);
            let result = poll_fn(|cx| handle.poll_op(cx, &mut op)).await;
            return (result, op.take_bufs());
        }

        if current_driver().is_some() {
            let inner = match self.inner.take() {
                Some(inner) => inner,
                None => return (Err(stdio_closed_error()), buf),
            };
            let (result, inner, buf) = read_in_blocking_pool(inner, buf).await;
            self.inner = Some(inner);
            (result, buf)
        } else {
            let inner = match self.inner.as_mut() {
                Some(inner) => inner,
                None => return (Err(stdio_closed_error()), buf),
            };
            let mut buf = buf;
            let result = read_into_buf(&mut buf, |slice| inner.read(slice));
            (result, buf)
        }
    }
}

impl AsyncRead for ChildStderr {
    #[inline]
    async fn read<B: IoBufMut>(&mut self, buf: B) -> (Result<usize, io::Error>, B) {
        if buf.buf_capacity() == 0 {
            return (Ok(0), buf);
        }

        #[cfg(unix)]
        if let ChildIo::Async(handle) = &self.io {
            let mut op = ReadOp::new(handle, buf);
            let result = poll_fn(|cx| handle.poll_op(cx, &mut op)).await;
            return (result, op.take_bufs());
        }

        if current_driver().is_some() {
            let inner = match self.inner.take() {
                Some(inner) => inner,
                None => return (Err(stdio_closed_error()), buf),
            };
            let (result, inner, buf) = read_in_blocking_pool(inner, buf).await;
            self.inner = Some(inner);
            (result, buf)
        } else {
            let inner = match self.inner.as_mut() {
                Some(inner) => inner,
                None => return (Err(stdio_closed_error()), buf),
            };
            let mut buf = buf;
            let result = read_into_buf(&mut buf, |slice| inner.read(slice));
            (result, buf)
        }
    }
}

#[cfg(unix)]
impl AsRawFd for ChildStdin {
    #[inline]
    fn as_raw_fd(&self) -> RawFd {
        self.inner
            .as_ref()
            .expect("child stdin is already taken")
            .as_raw_fd()
    }
}

#[cfg(unix)]
impl IntoRawFd for ChildStdin {
    #[inline]
    fn into_raw_fd(self) -> RawFd {
        self.into_std().into_raw_fd()
    }
}

#[cfg(unix)]
impl AsRawFd for ChildStdout {
    #[inline]
    fn as_raw_fd(&self) -> RawFd {
        self.inner
            .as_ref()
            .expect("child stdout is already taken")
            .as_raw_fd()
    }
}

#[cfg(unix)]
impl IntoRawFd for ChildStdout {
    #[inline]
    fn into_raw_fd(self) -> RawFd {
        self.into_std().into_raw_fd()
    }
}

#[cfg(unix)]
impl AsRawFd for ChildStderr {
    #[inline]
    fn as_raw_fd(&self) -> RawFd {
        self.inner
            .as_ref()
            .expect("child stderr is already taken")
            .as_raw_fd()
    }
}

#[cfg(unix)]
impl IntoRawFd for ChildStderr {
    #[inline]
    fn into_raw_fd(self) -> RawFd {
        self.into_std().into_raw_fd()
    }
}

#[cfg(windows)]
impl AsRawHandle for ChildStdin {
    #[inline]
    fn as_raw_handle(&self) -> RawHandle {
        self.inner
            .as_ref()
            .expect("child stdin is already taken")
            .as_raw_handle()
    }
}

#[cfg(windows)]
impl IntoRawHandle for ChildStdin {
    #[inline]
    fn into_raw_handle(self) -> RawHandle {
        self.into_std().into_raw_handle()
    }
}

#[cfg(windows)]
impl AsRawHandle for ChildStdout {
    #[inline]
    fn as_raw_handle(&self) -> RawHandle {
        self.inner
            .as_ref()
            .expect("child stdout is already taken")
            .as_raw_handle()
    }
}

#[cfg(windows)]
impl IntoRawHandle for ChildStdout {
    #[inline]
    fn into_raw_handle(self) -> RawHandle {
        self.into_std().into_raw_handle()
    }
}

#[cfg(windows)]
impl AsRawHandle for ChildStderr {
    #[inline]
    fn as_raw_handle(&self) -> RawHandle {
        self.inner
            .as_ref()
            .expect("child stderr is already taken")
            .as_raw_handle()
    }
}

#[cfg(windows)]
impl IntoRawHandle for ChildStderr {
    #[inline]
    fn into_raw_handle(self) -> RawHandle {
        self.into_std().into_raw_handle()
    }
}

/// Async-aware wrapper around `std::process::Child`.
///
/// This type provides async methods to interact with a running child process:
/// - `wait()`: asynchronously wait for the process to exit.
/// - `kill()`: kill the process.
/// - `try_wait()`: non-blocking check if the process has exited.
/// - `stdin`, `stdout`, `stderr`: async streams for stdio.
///
/// # Examples
/// See "Child-process pipes and exit status" in
/// `tools/vibeio-check/EXAMPLES.md` for a Unix-gated executable example with
/// checked I/O results, explicit flushing and concurrent pipe draining.
pub struct Child {
    inner: Option<std::process::Child>,
    id: u32,
    pub stdin: Option<ChildStdin>,
    pub stdout: Option<ChildStdout>,
    pub stderr: Option<ChildStderr>,
    reaper: ZombieReaper,
}

impl Child {
    /// Create a new `Child` from a standard library `Child`.
    #[inline]
    pub(crate) fn from_std(child: std::process::Child) -> io::Result<Self> {
        let id = child.id();
        // Install reaping ownership before any fallible stdio setup. Dropping
        // std::process::Child alone would not reap a partially wrapped child.
        let mut wrapped = Self {
            inner: Some(child),
            id,
            stdin: None,
            stdout: None,
            stderr: None,
            reaper: ZombieReaper::new(),
        };
        wrapped.stdin = wrapped
            .inner_mut()?
            .stdin
            .take()
            .map(ChildStdin::from_std)
            .transpose()?;
        wrapped.stdout = wrapped
            .inner_mut()?
            .stdout
            .take()
            .map(ChildStdout::from_std)
            .transpose()?;
        wrapped.stderr = wrapped
            .inner_mut()?
            .stderr
            .take()
            .map(ChildStderr::from_std)
            .transpose()?;
        Ok(wrapped)
    }

    /// Returns the OS-assigned process identifier.
    #[inline]
    pub fn id(&self) -> u32 {
        self.id
    }

    #[inline]
    fn inner_mut(&mut self) -> io::Result<&mut std::process::Child> {
        self.inner.as_mut().ok_or_else(child_consumed_error)
    }

    #[inline]
    fn take_inner(&mut self) -> io::Result<std::process::Child> {
        self.inner.take().ok_or_else(child_consumed_error)
    }

    /// Force kill the process.
    #[inline]
    pub fn kill(&mut self) -> io::Result<()> {
        self.inner_mut()?.kill()
    }

    /// Asynchronously wait for the process to exit.
    ///
    /// This method returns a future that resolves to the process's exit status.
    /// The future completes when the process has fully exited and been reaped.
    #[inline]
    pub async fn wait(&mut self) -> io::Result<ExitStatus> {
        let _ = self.stdin.take(); // Similarly to std::process::Child::wait
        let child = self.take_inner()?;
        self.reaper.wait(child).await
    }

    /// Check if the process has exited without blocking.
    ///
    /// Returns `Ok(Some(status))` if the process has exited, `Ok(None)` if it
    /// is still running, or an error if checking the status fails.
    #[inline]
    pub fn try_wait(&mut self) -> io::Result<Option<ExitStatus>> {
        self.inner_mut()?.try_wait()
    }
}

impl Drop for Child {
    #[inline]
    fn drop(&mut self) {
        let _ = self.stdin.take(); // Similarly to std::process::Child::wait
        if let Some(child) = self.inner.take() {
            self.reaper.reap_on_drop(child);
        }
    }
}

/// Async-aware builder for spawning child processes.
///
/// This type wraps `std::process::Command` and provides async versions of
/// the spawn methods:
/// - `spawn()`: spawn the process and return a `Child`.
/// - `status()`: run the process to completion and return its exit status.
/// - `output()`: run the process to completion and return its output.
///
/// # Examples
/// See "Child-process pipes and exit status" in
/// `tools/vibeio-check/EXAMPLES.md` for a Unix-gated executable example with
/// checked I/O results, explicit flushing and concurrent pipe draining.
pub struct Command {
    inner: Option<std::process::Command>,
}

impl Command {
    /// Create a new `Command` for the given program.
    #[inline]
    pub fn new(program: impl AsRef<OsStr>) -> Self {
        Self {
            inner: Some(std::process::Command::new(program)),
        }
    }

    #[inline]
    fn inner_mut(&mut self) -> &mut std::process::Command {
        self.inner.as_mut().expect("command has been consumed")
    }

    /// Add an argument to pass to the program.
    #[inline]
    pub fn arg(&mut self, arg: impl AsRef<OsStr>) -> &mut Self {
        self.inner_mut().arg(arg);
        self
    }

    /// Add multiple arguments to pass to the program.
    #[inline]
    pub fn args<I, S>(&mut self, args: I) -> &mut Self
    where
        I: IntoIterator<Item = S>,
        S: AsRef<OsStr>,
    {
        self.inner_mut().args(args);
        self
    }

    /// Set an environment variable for the process.
    #[inline]
    pub fn env<K, V>(&mut self, key: K, val: V) -> &mut Self
    where
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self.inner_mut().env(key, val);
        self
    }

    /// Set multiple environment variables for the process.
    #[inline]
    pub fn envs<I, K, V>(&mut self, vars: I) -> &mut Self
    where
        I: IntoIterator<Item = (K, V)>,
        K: AsRef<OsStr>,
        V: AsRef<OsStr>,
    {
        self.inner_mut().envs(vars);
        self
    }

    /// Remove an environment variable for the process.
    #[inline]
    pub fn env_remove<K: AsRef<OsStr>>(&mut self, key: K) -> &mut Self {
        self.inner_mut().env_remove(key);
        self
    }

    /// Clear all environment variables for the process.
    #[inline]
    pub fn env_clear(&mut self) -> &mut Self {
        self.inner_mut().env_clear();
        self
    }

    /// Set the working directory for the process.
    #[inline]
    pub fn current_dir(&mut self, dir: impl AsRef<std::path::Path>) -> &mut Self {
        self.inner_mut().current_dir(dir);
        self
    }

    /// Configure the standard input for the process.
    #[inline]
    pub fn stdin(&mut self, cfg: Stdio) -> &mut Self {
        self.inner_mut().stdin(cfg);
        self
    }

    /// Configure the standard output for the process.
    #[inline]
    pub fn stdout(&mut self, cfg: Stdio) -> &mut Self {
        self.inner_mut().stdout(cfg);
        self
    }

    /// Configure the standard error for the process.
    #[inline]
    pub fn stderr(&mut self, cfg: Stdio) -> &mut Self {
        self.inner_mut().stderr(cfg);
        self
    }

    /// Spawn the process and return a `Child` handle.
    #[inline]
    pub fn spawn(&mut self) -> io::Result<Child> {
        let child = self
            .inner
            .as_mut()
            .ok_or_else(command_consumed_error)?
            .spawn()?;
        Child::from_std(child)
    }

    /// Run the process to completion and return its exit status.
    ///
    /// This is an async version of `std::process::Command::status`.
    /// Inside a runtime it requires a blocking pool; outside one it blocks when
    /// polled. Canceling a pending offload leaves this command consumed and does
    /// not stop the worker. See the module's cancellation notes.
    #[inline]
    pub async fn status(&mut self) -> io::Result<ExitStatus> {
        if current_driver().is_some() {
            let inner = self.inner.take().ok_or_else(command_consumed_error)?;
            let (result, inner) =
                crate::vibeio::blocking::with_buffer(inner, std::process::Command::status).await;
            self.inner = Some(inner);
            result.unwrap_or_else(|_| Err(blocking_pool_io_error()))
        } else {
            self.inner_mut().status()
        }
    }

    /// Run the process to completion and return its output.
    ///
    /// This is an async version of `std::process::Command::output`.
    /// Inside a runtime it requires a blocking pool; outside one it blocks when
    /// polled. Canceling a pending offload leaves this command consumed and does
    /// not stop the worker. See the module's cancellation notes.
    #[inline]
    pub async fn output(&mut self) -> io::Result<Output> {
        if current_driver().is_some() {
            let inner = self.inner.take().ok_or_else(command_consumed_error)?;
            let (result, inner) =
                crate::vibeio::blocking::with_buffer(inner, std::process::Command::output).await;
            self.inner = Some(inner);
            result.unwrap_or_else(|_| Err(blocking_pool_io_error()))
        } else {
            self.inner_mut().output()
        }
    }

    /// Get a mutable reference to the underlying `std::process::Command`.
    #[inline]
    pub fn as_std(&mut self) -> &mut std::process::Command {
        self.inner_mut()
    }

    /// Consume this `Command` and return the underlying `std::process::Command`.
    #[inline]
    pub fn into_std(mut self) -> std::process::Command {
        self.inner.take().expect("command has been consumed")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vibeio::driver::AnyDriver;
    use crate::vibeio::executor::Runtime;
    use crate::vibeio::io::{AsyncRead, AsyncWrite, IoBufWithCursor};

    #[cfg(unix)]
    #[test]
    fn child_stream_conversion_deregisters_once_and_preserves_descriptors() {
        use std::os::fd::AsFd;
        let mut driver = AnyDriver::new_mock();
        let AnyDriver::Mock(mock) = &mut driver else {
            unreachable!()
        };
        mock.registrations = Some(Default::default());
        mock.registrations
            .as_ref()
            .unwrap()
            .results
            .borrow_mut()
            .extend((0..3).map(|index| Ok(mio::Token(index))));
        Runtime::new(driver).block_on(async {
            let mut child = std::process::Command::new(std::env::current_exe().unwrap())
                .arg("--list")
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped())
                .spawn()
                .unwrap();
            // Reap even if an assertion fails; closing streams unblocks output.
            struct Cleanup(std::process::Child);
            impl Drop for Cleanup {
                fn drop(&mut self) {
                    let _ = self.0.kill();
                    let _ = self.0.wait();
                }
            }
            let stdin = child.stdin.take().unwrap();
            let stdout = child.stdout.take().unwrap();
            let stderr = child.stderr.take().unwrap();
            let child = Cleanup(child);
            macro_rules! roundtrip {
                ($stream:ident, $wrapper:ident) => {{
                    let fd = $stream.as_raw_fd();
                    let stream = $wrapper::from_std($stream).unwrap().into_std();
                    assert_eq!(stream.as_raw_fd(), fd);
                    drop(stream.as_fd().try_clone_to_owned().unwrap());
                    stream
                }};
            }
            let stdin = roundtrip!(stdin, ChildStdin);
            let stdout = roundtrip!(stdout, ChildStdout);
            let stderr = roundtrip!(stderr, ChildStderr);
            let driver = current_driver().unwrap();
            let AnyDriver::Mock(mock) = driver.as_ref() else {
                unreachable!()
            };
            assert_eq!(
                *mock.registrations.as_ref().unwrap().deregistered.borrow(),
                [mio::Token(0), mio::Token(1), mio::Token(2)]
            );
            drop((stdin, stdout, stderr));
            drop(child);
        });
    }

    #[cfg(unix)]
    #[test]
    fn failed_child_io_configuration_releases_registration() {
        let mut driver = AnyDriver::new_mock();
        let AnyDriver::Mock(mock) = &mut driver else {
            unreachable!()
        };
        mock.registrations = Some(Default::default());
        mock.registrations
            .as_ref()
            .unwrap()
            .results
            .borrow_mut()
            .push_back(Ok(mio::Token(0)));
        Runtime::new(driver).block_on(async {
            let result = make_child_io(-1, Interest::READABLE);
            assert!(matches!(result, Err(ref error) if error.raw_os_error() == Some(libc::EBADF)));
            let driver = current_driver().unwrap();
            let AnyDriver::Mock(mock) = driver.as_ref() else {
                unreachable!()
            };
            assert_eq!(
                *mock.registrations.as_ref().unwrap().deregistered.borrow(),
                [mio::Token(0)]
            );
        });
    }

    fn make_runtime() -> Runtime {
        Runtime::new(AnyDriver::new_best().expect("driver should initialize"))
    }

    #[test]
    fn command_offloads_restore_configuration_on_success_and_pool_rejection() {
        struct TestPool(bool);
        impl crate::vibeio::blocking::BlockingThreadPool for TestPool {
            fn spawn(&self, task: Box<dyn FnOnce() + Send>) {
                if self.0 {
                    std::thread::spawn(task).join().unwrap();
                }
            }
        }
        for run in [false, true] {
            let runtime = crate::vibeio::RuntimeBuilder::new()
                .driver(crate::vibeio::DriverKind::Mock)
                .blocking_pool(Box::new(TestPool(run)))
                .build()
                .unwrap();
            runtime.block_on(async move {
                // Enumerate this test binary's tests without running them.
                let executable = std::env::current_exe().unwrap();
                let mut command = Command::new(&executable);
                command
                    .as_std()
                    .arg("--list")
                    .stdout(std::process::Stdio::null());
                let status = command.status().await;
                if run {
                    assert!(status.unwrap().success());
                } else {
                    assert_eq!(status.unwrap_err().kind(), io::ErrorKind::Other);
                }
                assert_eq!(command.as_std().get_program(), executable.as_os_str());
                assert_eq!(command.as_std().get_args().collect::<Vec<_>>(), ["--list"]);

                command.as_std().stdout(std::process::Stdio::piped());
                let output = command.output().await;
                if run {
                    let output = output.unwrap();
                    assert!(output.status.success());
                    assert!(!output.stdout.is_empty());
                } else {
                    assert_eq!(output.unwrap_err().kind(), io::ErrorKind::Other);
                }
                assert_eq!(command.as_std().get_program(), executable.as_os_str());
                assert_eq!(command.as_std().get_args().collect::<Vec<_>>(), ["--list"]);
            });
        }
    }

    #[test]
    fn blocking_pipe_worker_panics_preserve_stream_and_buffer() {
        struct JoiningPool;
        impl crate::vibeio::blocking::BlockingThreadPool for JoiningPool {
            fn spawn(&self, task: Box<dyn FnOnce() + Send>) {
                let _ = std::thread::spawn(task).join();
            }
        }
        struct PanickingIo(usize);
        impl Read for PanickingIo {
            fn read(&mut self, _buf: &mut [u8]) -> io::Result<usize> {
                self.0 += 1;
                panic!("injected reader panic")
            }
        }
        impl Write for PanickingIo {
            fn write(&mut self, _buf: &[u8]) -> io::Result<usize> {
                self.0 += 1;
                panic!("injected writer panic")
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }
        let runtime = crate::vibeio::RuntimeBuilder::new()
            .driver(crate::vibeio::DriverKind::Mock)
            .blocking_pool(Box::new(JoiningPool))
            .build()
            .unwrap();
        runtime.block_on(async {
            let buf = b"preserved".to_vec();
            let ptr = buf.as_ptr();
            let (result, inner, buf) = read_in_blocking_pool(PanickingIo(0), buf).await;
            assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Other);
            assert_eq!(inner.0, 1);
            assert_eq!(buf, b"preserved");
            assert_eq!(buf.as_ptr(), ptr);
            let (result, inner, buf) = write_in_blocking_pool(inner, buf).await;
            assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Other);
            assert_eq!(inner.0, 2);
            assert_eq!(buf, b"preserved");
            assert_eq!(buf.as_ptr(), ptr);
        });
    }

    #[cfg(feature = "blocking-default")]
    #[test]
    fn blocking_child_reader_fills_empty_vector_and_clears_it_at_eof() {
        Runtime::new(AnyDriver::new_mock()).block_on(async {
            let reader = std::io::Cursor::new(b"hello".to_vec());
            let (result, reader, buf) = read_in_blocking_pool(reader, Vec::with_capacity(8)).await;
            assert_eq!(result.unwrap(), 5);
            assert_eq!(buf, b"hello");
            let (result, _, buf) = read_in_blocking_pool(reader, buf).await;
            assert_eq!(result.unwrap(), 0);
            assert!(buf.is_empty());
        });
    }

    async fn write_all<W: AsyncWrite>(writer: &mut W, buf: &[u8]) -> io::Result<()> {
        let mut cursor = IoBufWithCursor::new(buf.to_vec());
        while cursor.buf_len() > 0 {
            let (result, mut next) = writer.write(cursor).await;
            let written = result?;
            if written == 0 {
                return Err(io::Error::new(
                    io::ErrorKind::WriteZero,
                    "failed to write whole buffer",
                ));
            }
            next.advance(written);
            cursor = next;
        }
        Ok(())
    }

    async fn read_line<R: AsyncRead>(reader: &mut R) -> io::Result<String> {
        let mut output = Vec::new();
        loop {
            let (result, buf) = reader.read(Vec::with_capacity(64)).await;
            let read = result?;
            if read == 0 {
                break;
            }
            output.extend_from_slice(&buf[..read]);
            if output.contains(&b'\n') {
                break;
            }
        }
        Ok(String::from_utf8_lossy(&output)
            .trim_end_matches(&['\r', '\n'][..])
            .to_string())
    }

    #[test]
    fn command_spawn_stdio_roundtrip() {
        make_runtime().block_on(async {
            let mut cmd = if cfg!(windows) {
                let mut cmd = Command::new("cmd");
                cmd.args([
                    "/V:ON",
                    "/C",
                    "set /p line= & echo out:!line!& echo err:!line!>&2",
                ]);
                cmd
            } else {
                let mut cmd = Command::new("sh");
                cmd.args(["-c", "read line; echo out:$line; echo err:$line 1>&2"]);
                cmd
            };

            cmd.stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());

            let mut child = cmd.spawn().expect("spawn should succeed");
            let mut stdin = child.stdin.take().expect("stdin should be piped");
            let mut stdout = child.stdout.take().expect("stdout should be piped");
            let mut stderr = child.stderr.take().expect("stderr should be piped");

            write_all(&mut stdin, b"hello\n")
                .await
                .expect("write to stdin");
            drop(stdin);

            let out_line = read_line(&mut stdout).await.expect("read stdout");
            let err_line = read_line(&mut stderr).await.expect("read stderr");

            assert_eq!(out_line, "out:hello");
            assert_eq!(err_line, "err:hello");

            let status = child.wait().await.expect("wait succeeds");
            assert!(status.success());
        });
    }
}
