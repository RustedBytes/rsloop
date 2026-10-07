//! Poll !Send bridge tasks exclusively on the calling Python loop thread.
use std::{
    cell::RefCell,
    future::Future,
    pin::Pin,
    sync::Arc,
    task::{Context, Wake, Waker},
};

use pyo3::prelude::*;

use super::runtime::{Handle, PanicError};

#[pyclass(unsendable)]
struct PollLocal {
    future: RefCell<Option<Pin<Box<dyn Future<Output = ()>>>>>,
    event_loop: Py<PyAny>,
}
struct Schedule {
    callback: Py<PyAny>,
    event_loop: Py<PyAny>,
}
impl Wake for Schedule {
    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "<Schedule as Wake>")
    )]
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }
    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "<Schedule as Wake>")
    )]
    fn wake_by_ref(self: &Arc<Self>) {
        Python::attach(|py| {
            if let Err(err) =
                self.event_loop
                    .call_method1(py, "call_soon_threadsafe", (&self.callback,))
            {
                err.write_unraisable(py, Some(self.callback.bind(py)));
            }
        });
    }
}
#[pymethods]
impl PollLocal {
    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "PollLocal"))]
    fn __call__(slf: Bound<'_, Self>) {
        let py = slf.py();
        let state = slf.borrow();
        let mut future = state.future.borrow_mut();
        let Some(task) = future.as_mut() else {
            return;
        };
        let waker = Waker::from(Arc::new(Schedule {
            callback: slf.clone().into_any().unbind(),
            event_loop: state.event_loop.clone_ref(py),
        }));
        if task
            .as_mut()
            .poll(&mut Context::from_waker(&waker))
            .is_ready()
        {
            future.take();
        }
    }
}
#[cfg_attr(feature = "profile", hotpath::measure)]
pub(super) fn spawn(fut: impl Future<Output = Result<(), PanicError>> + 'static) -> Handle {
    let (tx, rx) = futures::channel::oneshot::channel();
    Python::attach(|py| {
        let event_loop = super::get_current_locals(py)
            .expect("local bridge requires a running Python loop")
            .event_loop(py)
            .unbind();
        let callback = Py::new(
            py,
            PollLocal {
                future: RefCell::new(Some(Box::pin(async move {
                    let _ = tx.send(fut.await);
                }))),
                event_loop: event_loop.clone_ref(py),
            },
        )
        .expect("allocate local bridge callback");
        event_loop
            .call_method1(py, "call_soon", (callback,))
            .expect("schedule local bridge callback");
    });
    Box::pin(async move {
        rx.await
            .expect("local bridge worker terminated without a result")
    })
}
