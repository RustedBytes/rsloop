//! Process utilities for spawning and managing child processes.
//!
//! This module provides async-aware wrappers around `std::process::Command` and
//! `std::process::Child`, allowing you to spawn and interact with child processes
//! without blocking the executor.
//!
//! Key types:
//! - `Command`: an async-aware builder for spawning child processes.
//! - `Child`: represents a running child process with async `wait()`, `kill()`, etc.
//! - `ChildStdin`, `ChildStdout`, `ChildStderr`: async-aware stdio streams.
//!
//! Implementation notes:
//! - On Unix, the module uses `mio`/`io_uring` drivers to register child process
//!   file descriptors for async I/O when possible. Falls back to a blocking pool
//!   when the driver is unavailable or registration fails.
//! - A background `ZombieReaper` task runs per runtime to reap exited child processes
//!   and avoid leaving zombies. This is started automatically when the runtime is
//!   created.
//! - Calling any process API outside a runtime will panic (matching the library's
//!   general behavior for runtime-only APIs).

mod reaper;

use reaper::ZombieReaper;
pub(crate) use reaper::{start_zombie_reaper, ZombieReaperMessage};

use std::cell::RefCell;
use std::ffi::OsStr;
#[cfg(unix)]
use std::future::poll_fn;
use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex};

#[cfg(unix)]
use mio::Interest;

use std::mem::ManuallyDrop;
#[cfg(unix)]
use std::os::fd::{AsRawFd, IntoRawFd, RawFd};
#[cfg(windows)]
use std::os::windows::io::{AsRawHandle, IntoRawHandle, RawHandle};

#[cfg(unix)]
use crate::driver::RegistrationMode;
use crate::executor::current_driver;
#[cfg(unix)]
use crate::fd_inner::InnerRawHandle;
use crate::io::{iobuf_to_slice, iobufmut_to_slice, AsyncRead, AsyncWrite, IoBuf, IoBufMut};
#[cfg(unix)]
use crate::op::{ReadOp, WriteOp};

pub use std::process::{ExitStatus, Output, Stdio};

#[cfg(unix)]
enum ChildIo {
    Async(ManuallyDrop<InnerRawHandle>),
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
fn configure_nonblocking(fd: RawFd, uses_completion: bool) {
    // On Linux, pipe2() already sets O_NONBLOCK at creation time, so
    // we only need to adjust flags when the mode doesn't match.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if flags == -1 {
        return;
    }

    let mut new_flags = flags | libc::O_NONBLOCK;
    if uses_completion {
        new_flags &= !libc::O_NONBLOCK;
    }
    if new_flags != flags {
        unsafe {
            libc::fcntl(fd, libc::F_SETFL, new_flags);
        }
    }
}

#[cfg(unix)]
#[inline]
fn make_child_io(fd: RawFd, interest: Interest) -> ChildIo {
    if let Some(driver) = current_driver() {
        match InnerRawHandle::new_with_driver_and_mode(
            &driver,
            fd,
            interest,
            RegistrationMode::Completion,
        ) {
            Ok(handle) => {
                let handle = ManuallyDrop::new(handle);
                configure_nonblocking(fd, handle.uses_completion());
                ChildIo::Async(handle)
            }
            Err(_) => ChildIo::Blocking,
        }
    } else {
        ChildIo::Blocking
    }
}

#[inline]
async fn read_in_blocking_pool<R, B>(inner: R, buf: B) -> (io::Result<usize>, R, B)
where
    R: Read + Send + 'static,
    B: IoBufMut,
{
    let shared = Arc::new(Mutex::new(RefCell::new(Some((inner, buf)))));
    let shared_clone = shared.clone();
    let result = crate::spawn_blocking(move || {
        let (mut inner, mut buf) = shared_clone
            .try_lock()
            .ok()
            .and_then(|rc| rc.take())
            .expect("inner/buf is none");
        let temp_slice = iobufmut_to_slice(&mut buf);
        let result = inner.read(temp_slice);
        (result, inner, buf)
    })
    .await;

    match result {
        Ok(result) => result,
        Err(_) => {
            let (inner, buf) = shared
                .try_lock()
                .ok()
                .and_then(|rc| rc.take())
                .expect("inner/buf is none");
            (Err(blocking_pool_io_error()), inner, buf)
        }
    }
}

#[inline]
async fn write_in_blocking_pool<W, B>(inner: W, buf: B) -> (io::Result<usize>, W, B)
where
    W: Write + Send + 'static,
    B: IoBuf,
{
    let shared = Arc::new(Mutex::new(RefCell::new(Some((inner, buf)))));
    let shared_clone = shared.clone();
    let result = crate::spawn_blocking(move || {
        let (mut inner, buf) = shared_clone
            .try_lock()
            .ok()
            .and_then(|rc| rc.take())
            .expect("inner/buf is none");
        let temp_slice = iobuf_to_slice(&buf);
        let result = inner.write(temp_slice);
        (result, inner, buf)
    })
    .await;

    match result {
        Ok(result) => result,
        Err(_) => {
            let (inner, buf) = shared
                .try_lock()
                .ok()
                .and_then(|rc| rc.take())
                .expect("inner/buf is none");
            (Err(blocking_pool_io_error()), inner, buf)
        }
    }
}

