use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Weak,
};

use pyo3::ffi;
use pyo3::prelude::*;
use pyo3::types::PyTuple;

use crate::context::{enter_context, exit_context, is_nested_context_error};
use crate::fd_ops::RawFd;

pub type CallbackId = u64;

enum CallbackArgs {
    None,
    One(Py<PyAny>),
    Many(Py<PyTuple>),
}

#[derive(Clone, Copy, Debug)]
pub enum CallbackKind {
    Soon,
    Threadsafe,
    Timer,
    Signal(i32),
    Reader(RawFd),
    Writer(RawFd),
}

pub struct ReadyCallback {
    id: CallbackId,
    kind: CallbackKind,
    callback: Py<PyAny>,
    args: CallbackArgs,
    args_tuple: Py<PyTuple>,
    context: Py<PyAny>,
    context_needs_run: bool,
    cancelled: AtomicBool,
}

impl ReadyCallback {
    #[inline]
    pub fn new(
        py: Python<'_>,
        id: CallbackId,
        kind: CallbackKind,
        callback: Py<PyAny>,
        args_tuple: Py<PyTuple>,
        context: Py<PyAny>,
        context_needs_run: bool,
    ) -> Self {
        let args = match args_tuple.bind(py).len() {
            0 => CallbackArgs::None,
            1 => CallbackArgs::One(
                args_tuple
                    .bind(py)
                    .get_item(0)
                    .expect("single callback arg")
                    .unbind(),
            ),
            _ => CallbackArgs::Many(args_tuple.clone_ref(py)),
        };

        Self {
            id,
            kind,
            callback,
            args,
            args_tuple,
            context,
            context_needs_run,
            cancelled: AtomicBool::new(false),
        }
    }

    #[inline]
    pub fn id(&self) -> CallbackId {
        self.id
    }

    #[inline]
    pub fn kind(&self) -> CallbackKind {
        self.kind
    }

    #[inline]
    pub fn callback(&self) -> &Py<PyAny> {
        &self.callback
    }

    #[inline]
    pub fn context(&self) -> &Py<PyAny> {
        &self.context
    }

    #[inline]
    pub fn context_needs_run(&self) -> bool {
        self.context_needs_run
    }

    pub fn invoke(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        profiling::scope!("ReadyCallback::invoke");
        if !self.context_needs_run {
            return self.invoke_direct(py);
        }

        if let Err(err) = enter_context(py, &self.context) {
            return if is_nested_context_error(py, &err) {
                self.invoke_direct(py)
            } else {
                Err(err)
            };
        }

        let callback_result = self.invoke_direct(py);
        let exit_result = exit_context(py, &self.context);

        match (callback_result, exit_result) {
            (Ok(result), Ok(())) => Ok(result),
            (Err(err), _) => Err(err),
            (Ok(_), Err(err)) => Err(err),
        }
    }

    fn invoke_direct(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        profiling::scope!("ReadyCallback::invoke_direct");
        match &self.args {
            // SAFETY: `self.callback` is a live Python callable owned by `Py<PyAny>`, and the GIL
            // token proves this thread may call into CPython. The returned owned pointer is
            // immediately converted into a PyO3 `Bound`, which handles null/error propagation.
            CallbackArgs::None => unsafe {
                Bound::from_owned_ptr_or_err(
                    py,
                    ffi::compat::PyObject_CallNoArgs(self.callback.as_ptr()),
                )
                .map(Bound::unbind)
            },
            // SAFETY: `self.callback` and `arg` are live Python objects and the varargs list is
            // terminated with a null pointer as required by `PyObject_CallFunctionObjArgs`.
            // PyO3 converts a null return into `PyErr` and takes ownership of successful results.
            CallbackArgs::One(arg) => unsafe {
                Bound::from_owned_ptr_or_err(
                    py,
                    ffi::PyObject_CallFunctionObjArgs(
                        self.callback.as_ptr(),
                        arg.as_ptr(),
                        std::ptr::null_mut::<ffi::PyObject>(),
                    ),
                )
                .map(Bound::unbind)
            },
            CallbackArgs::Many(args) => self.callback.call1(py, args.clone_ref(py)),
        }
    }

    pub fn clone_args_tuple(&self, py: Python<'_>) -> Py<PyTuple> {
        self.args_tuple.clone_ref(py)
    }

    #[inline]
    pub fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    #[inline]
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

#[pyclass(name = "Handle", module = "rsloop._loop", weakref, freelist = 4096)]
pub struct PyHandle {
    callback_id: CallbackId,
    callback: Weak<ReadyCallback>,
    cancelled: AtomicBool,
}

impl PyHandle {
    #[inline]
    pub fn new(callback_id: CallbackId, callback: &Arc<ReadyCallback>) -> Self {
        Self {
            callback_id,
            callback: Arc::downgrade(callback),
            cancelled: AtomicBool::new(false),
        }
    }
}

#[pymethods]
impl PyHandle {
    fn cancel(&self) -> PyResult<()> {
        self.cancelled.store(true, Ordering::Relaxed);
        if let Some(callback) = self.callback.upgrade() {
            callback.cancel();
        }
        Ok(())
    }

    fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    fn __repr__(&self) -> String {
        format!(
            "<Handle id={} cancelled={}>",
            self.callback_id,
            self.cancelled()
        )
    }
}

#[pyclass(
    name = "TimerHandle",
    module = "rsloop._loop",
    weakref,
    freelist = 1024
)]
pub struct PyTimerHandle {
    callback_id: CallbackId,
    when: f64,
    callback: Weak<ReadyCallback>,
    cancelled: AtomicBool,
}

impl PyTimerHandle {
    #[inline]
    pub fn new(callback_id: CallbackId, when: f64, callback: &Arc<ReadyCallback>) -> Self {
        Self {
            callback_id,
            when,
            callback: Arc::downgrade(callback),
            cancelled: AtomicBool::new(false),
        }
    }
}

#[pymethods]
impl PyTimerHandle {
    fn cancel(&self) -> PyResult<()> {
        self.cancelled.store(true, Ordering::Relaxed);
        if let Some(callback) = self.callback.upgrade() {
            callback.cancel();
        }
        Ok(())
    }

    fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    fn when(&self) -> f64 {
        self.when
    }

    fn __repr__(&self) -> String {
        format!(
            "<TimerHandle id={} when={:.6} cancelled={}>",
            self.callback_id,
            self.when,
            self.cancelled()
        )
    }
}
