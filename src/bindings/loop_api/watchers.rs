//! `add_reader` / `add_writer` descriptor registrations.
//!
//! Each registration keeps one persistent [`ReadyCallback`]: every readiness
//! event schedules that same object, and removal cancels it so events already
//! queued for the old registration are skipped. The keepalive map also holds
//! the Python file object, because the caller may drop its last reference while
//! the watch is live.

use std::sync::Arc;

use pyo3::{prelude::*, types::PyTuple};

use super::PyLoop;
use crate::{
    context::capture_context,
    engine::{CallbackKind, FdWatch, LoopCommand, LoopIoCommand, ReadyCallback},
    fd_ops,
};

#[cfg_attr(feature = "profile", hotpath::measure)]
pub(super) fn add_reader(
    loop_ref: &PyLoop,
    py: Python<'_>,
    fd: &Bound<'_, PyAny>,
    callback: Py<PyAny>,
    args: &Bound<'_, PyTuple>,
) -> PyResult<()> {
    let raw_fd = fd_ops::fileobj_to_fd(py, fd)?;
    let (context, context_needs_run) = capture_context(py, None)?;
    // One persistent callback per registration: every readiness event
    // schedules this same object, and removal cancels it so already
    // queued fires are skipped (mirrors asyncio's reader Handle).
    let ready = Arc::new(ReadyCallback::new(
        py,
        loop_ref.core.next_callback_id(),
        CallbackKind::Reader(raw_fd),
        callback,
        args.clone().unbind(),
        context,
        context_needs_run,
    ));
    let previous = loop_ref
        .core
        .state
        .lock()
        .expect("poisoned loop state")
        .reader_keepalive
        .insert(
            raw_fd,
            FdWatch {
                fileobj: fd_ops::fileobj_keepalive(fd),
                ready: Arc::clone(&ready),
            },
        );
    if let Some(previous) = previous {
        previous.ready.cancel();
    }
    loop_ref
        .core
        .send_command(LoopCommand::Io(LoopIoCommand::StartReader {
            fd: raw_fd,
            callback: ready,
        }))
        .map_err(PyLoop::map_loop_error)
}

#[cfg_attr(feature = "profile", hotpath::measure)]
pub(super) fn remove_reader(
    loop_ref: &PyLoop,
    py: Python<'_>,
    fd: &Bound<'_, PyAny>,
) -> PyResult<bool> {
    let raw_fd = fd_ops::fileobj_to_fd(py, fd)?;
    let removed_fd = {
        let mut state = loop_ref.core.state.lock().expect("poisoned loop state");
        let key = if state.reader_keepalive.contains_key(&raw_fd) {
            Some(raw_fd)
        } else {
            // The file object may already be closed (fileno() == -1);
            // find the registration by object identity so the watch is
            // still torn down.
            state
                .reader_keepalive
                .iter()
                .find(|(_, watch)| watch.fileobj.as_ptr() == fd.as_ptr())
                .map(|(watch_fd, _)| *watch_fd)
        };
        key.and_then(|key| {
            state
                .reader_keepalive
                .remove(&key)
                .map(|watch| (key, watch))
        })
    };

    let (stop_fd, removed) = match removed_fd {
        Some((key, watch)) => {
            watch.ready.cancel();
            (key, true)
        }
        None => (raw_fd, false),
    };
    loop_ref
        .core
        .send_command(LoopCommand::Io(LoopIoCommand::StopReader(stop_fd)))
        .map_err(PyLoop::map_loop_error)?;
    Ok(removed)
}

#[cfg_attr(feature = "profile", hotpath::measure)]
pub(super) fn add_writer(
    loop_ref: &PyLoop,
    py: Python<'_>,
    fd: &Bound<'_, PyAny>,
    callback: Py<PyAny>,
    args: &Bound<'_, PyTuple>,
) -> PyResult<()> {
    let raw_fd = fd_ops::fileobj_to_fd(py, fd)?;
    let (context, context_needs_run) = capture_context(py, None)?;
    let ready = Arc::new(ReadyCallback::new(
        py,
        loop_ref.core.next_callback_id(),
        CallbackKind::Writer(raw_fd),
        callback,
        args.clone().unbind(),
        context,
        context_needs_run,
    ));
    let previous = loop_ref
        .core
        .state
        .lock()
        .expect("poisoned loop state")
        .writer_keepalive
        .insert(
            raw_fd,
            FdWatch {
                fileobj: fd_ops::fileobj_keepalive(fd),
                ready: Arc::clone(&ready),
            },
        );
    if let Some(previous) = previous {
        previous.ready.cancel();
    }
    loop_ref
        .core
        .send_command(LoopCommand::Io(LoopIoCommand::StartWriter {
            fd: raw_fd,
            callback: ready,
        }))
        .map_err(PyLoop::map_loop_error)
}

#[cfg_attr(feature = "profile", hotpath::measure)]
pub(super) fn remove_writer(
    loop_ref: &PyLoop,
    py: Python<'_>,
    fd: &Bound<'_, PyAny>,
) -> PyResult<bool> {
    let raw_fd = fd_ops::fileobj_to_fd(py, fd)?;
    let removed_fd = {
        let mut state = loop_ref.core.state.lock().expect("poisoned loop state");
        let key = if state.writer_keepalive.contains_key(&raw_fd) {
            Some(raw_fd)
        } else {
            state
                .writer_keepalive
                .iter()
                .find(|(_, watch)| watch.fileobj.as_ptr() == fd.as_ptr())
                .map(|(watch_fd, _)| *watch_fd)
        };
        key.and_then(|key| {
            state
                .writer_keepalive
                .remove(&key)
                .map(|watch| (key, watch))
        })
    };

    let (stop_fd, removed) = match removed_fd {
        Some((key, watch)) => {
            watch.ready.cancel();
            (key, true)
        }
        None => (raw_fd, false),
    };
    loop_ref
        .core
        .send_command(LoopCommand::Io(LoopIoCommand::StopWriter(stop_fd)))
        .map_err(PyLoop::map_loop_error)?;
    Ok(removed)
}
