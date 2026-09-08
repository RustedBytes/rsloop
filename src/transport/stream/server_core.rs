//! Shared server state behind `PyServer`.
//!
//! A `ServerCore` owns its listeners, the accept workers, and the counters that
//! `wait_closed` waits on. TLS servers additionally admit only
//! `max_pending_tls_handshakes()` concurrent handshakes: `reserve_tls_handshake`
//! hands out a guard whose `Drop` releases the slot, so a handshake flood is
//! shed at accept time rather than exhausting worker threads.

use std::net::TcpStream as StdTcpStream;
#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::net::UnixStream as StdUnixStream;
#[cfg(unix)]
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::Ordering;

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
use pyo3::types::{PyDict, PyTuple};
use pyo3_async_runtimes::TaskLocals;

use super::platform::tcp_listener_raw_fd;
#[cfg(unix)]
use super::platform::unix_raw_fd;
#[cfg(unix)]
use super::remove_unix_socket_if_present;
#[cfg(unix)]
use super::run_unix_accept_loop;
use super::tuning::max_pending_tls_handshakes;
use super::worker::WorkerThread;
use super::{
    BlockingAcceptLoop, PendingTlsHandshake, ServerCore, ServerListener, run_server_accept_task,
    run_tcp_accept_loop, task_locals_for_loop,
};
use crate::context::{ensure_running_loop, run_in_context};
use crate::engine::{LoopCommand, LoopIoCommand};

fn reserve_tls_slot(current: usize, limit: usize, closed: bool) -> Option<usize> {
    (!closed && current < limit).then_some(current + 1)
}

#[cfg(kani)]
fn release_tls_slot(current: usize) -> Option<usize> {
    current.checked_sub(1)
}

fn close_server_flags(closed: &mut bool, serving: &mut bool) -> bool {
    if *closed {
        return false;
    }
    *closed = true;
    *serving = false;
    true
}

impl ServerCore {
    pub(super) fn close_python_sockets(&self) {
        let _ = Python::try_attach(|py| -> PyResult<()> {
            for socket in &self.sockets {
                let _ = socket.bind(py).call_method0("close");
            }
            Ok(())
        });
    }

