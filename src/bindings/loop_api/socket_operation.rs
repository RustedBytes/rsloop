//! Raw socket I/O with immediate completion and loop-local readiness waits.
//! Python socket methods remain authoritative (including subclass overrides).
//! The detached reactor only handles an owned duplicate and Rust wake state;
//! it never calls Python while the runtime is borrowed.

use std::io;
use std::mem::ManuallyDrop;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::task::Poll;

use futures::future::poll_fn;
use futures::task::AtomicWaker;
use pyo3::exceptions::{PyOSError, PyRuntimeError, PyValueError};
use pyo3::prelude::*;
use pyo3::types::PyTuple;

use super::PyLoop;
use crate::engine::{CallbackArgs, CallbackKind, LoopCommand, LoopCore, ReadyCallback};
use crate::fd_ops;
use crate::vibeio::{InnerRawHandle, ReadinessOp, RegistrationMode};

pub(super) enum SocketAction {
    Recv(usize),
    RecvInto(Py<PyAny>),
    SendAll {
        data: Py<PyAny>,
        offset: usize,
        len: usize,
    },
    Accept,
}

impl SocketAction {
    fn writable(&self) -> bool {
        matches!(self, Self::SendAll { .. })
    }

    fn attempt(&mut self, py: Python<'_>, socket: &Py<PyAny>) -> PyResult<Option<Py<PyAny>>> {
        let result = match self {
            Self::Recv(count) => socket.call_method1(py, "recv", (*count,)),
            Self::RecvInto(buffer) => socket.call_method1(py, "recv_into", (buffer.bind(py),)),
            Self::Accept => socket.call_method0(py, "accept").and_then(|accepted| {
                accepted
                    .bind(py)
                    .get_item(0)?
                    .call_method1("setblocking", (false,))?;
                Ok(accepted)
            }),
            Self::SendAll { data, offset, len } => {
                // Bound synchronous progress so a large writable socket cannot
                // monopolize the loop. Only the remaining memoryview is sliced;
                // partial sends never copy the unsent payload into Python bytes.
                for _ in 0..16 {
                    if *offset == *len {
                        return Ok(Some(py.None()));
                    }
                    let chunk = if *offset == 0 {
                        data.bind(py).clone()
                    } else {
                        // Export only for this synchronous send. Retaining a
                        // memoryview across the wait would prevent the caller
                        // resizing its buffer immediately after cancellation.
                        // SAFETY: data is a live Python object while attached;
                        // CPython returns a new reference or sets an exception.
                        let view = unsafe {
                            Bound::from_owned_ptr_or_err(
                                py,
                                pyo3::ffi::PyMemoryView_FromObject(data.as_ptr()),
                            )
                        }?;
                        let bytes = view.call_method1("cast", ("B",))?;
                        bytes.get_item(pyo3::types::PySlice::new(
                            py,
                            *offset as isize,
                            *len as isize,
                            1,
                        ))?
                    };
                    match socket.call_method1(py, "send", (chunk,)) {
                        Ok(count) => {
                            let count: usize = count.extract(py)?;
                            if count == 0 || count > len.saturating_sub(*offset) {
                                return Err(PyOSError::new_err(
                                    "socket.send returned invalid progress",
                                ));
                            }
                            *offset += count;
                        }
                        Err(error) => return retry_or_error(py, error),
                    }
                }
                return Ok(if *offset == *len {
                    Some(py.None())
                } else {
                    None
                });
            }
        };
        match result {
            Ok(value) => Ok(Some(value)),
            Err(error) => retry_or_error(py, error),
        }
    }
}

fn retry_or_error(py: Python<'_>, error: PyErr) -> PyResult<Option<Py<PyAny>>> {
    if fd_ops::is_retryable_socket_error(py, &error)? {
        Ok(None)
    } else {
        Err(error)
    }
}

#[derive(Default)]
struct WakeState {
    done: AtomicBool,
    waker: AtomicWaker,
    error: Mutex<Option<io::Error>>,
}

#[pyclass(frozen)]
struct CancelWait(Arc<WakeState>);

#[pymethods]
impl CancelWait {
    fn __call__(&self, _future: &Bound<'_, PyAny>) {
        self.0.done.store(true, Ordering::Release);
        self.0.waker.wake();
    }
}

#[pyclass]
struct SocketOperation {
    core: Arc<LoopCore>,
    socket: Py<PyAny>,
    future: Py<PyAny>,
    action: SocketAction,
    context: Py<PyAny>,
    wake: Arc<WakeState>,
}

#[pymethods]
impl SocketOperation {
    fn __call__(slf: Py<Self>, py: Python<'_>) -> PyResult<()> {
        poll_operation(py, slf)
    }
}

pub(super) fn start<'py>(
    slf: Py<PyLoop>,
    py: Python<'py>,
    socket: Py<PyAny>,
    mut action: SocketAction,
) -> PyResult<Bound<'py, PyAny>> {
    // An immediate syscall must never block the owning loop. This is also
    // checked for wrapped sockets so their methods retain the same contract.
    if socket
        .call_method0(py, "gettimeout")?
        .extract::<Option<f64>>(py)?
        != Some(0.0)
    {
        return Err(PyValueError::new_err("the socket must be non-blocking"));
    }
    let loop_obj = slf.clone_ref(py).into_any();
    let future = if let Some(future) = super::tasks::try_fast_create_future(py, &loop_obj)? {
        future
    } else {
        loop_obj.call_method0(py, "create_future")?
    };
    match action.attempt(py, &socket) {
        Ok(Some(value)) => {
            future.call_method1(py, "set_result", (value,))?;
        }
        Err(error) => {
            future.call_method1(py, "set_exception", (error.into_value(py),))?;
        }
        Ok(None) => {
            let core = slf.borrow(py).core.clone();
            let (context, _) = crate::context::capture_context(py, None)?;
            let wake = Arc::new(WakeState::default());
            let operation = Py::new(
                py,
                SocketOperation {
                    core: core.clone(),
                    socket,
                    future: future.clone_ref(py),
                    action,
                    context,
                    wake: wake.clone(),
                },
            )?;
            future.call_method1(py, "add_done_callback", (Py::new(py, CancelWait(wake))?,))?;
            if core.on_runtime_thread() {
                arm_wait(py, operation)?;
            } else {
                // run_until_complete(loop.sock_recv(...)) constructs the Future
                // before a runtime exists. Its first retry starts on that run.
                core.schedule_callback(
                    py,
                    CallbackKind::Soon,
                    operation.into_any(),
                    PyTuple::empty(py).unbind(),
                    None,
                )?;
            }
        }
    }
    Ok(future.into_bound(py))
}

