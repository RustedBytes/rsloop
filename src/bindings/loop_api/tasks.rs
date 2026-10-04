//! `Future` and `Task` construction.
//!
//! `create_task` runs once per scheduled coroutine, so this module is written
//! around avoiding work on the common call. When the loop is the running loop and
//! no task factory or keyword is involved, `asyncio.Task(coro)` is called
//! directly — no `loop=` keyword, no kwargs dict, and on 3.11+ no `**` unpacking,
//! because the constructor is reached through a vectorcall.

use std::sync::Arc;

use pyo3::exceptions::PyTypeError;
use pyo3::prelude::*;
use pyo3::types::PyDict;

use super::PyLoop;
use super::asyncio_cache::{
    asyncio_future_cls, asyncio_get_running_loop_fn, asyncio_task_cls, asyncio_task_kwarg_support,
    call_callable_noargs, call_callable_onearg,
};
#[cfg(any(Py_3_12, all(Py_3_11, not(Py_LIMITED_API))))]
use super::asyncio_cache::{asyncio_future_loop_kwnames, asyncio_task_kwnames_for_options};
#[cfg(any(Py_3_12, all(Py_3_11, not(Py_LIMITED_API))))]
use super::ffi_helpers;

/// The keyword-only arguments `loop.create_task()` accepts, plus any extras the
/// caller passed for a custom task factory.
pub(super) struct TaskOptions {
    pub(super) name: Option<Py<PyAny>>,
    pub(super) context: Option<Py<PyAny>>,
    pub(super) eager_start: Option<bool>,
    pub(super) kwargs: Option<Py<PyDict>>,
}

fn is_current_running_loop(py: Python<'_>, loop_obj: &Py<PyAny>) -> PyResult<bool> {
    let current = asyncio_get_running_loop_fn(py)?.call0(py)?;
    if current.is_none(py) {
        return Ok(false);
    }
    Ok(current.bind(py).is(loop_obj.bind(py)))
}

fn create_asyncio_future_for_loop(py: Python<'_>, loop_obj: &Py<PyAny>) -> PyResult<Py<PyAny>> {
    #[cfg(any(Py_3_12, all(Py_3_11, not(Py_LIMITED_API))))]
    {
        let args = [loop_obj.as_ptr()];
        let cls = asyncio_future_cls(py)?.as_ptr();
        let kwnames = asyncio_future_loop_kwnames(py)?.as_ptr();
        ffi_helpers::vectorcall(py, cls, args.as_ptr(), 0, kwnames)
    }

    #[cfg(not(any(Py_3_12, all(Py_3_11, not(Py_LIMITED_API)))))]
    {
        let kwargs = PyDict::new(py);
        kwargs.set_item("loop", loop_obj.clone_ref(py))?;
        asyncio_future_cls(py)?.call(py, (), Some(&kwargs))
    }
}

fn create_asyncio_future_for_running_loop(py: Python<'_>) -> PyResult<Py<PyAny>> {
    call_callable_noargs(py, asyncio_future_cls(py)?)
}

/// Fast-path future creation for internal callers that hold a loop object:
/// when `loop_obj` is exactly a `PyLoop` running on this thread, skip the
/// Python-level `create_future` method dispatch. Returns `Ok(None)` when the
/// caller must fall back to calling `loop.create_future()`.
pub(crate) fn try_fast_create_future(
    py: Python<'_>,
    loop_obj: &Py<PyAny>,
) -> PyResult<Option<Py<PyAny>>> {
    let Ok(pyloop) = loop_obj.bind(py).cast_exact::<PyLoop>() else {
        return Ok(None);
    };
    if !pyloop.get().core.on_runtime_thread() {
        return Ok(None);
    }
    create_asyncio_future_for_running_loop(py).map(Some)
}

pub(crate) fn try_fast_create_task(
    py: Python<'_>,
    loop_obj: &Py<PyAny>,
    coro: Py<PyAny>,
) -> PyResult<Option<Py<PyAny>>> {
    let Ok(pyloop) = loop_obj.bind(py).cast_exact::<PyLoop>() else {
        return Ok(None);
    };
    let core = &pyloop.get().core;
    if !core.on_runtime_thread() || core.has_task_factory() {
        return Ok(None);
    }
    create_asyncio_task_for_running_loop(py, loop_obj.bind(py), coro).map(Some)
}