/// Async-aware child process stdin stream.
///
/// This type wraps `std::process::ChildStdin` and implements `AsyncWrite`
/// to allow writing to a child process's standard input without blocking
/// the executor.
///
/// # Examples
/// ```ignore
/// let mut child = Command::new("cat").stdin(Stdio::piped()).spawn()?;
/// child.stdin.as_mut().unwrap().write_all(b"hello\n").await?;
/// ```
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
/// ```ignore
/// let mut child = Command::new("echo").stdout(Stdio::piped()).spawn()?;
/// let mut buf = vec![0u8; 64];
/// let (result, buf) = child.stdout.as_mut().unwrap().read(buf).await;
/// ```
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
/// ```ignore
/// let mut child = Command::new("sh").stderr(Stdio::piped()).spawn()?;
/// let mut buf = vec![0u8; 64];
/// let (result, buf) = child.stderr.as_mut().unwrap().read(buf).await;
/// ```
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
        let io = make_child_io(inner.as_raw_fd(), Interest::WRITABLE);
        #[cfg(windows)]
        let io = ChildIo::Blocking;

        Ok(Self {
            inner: Some(inner),
            io,
        })
    }

    /// Consume this `ChildStdin` and return the underlying `std::process::ChildStdin`.
    #[inline]
    pub fn into_std(self) -> std::process::ChildStdin {
        #[cfg(not(unix))]
        let this = ManuallyDrop::new(self);
        #[cfg(unix)]
        let mut this = ManuallyDrop::new(self);
        #[cfg(unix)]
        if let ChildIo::Async(handle) = &mut this.io {
            unsafe {
                ManuallyDrop::drop(handle);
            }
        }
        let inner = unsafe { std::ptr::read(&this.inner) };
        inner.expect("child stdin is already taken")
    }

    #[inline]
    fn drop_handle(&mut self) {
        #[cfg(unix)]
        if let ChildIo::Async(handle) = &mut self.io {
            unsafe {
                ManuallyDrop::drop(handle);
            }
        }
    }
}

impl ChildStdout {
    /// Create a new `ChildStdout` from a standard library `ChildStdout`.
    #[inline]
    pub(crate) fn from_std(inner: std::process::ChildStdout) -> io::Result<Self> {
        #[cfg(unix)]
        let io = make_child_io(inner.as_raw_fd(), Interest::READABLE);
        #[cfg(windows)]
        let io = ChildIo::Blocking;

        Ok(Self {
            inner: Some(inner),
            io,
        })
    }

    /// Consume this `ChildStdout` and return the underlying `std::process::ChildStdout`.
    #[inline]
    pub fn into_std(self) -> std::process::ChildStdout {
        #[cfg(not(unix))]
        let this = ManuallyDrop::new(self);
        #[cfg(unix)]
        let mut this = ManuallyDrop::new(self);
        #[cfg(unix)]
        if let ChildIo::Async(handle) = &mut this.io {
            unsafe {
                ManuallyDrop::drop(handle);
            }
        }
        let inner = unsafe { std::ptr::read(&this.inner) };
        inner.expect("child stdout is already taken")
    }

    #[inline]
    fn drop_handle(&mut self) {
        #[cfg(unix)]
        if let ChildIo::Async(handle) = &mut self.io {
            unsafe {
                ManuallyDrop::drop(handle);
            }
        }
    }
}

impl ChildStderr {
    /// Create a new `ChildStderr` from a standard library `ChildStderr`.
    #[inline]
    pub(crate) fn from_std(inner: std::process::ChildStderr) -> io::Result<Self> {
        #[cfg(unix)]
        let io = make_child_io(inner.as_raw_fd(), Interest::READABLE);
        #[cfg(windows)]
        let io = ChildIo::Blocking;

        Ok(Self {
            inner: Some(inner),
            io,
        })
    }

    /// Consume this `ChildStderr` and return the underlying `std::process::ChildStderr`.
    #[inline]
    pub fn into_std(self) -> std::process::ChildStderr {
        #[cfg(not(unix))]
        let this = ManuallyDrop::new(self);
        #[cfg(unix)]
        let mut this = ManuallyDrop::new(self);
        #[cfg(unix)]
        if let ChildIo::Async(handle) = &mut this.io {
            unsafe {
                ManuallyDrop::drop(handle);
            }
        }
        let inner = unsafe { std::ptr::read(&this.inner) };
        inner.expect("child stderr is already taken")
    }

