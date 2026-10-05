//! Callback values and Python handle wrappers used by the loop engine.

use std::sync::{
    Arc, Weak,
    atomic::{AtomicBool, Ordering},
};

use pyo3::{prelude::*, types::PyTuple};

use crate::{
    context::{enter_context, exit_context, is_nested_context_error},
    fd_ops::RawFd,
};

pub type CallbackId = u64;

#[inline]
#[cfg_attr(feature = "profile", hotpath::measure)]
fn call_callback_noargs(py: Python<'_>, callback: &Py<PyAny>) -> PyResult<Py<PyAny>> {
    Ok(callback.bind(py).call0()?.unbind())
}

#[inline]
#[cfg_attr(feature = "profile", hotpath::measure)]
fn call_callback_onearg(
    py: Python<'_>,
    callback: &Py<PyAny>,
    arg: &Py<PyAny>,
) -> PyResult<Py<PyAny>> {
    Ok(callback.bind(py).call1((arg.bind(py),))?.unbind())
}

pub(crate) enum CallbackArgs {
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

// Store the tag separately from its payload so ReadyCallback's booleans can
// share the tag's padding. Keeping CallbackKind inline costs another word on
// 64-bit targets. The payload retains the full cross-platform RawFd range.
#[derive(Clone, Copy)]
enum CallbackSource {
    Soon,
    Threadsafe,
    Timer,
    Signal,
    Reader,
    Writer,
}

/// A Python callback, its positional arguments, and captured execution context.
///
/// The value is safe to enqueue across rsloop's worker threads. Invocation must
/// still happen while attached to Python, normally on the event-loop thread.
pub struct ReadyCallback {
    id: CallbackId,
    source: CallbackSource,
    source_value: RawFd,
    callback: Py<PyAny>,
    args: CallbackArgs,
    context: Py<PyAny>,
    context_needs_run: bool,
    cancelled: AtomicBool,
}

impl ReadyCallback {
    #[inline]
    /// Builds a ready callback and selects a zero-, one-, or many-argument fast
    /// path.
    ///
    /// `context_needs_run` records whether invocation must enter `context`.
    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "ReadyCallback")
    )]
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
            _ => CallbackArgs::Many(args_tuple),
        };

        Self::from_args(id, kind, callback, args, context, context_needs_run)
    }

    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "ReadyCallback")
    )]
    pub(crate) fn from_args(
        id: CallbackId,
        kind: CallbackKind,
        callback: Py<PyAny>,
        args: CallbackArgs,
        context: Py<PyAny>,
        context_needs_run: bool,
    ) -> Self {
        let (source, source_value) = match kind {
            CallbackKind::Soon => (CallbackSource::Soon, 0),
            CallbackKind::Threadsafe => (CallbackSource::Threadsafe, 0),
            CallbackKind::Timer => (CallbackSource::Timer, 0),
            CallbackKind::Signal(signal) => (CallbackSource::Signal, RawFd::from(signal)),
            CallbackKind::Reader(fd) => (CallbackSource::Reader, fd),
            CallbackKind::Writer(fd) => (CallbackSource::Writer, fd),
        };
        Self {
            id,
            source,
            source_value,
            callback,
            args,
            context,
            context_needs_run,
            cancelled: AtomicBool::new(false),
        }
    }

    #[inline]
    /// Returns the loop-unique identifier assigned to this callback.
    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "ReadyCallback")
    )]
    pub fn id(&self) -> CallbackId {
        self.id
    }

    #[inline]
    /// Returns the scheduling source used for diagnostics and re-arming I/O.
    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "ReadyCallback")
    )]
    pub fn kind(&self) -> CallbackKind {
        match self.source {
            CallbackSource::Soon => CallbackKind::Soon,
            CallbackSource::Threadsafe => CallbackKind::Threadsafe,
            CallbackSource::Timer => CallbackKind::Timer,
            // Only an i32 signal number can initialize this source variant.
            CallbackSource::Signal => CallbackKind::Signal(self.source_value as i32),
            CallbackSource::Reader => CallbackKind::Reader(self.source_value),
            CallbackSource::Writer => CallbackKind::Writer(self.source_value),
        }
    }

    #[inline]
    /// Borrows the underlying Python callable.
    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "ReadyCallback")
    )]
    pub fn callback(&self) -> &Py<PyAny> {
        &self.callback
    }

    #[inline]
    /// Borrows the captured Python `contextvars.Context`.
    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "ReadyCallback")
    )]
    pub fn context(&self) -> &Py<PyAny> {
        &self.context
    }

    #[inline]
    /// Reports whether invocation needs to enter the captured context.
    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "ReadyCallback")
    )]
    pub fn context_needs_run(&self) -> bool {
        self.context_needs_run
    }

    /// Invokes the callback with its stored arguments and execution context.
    ///
    /// A nested-context error falls back to direct invocation because that
    /// means the desired context is already active on this thread.
    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "ReadyCallback")
    )]
    pub fn invoke(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
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
            (Err(err), _) | (Ok(_), Err(err)) => Err(err),
        }
    }

    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "ReadyCallback")
    )]
    fn invoke_direct(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        match &self.args {
            CallbackArgs::None => call_callback_noargs(py, &self.callback),
            CallbackArgs::One(arg) => call_callback_onearg(py, &self.callback, arg),
            CallbackArgs::Many(args) => self.callback.call1(py, args),
        }
    }

    #[inline]
    /// Reports whether this callback has been cancelled.
    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "ReadyCallback")
    )]
    pub fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    #[inline]
    /// Marks the callback as cancelled.
    ///
    /// Cancellation is idempotent and does not remove an already queued value;
    /// the loop skips it when draining the ready queue.
    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "ReadyCallback")
    )]
    pub fn cancel(&self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

/// Python's cancellable `asyncio.Handle` equivalent.
///
/// The handle owns its callback inline and the ready queue holds `Py<PyHandle>`
/// clones, requiring one allocation per `call_soon`. The class is frozen so
/// the event loop can read the callback without dynamic borrow checking.
/// A PyO3 freelist slowed large callback bursts: their handles remain live
/// until the ready batch drains, so allocation usually finds the list empty.
#[pyclass(name = "Handle", module = "rsloop._loop", weakref, frozen)]
pub struct PyHandle {
    callback: ReadyCallback,
}

impl PyHandle {
    #[inline]
    /// Wraps a callback in a Python-visible handle.
    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "PyHandle"))]
    pub fn new(callback: ReadyCallback) -> Self {
        Self { callback }
    }

    #[inline]
    /// Borrows the callback controlled by this handle.
    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "PyHandle"))]
    pub fn ready(&self) -> &ReadyCallback {
        &self.callback
    }
}