fn create_asyncio_task_for_loop(
    py: Python<'_>,
    loop_obj: &Py<PyAny>,
    coro: Py<PyAny>,
    name: Option<Py<PyAny>>,
    context: Option<Py<PyAny>>,
) -> PyResult<Py<PyAny>> {
    #[cfg(any(Py_3_12, all(Py_3_11, not(Py_LIMITED_API))))]
    {
        let name = name.as_ref();
        let context = context.as_ref();
        // Vectorcall borrows at most four pointers for this synchronous call;
        // constructor keyword options do not need a heap allocation.
        let mut args = [
            coro.as_ptr(),
            loop_obj.as_ptr(),
            std::ptr::null_mut(),
            std::ptr::null_mut(),
        ];
        let mut next = 2;
        if let Some(name) = name {
            args[next] = name.as_ptr();
            next += 1;
        }
        if let Some(context) = context {
            args[next] = context.as_ptr();
        }

        let cls = asyncio_task_cls(py)?.as_ptr();
        let kwnames = asyncio_task_kwnames_for_options(py, name.is_some(), context.is_some())?;
        ffi_helpers::vectorcall(py, cls, args.as_ptr(), 1, kwnames.as_ptr())
    }

    #[cfg(not(any(Py_3_12, all(Py_3_11, not(Py_LIMITED_API)))))]
    {
        let kwargs = PyDict::new(py);
        kwargs.set_item("loop", loop_obj.clone_ref(py))?;
        if let Some(name) = name {
            kwargs.set_item("name", name)?;
        }
        if let Some(context) = context {
            kwargs.set_item("context", context)?;
        }
        asyncio_task_cls(py)?.call(py, (coro,), Some(&kwargs))
    }
}

#[inline]
fn create_asyncio_task_for_running_loop(
    py: Python<'_>,
    _loop_obj: &Bound<'_, PyAny>,
    coro: Py<PyAny>,
) -> PyResult<Py<PyAny>> {
    let task_cls = asyncio_task_cls(py)?;
    call_callable_onearg(py, task_cls, coro.bind(py))
}

fn create_asyncio_task_with_kwargs(
    py: Python<'_>,
    loop_obj: Option<&Py<PyAny>>,
    coro: Py<PyAny>,
    kwargs: &Bound<'_, PyDict>,
) -> PyResult<Py<PyAny>> {
    let task_kwargs = kwargs.copy()?;
    if let Some(loop_obj) = loop_obj {
        task_kwargs.set_item("loop", loop_obj.clone_ref(py))?;
    }
    asyncio_task_cls(py)?.call(py, (coro,), Some(&task_kwargs))
}

fn trim_task_source_traceback(py: Python<'_>, task: &Py<PyAny>) -> PyResult<()> {
    let Ok(source_traceback) = task.getattr(py, "_source_traceback") else {
        return Ok(());
    };
    if source_traceback.is_none(py) {
        return Ok(());
    }

    let source_traceback = source_traceback.bind(py);
    if source_traceback.len()? == 0 {
        return Ok(());
    }

    source_traceback.del_item(source_traceback.len()? - 1)
}

pub(super) fn create_future(slf: Py<PyLoop>, py: Python<'_>) -> PyResult<Py<PyAny>> {
    if slf.get().core.on_runtime_thread() {
        return create_asyncio_future_for_running_loop(py);
    }

    let loop_obj = PyLoop::as_py_any(py, &slf);
    if is_current_running_loop(py, &loop_obj)? {
        return create_asyncio_future_for_running_loop(py);
    }

    create_asyncio_future_for_loop(py, &loop_obj)
}

