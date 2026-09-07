//! `loop.sock_*` operations on raw Python sockets.
//!
//! Each one drives the Python socket method and, when it reports a retryable
//! error, waits for readiness before trying again — the socket object stays the
//! source of truth so subclassed or wrapped sockets keep working.

use pyo3::prelude::*;

use super::PyLoop;
use super::socket_connect::connect_socket_to_address;

pub(super) fn sock_recv<'py>(
    slf: Py<PyLoop>,
    py: Python<'py>,
    sock: Py<PyAny>,
    nbytes: usize,
) -> PyResult<Bound<'py, PyAny>> {
    super::socket_operation::start(
        slf,
        py,
        sock,
        super::socket_operation::SocketAction::Recv(nbytes),
    )
}

pub(super) fn sock_recv_into<'py>(
    slf: Py<PyLoop>,
    py: Python<'py>,
    sock: Py<PyAny>,
    buf: Py<PyAny>,
) -> PyResult<Bound<'py, PyAny>> {
    super::socket_operation::start(
        slf,
        py,
        sock,
        super::socket_operation::SocketAction::RecvInto(buf),
    )
}

pub(super) fn sock_sendall<'py>(
    slf: Py<PyLoop>,
    py: Python<'py>,
    sock: Py<PyAny>,
    data: Py<PyAny>,
) -> PyResult<Bound<'py, PyAny>> {
    let len = {
        let export = pyo3::buffer::PyUntypedBuffer::get(data.bind(py))?;
        if !export.is_c_contiguous() {
            return Err(pyo3::exceptions::PyBufferError::new_err(
                "sendall requires a contiguous buffer",
            ));
        }
        export.len_bytes()
    };
    super::socket_operation::start(
        slf,
        py,
        sock,
        super::socket_operation::SocketAction::SendAll {
            data,
            offset: 0,
            len,
        },
    )
}

pub(super) fn sock_accept<'py>(
    slf: Py<PyLoop>,
    py: Python<'py>,
    sock: Py<PyAny>,
) -> PyResult<Bound<'py, PyAny>> {
    super::socket_operation::start(slf, py, sock, super::socket_operation::SocketAction::Accept)
}

pub(super) fn sock_connect<'py>(
    slf: Py<PyLoop>,
    py: Python<'py>,
    sock: Py<PyAny>,
    address: Py<PyAny>,
) -> PyResult<Bound<'py, PyAny>> {
    let locals = PyLoop::task_locals(py, &slf)?;
    pyo3_async_runtimes::async_std::future_into_py_with_locals(py, locals, async move {
        connect_socket_to_address(sock, address).await?;
        Ok(Python::attach(|py| py.None()))
    })
}

/// Connects an INET/INET6 stream socket, returning a loop-native Future
/// (not a coroutine — awaited directly, never `create_task`ed). On Unix the
/// writability wait runs on the vibeio reactor and its completion is
/// delivered through the loop's batched, GIL-free ready queue, so many
/// concurrent connections drain in one loop iteration instead of paying a
/// per-connection async-runtime handoff. Non-Unix and non-INET sockets fall
/// back to the general `sock_connect` path.
pub(super) fn sock_connect_fast<'py>(
    slf: Py<PyLoop>,
    py: Python<'py>,
    sock: Py<PyAny>,
    address: Py<PyAny>,
) -> PyResult<Bound<'py, PyAny>> {
    // Only `__loop_create_connection` calls this, and only with an
    // INET/INET6 SOCK_STREAM socket it just built, so the family/type check
    // (a `socket` import plus IntEnum property reads on every connection) is
    // pure overhead — skip straight to the loop-thread connect on Unix.
    #[cfg(unix)]
    {
        super::socket_connect::fast_sock_connect(&slf, py, sock, address)
    }
    #[cfg(not(unix))]
    {
        sock_connect(slf, py, sock, address)
    }
}