#[pymethods]
impl PyHandle {
    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "PyHandle"))]
    fn cancel(&self) -> PyResult<()> {
        self.callback.cancel();
        Ok(())
    }

    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "PyHandle"))]
    fn cancelled(&self) -> bool {
        self.callback.cancelled()
    }

    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "PyHandle"))]
    fn __repr__(&self) -> String {
        format!(
            "<Handle id={} cancelled={}>",
            self.callback.id(),
            self.cancelled()
        )
    }
}

/// Python's cancellable `asyncio.TimerHandle` equivalent.
///
/// The weak callback reference lets cancellation update a live scheduled
/// callback without keeping it alive after the timer queue releases it.
#[pyclass(
    name = "TimerHandle",
    module = "rsloop._loop",
    weakref,
    frozen,
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
    /// Creates a timer handle for a callback scheduled at loop time `when`.
    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "PyTimerHandle")
    )]
    pub fn new(callback_id: CallbackId, when: f64, callback: &Arc<ReadyCallback>) -> Self {
        Self {
            callback_id,
            when,
            callback: Arc::downgrade(callback),
            cancelled: AtomicBool::new(false),
        }
    }
}

#[cfg(test)]
mod tests {
    use pyo3::{ffi::c_str, types::PyTuple};

    use super::*;
    use crate::context::capture_context;

    fn callback_with_args(py: Python<'_>, id: CallbackId, args: Py<PyTuple>) -> ReadyCallback {
        let callback = py
            .eval(c_str!("lambda *args: args"), None, None)
            .expect("build callback")
            .unbind();
        let (context, needs_run) = capture_context(py, None).expect("capture context");
        ReadyCallback::new(
            py,
            id,
            CallbackKind::Soon,
            callback,
            args,
            context,
            needs_run,
        )
    }

    #[test]
    fn callback_source_preserves_signal_and_descriptor_ranges() {
        crate::initialize_python_for_tests();
        Python::attach(|py| {
            for kind in [
                CallbackKind::Soon,
                CallbackKind::Threadsafe,
                CallbackKind::Timer,
                CallbackKind::Signal(i32::MIN),
                CallbackKind::Signal(i32::MAX),
                CallbackKind::Reader(RawFd::MIN),
                CallbackKind::Reader(RawFd::MAX),
                CallbackKind::Writer(RawFd::MIN),
                CallbackKind::Writer(RawFd::MAX),
            ] {
                let ready = ReadyCallback::from_args(
                    1,
                    kind,
                    py.None(),
                    CallbackArgs::None,
                    py.None(),
                    false,
                );
                match (kind, ready.kind()) {
                    (CallbackKind::Soon, CallbackKind::Soon)
                    | (CallbackKind::Threadsafe, CallbackKind::Threadsafe)
                    | (CallbackKind::Timer, CallbackKind::Timer) => {}
                    (CallbackKind::Signal(expected), CallbackKind::Signal(actual)) => {
                        assert_eq!(actual, expected);
                    }
                    (CallbackKind::Reader(expected), CallbackKind::Reader(actual))
                    | (CallbackKind::Writer(expected), CallbackKind::Writer(actual)) => {
                        assert_eq!(actual, expected);
                    }
                    (expected, actual) => panic!("source changed: {expected:?} -> {actual:?}"),
                }
            }
        });
    }