pub(super) fn create_task(
    slf: Py<PyLoop>,
    py: Python<'_>,
    coro: Py<PyAny>,
    options: TaskOptions,
) -> PyResult<Py<PyAny>> {
    let TaskOptions {
        name,
        context,
        eager_start,
        kwargs,
    } = options;

    // Hot path: a bare `create_task(coro)` with no name/context/eager_start/
    // kwargs — what asyncio.Task step scheduling and gather() always hit.
    // Skip the Arc clone and the (cached) kwarg-support probe those extras
    // would need, and go straight to constructing the Task.
    let bare = name.is_none()
        && context.is_none()
        && eager_start.is_none()
        && kwargs
            .as_ref()
            .is_none_or(|kwargs| kwargs.bind(py).is_empty());
    if bare {
        let loop_ref = slf.get();
        if !loop_ref.core.has_task_factory() && loop_ref.core.on_runtime_thread() {
            return create_asyncio_task_for_running_loop(py, slf.bind(py).as_any(), coro);
        }
    }

    let core = Arc::clone(&slf.get().core);
    let task_kwarg_support = asyncio_task_kwarg_support(py)?;
    let extra_kwargs = kwargs
        .as_ref()
        .is_some_and(|kwargs| !kwargs.bind(py).is_empty());
    let has_kwargs = extra_kwargs
        || name.is_some()
        || (context.is_some() && task_kwarg_support.context)
        || (eager_start.is_some() && task_kwarg_support.eager_start);

    if !core.has_task_factory() && !has_kwargs && core.on_runtime_thread() {
        return create_asyncio_task_for_running_loop(py, slf.bind(py).as_any(), coro);
    }

    let loop_obj = PyLoop::as_py_any(py, &slf);

    let task_factory = if core.has_task_factory() {
        core.state
            .lock()
            .expect("poisoned loop state")
            .task_factory
            .as_ref()
            .map(|factory| factory.clone_ref(py))
    } else {
        None
    };

    if task_factory.is_none() && extra_kwargs {
        let unexpected = kwargs
            .as_ref()
            .and_then(|kwargs| kwargs.bind(py).iter().next().map(|(key, _)| key))
            .expect("non-empty kwargs when extra_kwargs is true");
        let unexpected = unexpected.repr()?.extract::<String>()?;
        return Err(PyTypeError::new_err(format!(
            "create_task() got an unexpected keyword argument {unexpected}"
        )));
    }

    // Ordinary Task options fit the cached vectorcall keyword layouts. Avoid
    // constructing a kwargs dict and then copying it in the generic path.
    // An explicit loop is correct both inside and outside a running loop and
    // also avoids a Python _get_running_loop call here. Custom factories and
    // eager_start retain their existing compatibility path below.
    if task_factory.is_none() && eager_start.is_none() {
        let created = create_asyncio_task_for_loop(
            py,
            &loop_obj,
            coro,
            name.filter(|_| task_kwarg_support.name),
            context.filter(|_| task_kwarg_support.context),
        )?;
        if core.get_debug() {
            trim_task_source_traceback(py, &created)?;
        }
        return Ok(created);
    }

    let task_kwargs = if has_kwargs || task_factory.is_some() {
        let task_kwargs = PyDict::new(py);
        if let Some(kwargs_in) = kwargs.as_ref() {
            for (key, value) in kwargs_in.bind(py).iter() {
                task_kwargs.set_item(key, value)?;
            }
        }
        if task_factory.is_some() {
            let factory_name = name
                .as_ref()
                .map(|name| name.clone_ref(py))
                .unwrap_or_else(|| py.None());
            task_kwargs.set_item("name", factory_name)?;
        } else if task_kwarg_support.name
            && let Some(name) = name.as_ref()
        {
            task_kwargs.set_item("name", name)?;
        }
        if let Some(context) = context.as_ref()
            && (task_factory.is_some() || task_kwarg_support.context)
        {
            task_kwargs.set_item("context", context)?;
        }
        if let Some(eager_start) = eager_start
            && (task_factory.is_some() || task_kwarg_support.eager_start)
        {
            task_kwargs.set_item("eager_start", eager_start)?;
        }
        Some(task_kwargs)
    } else {
        None
    };

    if let Some(factory) = task_factory {
        let created = factory.call(py, (loop_obj.clone_ref(py), coro), task_kwargs.as_ref())?;
        return Ok(created);
    }

    let trim_source_traceback = core.get_debug();
    if is_current_running_loop(py, &loop_obj)? {
        let created = if has_kwargs {
            create_asyncio_task_with_kwargs(
                py,
                None,
                coro,
                task_kwargs.as_ref().expect("task kwargs"),
            )?
        } else {
            create_asyncio_task_for_running_loop(py, loop_obj.bind(py), coro)?
        };
        if trim_source_traceback {
            trim_task_source_traceback(py, &created)?;
        }
        return Ok(created);
    }

    let created = if has_kwargs {
        create_asyncio_task_with_kwargs(
            py,
            Some(&loop_obj),
            coro,
            task_kwargs.as_ref().expect("task kwargs"),
        )?
    } else {
        create_asyncio_task_for_loop(py, &loop_obj, coro, name, context)?
    };
    if trim_source_traceback {
        trim_task_source_traceback(py, &created)?;
    }
    Ok(created)
}