fn poll_operation(py: Python<'_>, operation: Py<SocketOperation>) -> PyResult<()> {
    let mut op = operation.borrow_mut(py);
    if op.future.call_method0(py, "done")?.extract::<bool>(py)? {
        return Ok(());
    }
    let error = op
        .wake
        .error
        .lock()
        .expect("poisoned socket wait error")
        .take();
    let result = if let Some(error) = error {
        Err(PyErr::from(error))
    } else {
        let socket = op.socket.clone_ref(py);
        op.action.attempt(py, &socket)
    };
    match result {
        Ok(Some(value)) => {
            op.future.call_method1(py, "set_result", (value,))?;
        }
        Err(error) => {
            op.future
                .call_method1(py, "set_exception", (error.into_value(py),))?;
        }
        Ok(None) => {
            drop(op);
            arm_wait(py, operation)?;
        }
    }
    Ok(())
}

fn duplicate_socket(py: Python<'_>, socket: &Py<PyAny>) -> PyResult<socket2::Socket> {
    let fd = fd_ops::fileobj_to_fd(py, socket.bind(py))?;
    #[cfg(unix)]
    let borrowed = {
        use std::os::fd::FromRawFd;
        let fd = i32::try_from(fd).map_err(|_| PyOSError::new_err("invalid socket descriptor"))?;
        // SAFETY: the Python socket owns fd. ManuallyDrop prevents closing it,
        // including when try_clone fails. Only the duplicate becomes owned.
        ManuallyDrop::new(unsafe { socket2::Socket::from_raw_fd(fd) })
    };
    #[cfg(windows)]
    let borrowed = {
        use std::os::windows::io::FromRawSocket;
        // SAFETY: same ownership contract as the Unix branch; RawFd stores
        // SOCKET's bit pattern, which may use the signed integer's high bit.
        ManuallyDrop::new(unsafe { socket2::Socket::from_raw_socket(fd as _) })
    };
    Ok(borrowed.try_clone()?)
}

fn arm_wait(py: Python<'_>, operation: Py<SocketOperation>) -> PyResult<()> {
    let op = operation.borrow(py);
    let duplicate = match duplicate_socket(py, &op.socket) {
        Ok(socket) => socket,
        Err(error) => {
            op.future
                .call_method1(py, "set_exception", (error.into_value(py),))?;
            return Ok(());
        }
    };
    let core = op.core.clone();
    let wake = op.wake.clone();
    let writable = op.action.writable();
    let context = op.context.clone_ref(py);
    let future = op.future.clone_ref(py);
    drop(op);
    let ready = Arc::new(ReadyCallback::from_args(
        core.next_callback_id(),
        CallbackKind::Soon,
        operation.into_any(),
        CallbackArgs::None,
        context,
        true,
    ));
    if !core.spawn_io(wait_socket(core.clone(), duplicate, writable, wake, ready)) {
        future.call_method1(
            py,
            "set_exception",
            (PyRuntimeError::new_err("socket wait has no running loop").into_value(py),),
        )?;
    }
    Ok(())
}

async fn wait_socket(
    core: Arc<LoopCore>,
    socket: socket2::Socket,
    writable: bool,
    wake: Arc<WakeState>,
    ready: Arc<ReadyCallback>,
) {
    #[cfg(unix)]
    let raw = {
        use std::os::fd::AsRawFd;
        socket.as_raw_fd()
    };
    #[cfg(windows)]
    let raw = {
        use std::os::windows::io::AsRawSocket;
        crate::vibeio::RawOsHandle::Socket(socket.as_raw_socket())
    };
    let interest = if writable {
        mio::Interest::WRITABLE
    } else {
        mio::Interest::READABLE
    };
    let result = match InnerRawHandle::new_with_mode(raw, interest, RegistrationMode::Poll) {
        Err(error) => Err(error),
        Ok(handle) => {
            let mut armed = false;
            poll_fn(|cx| {
                wake.waker.register(cx.waker());
                if wake.done.load(Ordering::Acquire) {
                    return Poll::Ready(Ok(false));
                }
                // Readiness is advisory: any later retry still calls the Python
                // socket and can arm another wait after a spurious notification.
                if armed {
                    return Poll::Ready(Ok(true));
                }
                let mut op = if writable {
                    ReadinessOp::new_writable(&handle)
                } else {
                    ReadinessOp::new_readable(&handle)
                };
                armed = true;
                handle
                    .poll_op_poll(cx, &mut op)
                    .map(|result| result.map(|()| true))
            })
            .await
            // handle deregisters before the duplicate is closed, on its owner.
        }
    };
    drop(socket);
    if !wake.done.load(Ordering::Acquire) {
        if let Err(error) = result {
            *wake.error.lock().expect("poisoned socket wait error") = Some(error);
        }
        let _ = core.send_command(LoopCommand::ScheduleReady(ready));
    }
}
