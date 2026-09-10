//! The subprocess event queue and the protocol-call plumbing above it.
//!
//! Reader and waiter threads never touch Python. They enqueue
//! `PendingProcessEvent`s and ask the loop for a drain;
//! `drain_pending_events_with_py` is the only place the protocol is called, so
//! `pipe_data_received`, `process_exited`, and `connection_lost` always arrive
//! on the loop thread in the order the workers produced them.
//!
//! The drain re-checks ordering constraints as it goes: `connection_lost` is
//! held back until the process has exited and every pipe has closed, which is
//! the guarantee `asyncio.SubprocessProtocol` expects.

use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyTuple};

use super::{PendingProcessEvent, ProcessTransportCore};
use crate::context::{ensure_running_loop, run_in_context};
use crate::engine::{LoopCommand, LoopTransportCommand};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProcessEventKind {
    PipeData,
    PipeLost,
    Exited,
    Lost,
}

fn process_event_kind(event: &PendingProcessEvent) -> ProcessEventKind {
    match event {
        PendingProcessEvent::PipeDataReceived { .. } => ProcessEventKind::PipeData,
        PendingProcessEvent::PipeConnectionLost { .. } => ProcessEventKind::PipeLost,
        PendingProcessEvent::ProcessExited { .. } => ProcessEventKind::Exited,
        PendingProcessEvent::ConnectionLost { .. } => ProcessEventKind::Lost,
    }
}

fn process_event_stops_drain(kind: ProcessEventKind) -> bool {
    kind == ProcessEventKind::Lost
}
impl ProcessTransportCore {
    pub(super) fn enqueue_pending_event(self: &Arc<Self>, event: PendingProcessEvent) {
        self.pending_events
            .lock()
            .expect("poisoned process pending queue")
            .push_back(event);

        if !self.events_scheduled.swap(true, Ordering::AcqRel)
            && self
                .loop_core
                .send_command(LoopCommand::Transport(LoopTransportCommand::Process(
                    Arc::clone(self),
                )))
                .is_err()
        {
            self.events_scheduled.store(false, Ordering::Release);
        }
    }

    pub(crate) fn drain_pending_events_with_py(self: &Arc<Self>, py: Python<'_>) -> PyResult<()> {
        let mut drained = VecDeque::new();
        loop {
            {
                let mut queue = self
                    .pending_events
                    .lock()
                    .expect("poisoned process pending queue");
                if queue.is_empty() {
                    self.events_scheduled.store(false, Ordering::Release);
                    return Ok(());
                }

                std::mem::swap(&mut drained, &mut *queue);
            }

            while let Some(event) = drained.pop_front() {
                let stops_drain = process_event_stops_drain(process_event_kind(&event));
                match event {
                    PendingProcessEvent::PipeDataReceived { fd, data } => {
                        if let Err(err) = self.pipe_data_received_with_py(py, fd, &data) {
                            self.report_error(err, "subprocess pipe_data_received failed");
                            let _ = self.connection_lost_with_py(py, None);
                            self.events_scheduled.store(false, Ordering::Release);
                            return Ok(());
                        }
                    }
                    PendingProcessEvent::PipeConnectionLost { fd, exc } => {
                        if let Err(err) = self.pipe_connection_lost_value_with_py(
                            py,
                            fd,
                            exc.map(PyRuntimeError::new_err),
                        ) {
                            self.report_error(err, "subprocess pipe_connection_lost failed");
                            let _ = self.connection_lost_with_py(py, None);
                            self.events_scheduled.store(false, Ordering::Release);
                            return Ok(());
                        }
                    }
                    PendingProcessEvent::ProcessExited { returncode } => {
                        if let Err(err) = self.process_exited_with_py(py, returncode) {
                            self.report_error(err, "subprocess process_exited failed");
                            let _ = self.connection_lost_with_py(py, None);
                            self.events_scheduled.store(false, Ordering::Release);
                            return Ok(());
                        }
                    }
                    PendingProcessEvent::ConnectionLost { exc } => {
                        let _ = self.connection_lost_with_py(py, exc.map(PyRuntimeError::new_err));
                    }
                }
                if stops_drain {
                    self.events_scheduled.store(false, Ordering::Release);
                    return Ok(());
                }
            }
        }
    }