    pub(crate) fn report_error(&self, err: PyErr, message: &str) {
        let _ = Python::try_attach(|py| -> PyResult<()> {
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

    pub(super) fn create_protocol_with_py(&self, py: Python<'_>) -> PyResult<Py<PyAny>> {
        ensure_running_loop(py, &self.loop_obj)?;
        let callback = self.protocol_factory.bind(py).clone().unbind();
        let args = PyTuple::empty(py).unbind();
        run_in_context(py, &self.context, self.context_needs_run, &callback, &args)
    }

    #[inline]
    pub(super) fn locals(&self, py: Python<'_>) -> PyResult<TaskLocals> {
        task_locals_for_loop(py, &self.loop_obj)
    }

    #[inline]
    pub(super) fn is_closed(&self) -> bool {
        self.state.lock().expect("poisoned server state").closed
    }

    pub(super) fn is_serving(&self) -> bool {
        let state = self.state.lock().expect("poisoned server state");
        state.serving && !state.closed
    }

    #[inline]
    pub(super) fn connection_opened(&self) {
        self.active_connections.fetch_add(1, Ordering::SeqCst);
    }

    #[inline]
    pub(super) fn connection_lost(&self) {
        self.active_connections.fetch_sub(1, Ordering::SeqCst);
        self.closed_notify.notify_all();
    }

    pub(super) fn reserve_tls_handshake(self: &Arc<Self>) -> Option<PendingTlsHandshake> {
        if self.is_closed() {
            return None;
        }
        let limit = max_pending_tls_handshakes();
        let reserved = self.pending_tls_handshakes.fetch_update(
            Ordering::AcqRel,
            Ordering::Acquire,
            |current| reserve_tls_slot(current, limit, false),
        );
        reserved.ok().map(|_| PendingTlsHandshake {
            server: Arc::clone(self),
        })
    }

    pub(super) fn close(&self) {
        {
            let mut state = self.state.lock().expect("poisoned server state");
            let super::ServerState {
                closed, serving, ..
            } = &mut *state;
            if !close_server_flags(closed, serving) {
                return;
            }
            state.listeners.clear();
        }

        // Blocking TLS accept workers are woken by connecting to their listening
        // address.  Keep the exposed Python socket alive until after that wake:
        // on Windows duplicated sockets share listener state, so closing the
        // Python handle first makes the wake connect pay the TCP failure timeout.
        for task in self
            .accept_tasks
            .lock()
            .expect("poisoned accept tasks")
            .drain(..)
        {
            task.abort();
        }
        for fd in self
            .accept_fds
            .lock()
            .expect("poisoned accept fds")
            .drain(..)
        {
            // The accept task owns its listener, so failing to cancel it leaks
            // that descriptor for as long as the loop lives. It may sit in
            // either of two registries -- the loop thread's `IO_TASKS` if it was
            // spawned from the loop thread, or the runtime thread's dispatcher
            // map if it went through `StartServerAccept` -- and those are
            // thread-locals of different threads. `spawn_accept_tasks` picks
            // between them by where it happens to run, and `create_server`
            // runs its body on an executor thread while `close()` is called
            // from Python on the loop thread, so the two decisions routinely
            // disagree. Try the local registry, then fall back to the command
            // path rather than assuming.
            if self.loop_core.on_runtime_thread() && self.loop_core.stop_io_task(fd) {
                continue;
            }
            let _ = self
                .loop_core
                .send_command(LoopCommand::Io(LoopIoCommand::StopServerAccept(fd)));
        }

        self.close_python_sockets();

        #[cfg(unix)]
        if let Some(path) = &self.cleanup_path
            && let Err(err) = remove_unix_socket_if_present(path)
        {
            self.report_error(
                PyRuntimeError::new_err(err.to_string()),
                "failed to remove Unix server socket",
            );
        }

        self.closed_notify.notify_all();
    }

    pub fn spawn_accept_tasks(self: &Arc<Self>) {
        let listeners = {
            let mut state = self.state.lock().expect("poisoned server state");
            if state.closed || state.serving {
                return;
            }
            state.serving = true;
            std::mem::take(&mut state.listeners)
        };

        if self.tls.is_some() {
            let mut tasks = self.accept_tasks.lock().expect("poisoned accept tasks");
            for listener in listeners {
                let server = Arc::clone(self);
                let task = match listener {
                    ServerListener::Tcp(listener) => {
                        let wake_addr = listener.local_addr().ok();
                        WorkerThread::spawn_interruptible(
                            "rsloop-tcp-accept",
                            move || {
                                if let Some(addr) = wake_addr {
                                    let _ = StdTcpStream::connect(addr);
                                }
                            },
                            move |stop| {
                                run_tcp_accept_loop(BlockingAcceptLoop::new(server, listener, stop))
                            },
                        )
                    }
                    #[cfg(unix)]
                    ServerListener::Unix(listener) => {
                        let wake_path = listener
                            .local_addr()
                            .ok()
                            .and_then(|addr| addr.as_pathname().map(PathBuf::from));
                        WorkerThread::spawn_interruptible(
                            "rsloop-unix-accept",
                            move || {
                                if let Some(path) = wake_path {
                                    let _ = StdUnixStream::connect(path);
                                }
                            },
                            move |stop| {
                                run_unix_accept_loop(BlockingAcceptLoop::new(
                                    server, listener, stop,
                                ))
                            },
                        )
                    }
                };
                match task {
                    Ok(task) => tasks.push(task),
                    Err(err) => self.report_error(
                        PyRuntimeError::new_err(err.to_string()),
                        "failed to spawn server accept worker",
                    ),
                }
            }
            return;
        }

        let mut accept_fds = self.accept_fds.lock().expect("poisoned accept fds");
        for listener in listeners {
            let fd = match &listener {
                ServerListener::Tcp(listener) => tcp_listener_raw_fd(listener),
                #[cfg(unix)]
                ServerListener::Unix(listener) => unix_raw_fd(listener.as_raw_fd()),
            };
            accept_fds.push(fd);
            let server = Arc::clone(self);
            // On the loop thread, host the accept loop directly on the loop's
            // own runtime so accepted connections are delivered without a
            // cross-thread hop. Off-thread callers fall back to the transitional
            // runtime-thread command path.
            if self.loop_core.on_runtime_thread() {
                self.loop_core
                    .spawn_io_tracked(fd, run_server_accept_task(server, listener));
            } else {
                let _ = self.loop_core.send_command(LoopCommand::Io(
                    LoopIoCommand::StartServerAccept {
                        fd,
                        server,
                        listener,
                    },
                ));
            }
        }
    }
}

#[cfg(kani)]
mod verification {
    use super::{close_server_flags, release_tls_slot, reserve_tls_slot};

    const MODEL_OPERATIONS: usize = 6;

    #[kani::proof]
    #[kani::unwind(8)]
    fn extended_tls_admission_and_server_close_preserve_bounds() {
        let limit = usize::from(kani::any::<u8>() % 5);
        let operations: [u8; MODEL_OPERATIONS] = kani::any();
        let mut pending = 0_usize;
        let mut closed = false;
        let mut serving: bool = kani::any();
        let mut listeners = usize::from(kani::any::<u8>() % 4);

        for operation in operations {
            match operation % 4 {
                0 => {
                    let next = reserve_tls_slot(pending, limit, closed);
                    if let Some(next) = next {
                        assert!(!closed);
                        assert!(pending < limit);
                        pending = next;
                    } else {
                        assert!(closed || pending >= limit);
                    }
                }
                1 => {
                    if let Some(next) = release_tls_slot(pending) {
                        assert!(next < pending);
                        pending = next;
                    } else {
                        assert_eq!(pending, 0);
                    }
                }
                2 => {
                    let changed = close_server_flags(&mut closed, &mut serving);
                    if changed {
                        listeners = 0;
                    } else {
                        assert!(closed);
                    }
                    assert!(closed);
                    assert!(!serving);
                    assert_eq!(listeners, 0);
                }
                _ => {
                    if !closed && !serving {
                        serving = true;
                        listeners = 0;
                    }
                }
            }
            assert!(pending <= limit);
            if closed {
                assert!(reserve_tls_slot(pending, limit, closed).is_none());
                assert!(!serving);
                assert_eq!(listeners, 0);
            }
        }
        kani::cover!(pending == limit);
        kani::cover!(closed && pending > 0);
    }
}

#[cfg(test)]
pub(super) mod tests {
    use std::sync::Mutex;
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

    use super::*;
    use crate::async_event::AsyncEvent;
    use crate::engine::LoopCore;
    use crate::transport::stream::ServerState;

    pub(in crate::transport::stream) fn build_test_server(
        py: Python<'_>,
    ) -> (Arc<ServerCore>, Arc<LoopCore>) {
        let loop_core = LoopCore::new();
        loop_core.mark_runtime_thread();
        let server = Arc::new(ServerCore {
            loop_core: Arc::clone(&loop_core),
            loop_obj: py.None(),
            protocol_factory: py.None(),
            context: py.None(),
            context_needs_run: false,
            sockets: Vec::new(),
            state: Mutex::new(ServerState {
                closed: false,
                serving: false,
                listeners: Vec::new(),
            }),
            accept_tasks: Mutex::new(Vec::new()),
            accept_fds: Mutex::new(Vec::new()),
            active_connections: AtomicUsize::new(0),
            pending_tls_handshakes: AtomicUsize::new(0),
            tls_overload_reported: AtomicBool::new(false),
            closed_notify: AsyncEvent::new(),
            cleanup_path: None,
            tls: None,
        });
        (server, loop_core)
    }

    pub(in crate::transport::stream) fn shutdown_test_server(
        server: Arc<ServerCore>,
        loop_core: Arc<LoopCore>,
    ) {
        server.close();
        loop_core.clear_runtime_thread();
        drop(server);
        loop_core.close().expect("close test loop");
    }

    #[test]
    fn server_serving_close_and_connection_notifications_are_idempotent() {
        crate::initialize_python_for_tests();
        Python::attach(|py| {
            let (server, loop_core) = build_test_server(py);
            assert!(!server.is_serving());
            assert!(!server.is_closed());

            server.spawn_accept_tasks();
            assert!(server.is_serving());
            server.spawn_accept_tasks();
            assert!(server.is_serving());

            server.connection_opened();
            assert_eq!(server.active_connections.load(Ordering::SeqCst), 1);
            let mut connection_notice = server.closed_notify.listen();
            server.connection_lost();
            assert_eq!(server.active_connections.load(Ordering::SeqCst), 0);
            assert_eq!(
                connection_notice.try_recv().expect("connection notice"),
                Some(())
            );

            let mut close_notice = server.closed_notify.listen();
            server.close();
            assert!(server.is_closed());
            assert!(!server.is_serving());
            assert_eq!(close_notice.try_recv().expect("close notice"), Some(()));
            server.close();

            shutdown_test_server(server, loop_core);
        });
    }

    #[test]
    fn tls_admission_enforces_the_limit_and_guard_drop_releases_slots() {
        crate::initialize_python_for_tests();
        Python::attach(|py| {
            let (server, loop_core) = build_test_server(py);
            let limit = max_pending_tls_handshakes();
            let guards = (0..limit)
                .map(|_| {
                    server
                        .reserve_tls_handshake()
                        .expect("slot below admission limit")
                })
                .collect::<Vec<_>>();
            assert_eq!(server.pending_tls_handshakes.load(Ordering::Acquire), limit);
            assert!(server.reserve_tls_handshake().is_none());

            server.tls_overload_reported.store(true, Ordering::Release);
            drop(guards);
            assert_eq!(server.pending_tls_handshakes.load(Ordering::Acquire), 0);
            assert!(!server.tls_overload_reported.load(Ordering::Acquire));

            let guard = server
                .reserve_tls_handshake()
                .expect("released slot can be reused");
            server.close();
            assert!(server.reserve_tls_handshake().is_none());
            drop(guard);

            shutdown_test_server(server, loop_core);
        });
    }
}
