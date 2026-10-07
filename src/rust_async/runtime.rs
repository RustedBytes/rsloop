//! smol runtime adapter for PyO3's generic asyncio bridge.
use std::{
    any::Any,
    cell::RefCell,
    future::Future,
    panic::AssertUnwindSafe,
    pin::Pin,
    task::{Context, Poll},
};

use futures_util::FutureExt;
use pyo3_async_runtimes::{
    TaskLocals,
    generic::{ContextExt, JoinError, Runtime},
};

thread_local! {
    static LOCALS: RefCell<Option<TaskLocals>> = const { RefCell::new(None) };
}

pub(super) struct SmolRuntime;
pub(super) struct PanicError(Box<dyn Any + Send>);
impl JoinError for PanicError {
    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "<PanicError as JoinError>")
    )]
    fn is_panic(&self) -> bool {
        true
    }
    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "<PanicError as JoinError>")
    )]
    fn into_panic(self) -> Box<dyn Any + Send> {
        self.0
    }
}

// Generic PyO3 helpers deliberately discard some handles. Unlike smol::Task,
// dropping this handle must leave the task running.
pub(super) type Handle = Pin<Box<dyn Future<Output = Result<(), PanicError>> + Send>>;
struct DetachedHandle(Option<smol::Task<Result<(), PanicError>>>);
impl Future for DetachedHandle {
    type Output = Result<(), PanicError>;
    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "<DetachedHandle as Future>")
    )]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        Pin::new(self.get_mut().0.as_mut().expect("bridge task missing")).poll(cx)
    }
}
impl Drop for DetachedHandle {
    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "<DetachedHandle as Drop>")
    )]
    fn drop(&mut self) {
        if let Some(task) = self.0.take() {
            task.detach();
        }
    }
}
#[cfg_attr(feature = "profile", hotpath::measure)]
fn detached(task: smol::Task<Result<(), PanicError>>) -> Handle {
    Box::pin(DetachedHandle(Some(task)))
}
impl Runtime for SmolRuntime {
    type JoinError = PanicError;
    type JoinHandle = Handle;
    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "<SmolRuntime as Runtime>")
    )]
    fn spawn<F>(fut: F) -> Handle
    where
        F: Future<Output = ()> + Send + 'static,
    {
        detached(smol::spawn(async move {
            AssertUnwindSafe(fut)
                .catch_unwind()
                .await
                .map_err(PanicError)
        }))
    }
    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "<SmolRuntime as Runtime>")
    )]
    fn spawn_blocking<F>(f: F) -> Handle
    where
        F: FnOnce() + Send + 'static,
    {
        detached(smol::unblock(move || {
            std::panic::catch_unwind(AssertUnwindSafe(f)).map_err(PanicError)
        }))
    }
}
struct Restore(Option<TaskLocals>);
impl Drop for Restore {
    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "<Restore as Drop>"))]
    fn drop(&mut self) {
        LOCALS.with(|slot| {
            slot.replace(self.0.take());
        });
    }
}
struct Scoped<F> {
    future: Pin<Box<F>>,
    locals: TaskLocals,
}
impl<F: Future> Future for Scoped<F> {
    type Output = F::Output;
    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "<Scoped as Future>")
    )]
    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let _restore = Restore(LOCALS.with(|slot| slot.replace(Some(this.locals.clone()))));
        this.future.as_mut().poll(cx)
    }
}
impl ContextExt for SmolRuntime {
    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "<SmolRuntime as ContextExt>")
    )]
    fn scope<F, R>(locals: TaskLocals, fut: F) -> Pin<Box<dyn Future<Output = R> + Send>>
    where
        F: Future<Output = R> + Send + 'static,
    {
        Box::pin(Scoped {
            future: Box::pin(fut),
            locals,
        })
    }
    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "<SmolRuntime as ContextExt>")
    )]
    fn get_task_locals() -> Option<TaskLocals> {
        LOCALS.with(|slot| slot.borrow().clone())
    }
}
impl pyo3_async_runtimes::generic::LocalContextExt for SmolRuntime {
    #[cfg_attr(
        feature = "profile",
        hotpath::measure(
            impl_type = "<SmolRuntime as pyo3_async_runtimes :: generic :: LocalContextExt>"
        )
    )]
    fn scope_local<F, R>(locals: TaskLocals, fut: F) -> Pin<Box<dyn Future<Output = R>>>
    where
        F: Future<Output = R> + 'static,
    {
        Box::pin(Scoped {
            future: Box::pin(fut),
            locals,
        })
    }
}

impl pyo3_async_runtimes::generic::SpawnLocalExt for SmolRuntime {
    #[cfg_attr(
        feature = "profile",
        hotpath::measure(
            impl_type = "<SmolRuntime as pyo3_async_runtimes :: generic :: SpawnLocalExt>"
        )
    )]
    fn spawn_local<F>(fut: F) -> Handle
    where
        F: Future<Output = ()> + 'static,
    {
        super::local::spawn(async move {
            AssertUnwindSafe(fut)
                .catch_unwind()
                .await
                .map_err(PanicError)
        })
    }
}

#[cfg_attr(feature = "profile", hotpath::measure)]
/// Install task context while creating a local bridge; restore even on panic.
pub(super) fn with_locals<R>(locals: TaskLocals, operation: impl FnOnce() -> R) -> R {
    let _restore = Restore(LOCALS.with(|slot| slot.replace(Some(locals))));
    operation()
}