    #[test]
    fn ready_callback_invokes_zero_one_and_many_argument_fast_paths() {
        crate::initialize_python_for_tests();
        Python::attach(|py| {
            for (id, values) in [(1, Vec::<i32>::new()), (2, vec![10]), (3, vec![10, 20, 30])] {
                let args = PyTuple::new(py, &values).expect("callback args").unbind();
                let ready = callback_with_args(py, id, args);
                let result = ready.invoke(py).expect("invoke callback");
                assert_eq!(
                    result.extract::<Vec<i32>>(py).expect("tuple result"),
                    values
                );
            }
        });
    }

    #[test]
    fn ready_callback_runs_in_its_captured_context() {
        crate::initialize_python_for_tests();
        Python::attach(|py| {
            let contextvars = py.import("contextvars").expect("import contextvars");
            let var = contextvars
                .getattr("ContextVar")
                .expect("ContextVar")
                .call1(("ready_value",))
                .expect("create ContextVar");
            var.call_method1("set", ("captured",))
                .expect("set captured value");
            let callback = var.getattr("get").expect("ContextVar.get").unbind();
            let (context, needs_run) = capture_context(py, None).expect("capture context");
            var.call_method1("set", ("ambient",))
                .expect("set ambient value");
            let ready = ReadyCallback::new(
                py,
                7,
                CallbackKind::Timer,
                callback,
                PyTuple::empty(py).unbind(),
                context,
                needs_run,
            );

            assert_eq!(
                ready
                    .invoke(py)
                    .expect("invoke callback")
                    .extract::<String>(py)
                    .expect("callback string"),
                "captured"
            );
            assert_eq!(
                var.call_method0("get")
                    .expect("read ambient value")
                    .extract::<String>()
                    .expect("ambient string"),
                "ambient"
            );
        });
    }

    #[test]
    fn handle_cancellation_and_metadata_are_consistent() {
        crate::initialize_python_for_tests();
        Python::attach(|py| {
            let ready = callback_with_args(py, 99, PyTuple::empty(py).unbind());
            assert_eq!(ready.id(), 99);
            assert!(matches!(ready.kind(), CallbackKind::Soon));
            assert!(!ready.cancelled());
            assert!(!ready.callback().bind(py).is_none());
            assert!(!ready.context().bind(py).is_none());
            assert!(ready.context_needs_run());

            let handle = PyHandle::new(ready);
            assert!(!handle.cancelled());
            handle.cancel().expect("cancel handle");
            assert!(handle.cancelled());
            assert!(handle.ready().cancelled());
            assert_eq!(handle.__repr__(), "<Handle id=99 cancelled=true>");
        });
    }

    #[test]
    fn timer_handle_cancels_live_callback_and_tolerates_a_dropped_one() {
        crate::initialize_python_for_tests();
        Python::attach(|py| {
            let ready = Arc::new(callback_with_args(py, 12, PyTuple::empty(py).unbind()));
            let timer = PyTimerHandle::new(12, 1.25, &ready);
            assert!((timer.when() - 1.25).abs() < f64::EPSILON);
            assert!(!timer.cancelled());

            timer.cancel().expect("cancel timer");
            assert!(timer.cancelled());
            assert!(ready.cancelled());
            assert_eq!(
                timer.__repr__(),
                "<TimerHandle id=12 when=1.250000 cancelled=true>"
            );

            let dropped = Arc::new(callback_with_args(py, 13, PyTuple::empty(py).unbind()));
            let dropped_timer = PyTimerHandle::new(13, 2.0, &dropped);
            drop(dropped);
            dropped_timer
                .cancel()
                .expect("cancelling expired weak callback is harmless");
            assert!(dropped_timer.cancelled());
        });
    }
}

#[pymethods]
impl PyTimerHandle {
    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "PyTimerHandle")
    )]
    fn cancel(&self) -> PyResult<()> {
        self.cancelled.store(true, Ordering::Relaxed);
        if let Some(callback) = self.callback.upgrade() {
            callback.cancel();
        }
        Ok(())
    }

    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "PyTimerHandle")
    )]
    fn cancelled(&self) -> bool {
        self.cancelled.load(Ordering::Relaxed)
    }

    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "PyTimerHandle")
    )]
    fn when(&self) -> f64 {
        self.when
    }

    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "PyTimerHandle")
    )]
    fn __repr__(&self) -> String {
        format!(
            "<TimerHandle id={} when={:.6} cancelled={}>",
            self.callback_id,
            self.when,
            self.cancelled()
        )
    }
}