    #[inline]
    fn drop_handle(&mut self) {
        #[cfg(unix)]
        if let ChildIo::Async(handle) = &mut self.io {
            unsafe {
                ManuallyDrop::drop(handle);
            }
        }
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
            let shared = Arc::new(Mutex::new(RefCell::new(Some(inner))));
            let shared_clone = shared.clone();
            let result = crate::spawn_blocking(move || {
                let mut inner = shared_clone
                    .try_lock()
                    .ok()
                    .and_then(|rc| rc.take())
                    .expect("inner is none");
                let flush_result = inner.flush();
                (flush_result, inner)
            })
            .await;

            match result {
                Ok((flush_result, inner)) => {
                    self.inner = Some(inner);
                    flush_result
                }
                Err(_) => {
                    let inner = shared
                        .try_lock()
                        .ok()
                        .and_then(|rc| rc.take())
                        .expect("inner is none");
                    self.inner = Some(inner);
                    Err(blocking_pool_io_error())
                }
            }
        } else {
            let inner = self.inner.as_mut().ok_or_else(stdio_closed_error)?;
            inner.flush()
        }
    }
}

impl AsyncRead for ChildStdout {
    #[inline]
    async fn read<B: IoBufMut>(&mut self, buf: B) -> (Result<usize, io::Error>, B) {
        if buf.buf_len() == 0 {
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
            let temp_slice = iobufmut_to_slice(&mut buf);
            (inner.read(temp_slice), buf)
        }
    }
}

impl AsyncRead for ChildStderr {
    #[inline]
    async fn read<B: IoBufMut>(&mut self, buf: B) -> (Result<usize, io::Error>, B) {
        if buf.buf_len() == 0 {
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
            let temp_slice = iobufmut_to_slice(&mut buf);
            (inner.read(temp_slice), buf)
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
/// ```ignore
/// let mut child = Command::new("echo").arg("hello").spawn()?;
/// let status = child.wait().await?;
/// println!("exit status: {}", status);
/// ```
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
    pub(crate) fn from_std(mut child: std::process::Child) -> io::Result<Self> {
        let stdin = child.stdin.take().map(ChildStdin::from_std).transpose()?;
        let stdout = child.stdout.take().map(ChildStdout::from_std).transpose()?;
        let stderr = child.stderr.take().map(ChildStderr::from_std).transpose()?;
        let id = child.id();

        Ok(Self {
            inner: Some(child),
            id,
            stdin,
            stdout,
            stderr,
            reaper: ZombieReaper::new(),
        })
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
/// ```ignore
/// let status = Command::new("echo")
///     .arg("hello")
///     .status()
///     .await?;
/// println!("exit status: {}", status);
/// ```
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
    #[inline]
    pub async fn status(&mut self) -> io::Result<ExitStatus> {
        if current_driver().is_some() {
            let inner = self.inner.take().ok_or_else(command_consumed_error)?;
            let shared = Arc::new(Mutex::new(RefCell::new(Some(inner))));
            let shared_clone = shared.clone();
            let result = crate::spawn_blocking(move || {
                let mut cmd = shared_clone
                    .try_lock()
                    .ok()
                    .and_then(|rc| rc.take())
                    .expect("command is none");
                let status = cmd.status();
                (status, cmd)
            })
            .await;

            match result {
                Ok((status, cmd)) => {
                    self.inner = Some(cmd);
                    status
                }
                Err(_) => {
                    let cmd = shared
                        .try_lock()
                        .ok()
                        .and_then(|rc| rc.take())
                        .expect("command is none");
                    self.inner = Some(cmd);
                    Err(blocking_pool_io_error())
                }
            }
        } else {
            self.inner_mut().status()
        }
    }

    /// Run the process to completion and return its output.
    ///
    /// This is an async version of `std::process::Command::output`.
    #[inline]
    pub async fn output(&mut self) -> io::Result<Output> {
        if current_driver().is_some() {
            let inner = self.inner.take().ok_or_else(command_consumed_error)?;
            let shared = Arc::new(Mutex::new(RefCell::new(Some(inner))));
            let shared_clone = shared.clone();
            let result = crate::spawn_blocking(move || {
                let mut cmd = shared_clone
                    .try_lock()
                    .ok()
                    .and_then(|rc| rc.take())
                    .expect("command is none");
                let output = cmd.output();
                (output, cmd)
            })
            .await;

            match result {
                Ok((output, cmd)) => {
                    self.inner = Some(cmd);
                    output
                }
                Err(_) => {
                    let cmd = shared
                        .try_lock()
                        .ok()
                        .and_then(|rc| rc.take())
                        .expect("command is none");
                    self.inner = Some(cmd);
                    Err(blocking_pool_io_error())
                }
            }
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
    use crate::driver::AnyDriver;
    use crate::executor::Runtime;
    use crate::io::{AsyncRead, AsyncWrite, IoBufWithCursor};

    fn make_runtime() -> Runtime {
        Runtime::new(AnyDriver::new_best().expect("driver should initialize"))
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
            let (result, buf) = reader.read(vec![0u8; 64]).await;
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