    pub(super) fn call_protocol_with_tuple(
        &self,
        py: Python<'_>,
        method: &str,
        args: &Bound<'_, PyTuple>,
    ) -> PyResult<Py<PyAny>> {
        let (protocol, context, context_needs_run) = {
            let state = self.state.lock().expect("poisoned process state");
            (
                state.protocol.clone_ref(py),
                state.context.clone_ref(py),
                state.context_needs_run,
            )
        };
        let callback = protocol.bind(py).getattr(method)?.unbind();
        let tuple = args.clone().unbind();
        run_in_context(py, &context, context_needs_run, &callback, &tuple)
    }

    pub(super) fn call_in_loop_context<T>(
        &self,
        f: impl for<'py> FnOnce(Python<'py>) -> PyResult<T>,
    ) -> PyResult<T> {
        Python::attach(|py| {
            ensure_running_loop(py, &self.loop_obj)?;
            f(py)
        })
    }

    pub(super) fn call_protocol_method0(
        &self,
        py: Python<'_>,
        method: &str,
    ) -> PyResult<Py<PyAny>> {
        let args = PyTuple::empty(py);
        self.call_protocol_with_tuple(py, method, &args)
    }

    pub(super) fn call_protocol_method1(
        &self,
        py: Python<'_>,
        method: &str,
        arg: Py<PyAny>,
    ) -> PyResult<Py<PyAny>> {
        let args = PyTuple::new(py, [arg])?;
        self.call_protocol_with_tuple(py, method, &args)
    }

    pub(super) fn call_protocol_method2(
        &self,
        py: Python<'_>,
        method: &str,
        arg0: Py<PyAny>,
        arg1: Py<PyAny>,
    ) -> PyResult<Py<PyAny>> {
        let args = PyTuple::new(py, [arg0, arg1])?;
        self.call_protocol_with_tuple(py, method, &args)
    }

    pub(super) fn report_error(&self, err: PyErr, message: &str) {
        let _ = Python::attach(|py| -> PyResult<()> {
            let context = PyDict::new(py);
            context.set_item("message", message)?;
            context.set_item("exception", err.value(py))?;
            self.loop_core.call_exception_handler(
                py,
                Some(&self.loop_obj),
                context.unbind().into_any(),
            )
        });
    }
}

#[cfg(kani)]
mod verification {
    use super::{ProcessEventKind, process_event_stops_drain};

    fn event_kind(tag: u8) -> ProcessEventKind {
        match tag % 4 {
            0 => ProcessEventKind::PipeData,
            1 => ProcessEventKind::PipeLost,
            2 => ProcessEventKind::Exited,
            _ => ProcessEventKind::Lost,
        }
    }

    #[kani::proof]
    #[kani::unwind(7)]
    fn extended_process_event_drain_delivers_exact_terminal_prefix() {
        const EVENT_COUNT: usize = 6;
        let mut events = [ProcessEventKind::PipeData; EVENT_COUNT];
        for event in &mut events {
            *event = event_kind(kani::any());
        }

        let mut delivered = [ProcessEventKind::PipeData; EVENT_COUNT];
        let mut delivered_len = 0_usize;
        let mut first_terminal = None;
        for (index, event) in events.iter().copied().enumerate() {
            delivered[delivered_len] = event;
            delivered_len += 1;
            if process_event_stops_drain(event) {
                first_terminal = Some(index);
                break;
            }
        }

        assert_eq!(
            delivered_len,
            first_terminal.map_or(EVENT_COUNT, |index| index + 1)
        );
        for index in 0..delivered_len {
            assert_eq!(delivered[index], events[index]);
        }
        if first_terminal.is_some() {
            assert_eq!(delivered[delivered_len - 1], ProcessEventKind::Lost);
        } else {
            assert!(
                !delivered[..delivered_len]
                    .iter()
                    .copied()
                    .any(process_event_stops_drain)
            );
        }

        kani::cover!(first_terminal == Some(0));
        kani::cover!(first_terminal == Some(EVENT_COUNT - 1));
        kani::cover!(first_terminal.is_none());
    }
}
