//! Non-blocking `connect` and its completion handling.
//!
//! A non-blocking `connect()` to a reachable peer reports `EINPROGRESS`, so
//! every path here is "start the connect, then wait for writability and read
//! `SO_ERROR`". Two variants exist for a reason: the generic
//! [`connect_socket_to_address`] runs on the async runtime and works for any
//! address Python can parse, while [`fast_sock_connect`] stays on the loop
//! thread and skips both `socket.connect`'s `BlockingIOError` and the
//! cross-thread wakeup.

#[cfg(unix)]
use std::sync::Arc;

#[cfg(unix)]
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;

#[cfg(unix)]
use super::PyLoop;
use crate::fd_ops;

const WSAEISCONN: i32 = 10056;

#[cfg_attr(feature = "hotpath-profile", hotpath::measure(future = true))]
pub(super) async fn connect_socket_to_address(sock: Py<PyAny>, address: Py<PyAny>) -> PyResult<()> {
    // Initiate the connect and look up the descriptor in a single GIL
    // acquisition. Every `Python::attach` here runs on the async runtime's
    // worker thread and contends for the GIL held by the main loop thread, so
    // collapsing the fd lookup, the connect() call, and its error
    // classification into one attach removes several contended handoffs per
    // connection. On a non-blocking socket, connect() to a reachable peer
    // returns EINPROGRESS (retryable); anything else is an immediate success or
    // a hard error.
    let fd = match Python::attach(|py| -> PyResult<Option<fd_ops::RawFd>> {
        let fd = fd_ops::fileobj_to_fd(py, sock.bind(py))?;
        match sock.call_method1(py, "connect", (address.bind(py),)) {
            Ok(_) => Ok(None),
            Err(err) => {
                if fd_ops::is_retryable_socket_error(py, &err)? {
                    Ok(Some(fd))
                } else if is_already_connected_socket_error(py, &err)? {
                    Ok(None)
                } else {
                    Err(err)
                }
            }
        }
    })? {
        Some(fd) => fd,
        None => return Ok(()),
    };

    loop {
        fd_ops::wait_writable(fd).await?;
        let so_error = connect_so_error(fd, &sock)?;
        if so_error == 0 {
            return Ok(());
        }
        if is_connect_in_progress_errno(so_error) {
            continue;
        }
        if is_already_connected_errno(so_error) {
            return Ok(());
        }
        return Python::attach(|py| socket_os_error(py, so_error));
    }
}

/// Reads `SO_ERROR` for a connecting socket. On Unix this uses a direct
/// `getsockopt` so the hot connect-completion path never re-acquires the GIL
/// (or re-imports the `socket` module); Windows keeps the Python fallback.
#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
fn connect_so_error(fd: fd_ops::RawFd, sock: &Py<PyAny>) -> PyResult<i32> {
    #[cfg(unix)]
    {
        let _ = sock;
        let fd: libc::c_int = fd
            .try_into()
            .map_err(|_| PyRuntimeError::new_err("socket file descriptor out of range"))?;
        let mut value: libc::c_int = 0;
        let mut len: libc::socklen_t = std::mem::size_of::<libc::c_int>()
            .try_into()
            .expect("socklen_t can represent c_int size");
        let value_ptr = (&mut value as *mut libc::c_int).cast();
        let result = {
            // SAFETY: `fd` is a live socket and the out-parameters remain valid for the
            // call.
            unsafe { libc::getsockopt(fd, libc::SOL_SOCKET, libc::SO_ERROR, value_ptr, &mut len) }
        };
        if result == 0 {
            Ok(value)
        } else {
            Err(PyErr::from(std::io::Error::last_os_error()))
        }
    }

    #[cfg(windows)]
    {
        let _ = fd;
        Python::attach(|py| socket_so_error(py, sock))
    }
}

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
#[cfg(windows)]
fn socket_so_error(py: Python<'_>, sock: &Py<PyAny>) -> PyResult<i32> {
    let socket_mod = py.import("socket")?;
    sock.call_method1(
        py,
        "getsockopt",
        (
            socket_mod.getattr("SOL_SOCKET")?,
            socket_mod.getattr("SO_ERROR")?,
        ),
    )?
    .extract(py)
}

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
fn socket_os_error(py: Python<'_>, errno: i32) -> PyResult<()> {
    let builtins = py.import("builtins")?;
    let oserror = builtins.getattr("OSError")?;
    Err(PyErr::from_value(oserror.call1((
        errno,
        format!("socket connect failed: {errno}"),
    ))?))
}

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
fn is_already_connected_socket_error(py: Python<'_>, err: &PyErr) -> PyResult<bool> {
    let builtins = py.import("builtins")?;
    let oserror = builtins.getattr("OSError")?;
    if !err.is_instance(py, &oserror) {
        return Ok(false);
    }
    Ok(err
        .value(py)
        .getattr("errno")?
        .extract::<i32>()
        .ok()
        .is_some_and(is_already_connected_errno))
}

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
#[inline]
fn is_already_connected_errno(errno: i32) -> bool {
    errno == libc::EISCONN || errno == WSAEISCONN
}

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
#[inline]
fn is_connect_in_progress_errno(errno: i32) -> bool {
    errno == libc::EINPROGRESS || errno == libc::EALREADY || errno == libc::EWOULDBLOCK
}

/// Attempts to initiate the connect via a direct `libc::connect` for a numeric
/// address, skipping Python's `socket.connect` (its dispatch, address parsing,
/// and — the expensive part — raising a `BlockingIOError` for EINPROGRESS on
/// every non-blocking connect). Returns `Some(errno)` when the libc path ran
/// (0 = connected immediately), or `None` when the address is not a plain
/// numeric literal and the caller must fall back to `socket.connect`.
#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
#[cfg(unix)]
fn libc_connect_numeric(fd: fd_ops::RawFd, address: &Bound<'_, PyAny>) -> PyResult<Option<i32>> {
    let Ok(host_obj) = address.get_item(0) else {
        return Ok(None);
    };
    let Ok(host) = host_obj.extract::<std::borrow::Cow<'_, str>>() else {
        return Ok(None);
    };
    let Ok(ip) = host.parse::<std::net::IpAddr>() else {
        // Hostname, or scoped IPv6 ("fe80::1%eth0") std can't parse — fall back.
        return Ok(None);
    };
    let Ok(port) = address.get_item(1).and_then(|value| value.extract::<u16>()) else {
        return Ok(None);
    };
    let fd: libc::c_int = fd
        .try_into()
        .map_err(|_| PyRuntimeError::new_err("socket file descriptor out of range"))?;
    let sockaddr = socket2::SockAddr::from(std::net::SocketAddr::new(ip, port));
    // SAFETY: `sockaddr` owns a valid `sockaddr` of the reported length; `fd` is
    // the non-blocking socket the caller just built for this address family.
    let rc = unsafe { libc::connect(fd, sockaddr.as_ptr().cast(), sockaddr.len()) };
    if rc == 0 {
        Ok(Some(0))
    } else {
        Ok(Some(
            std::io::Error::last_os_error().raw_os_error().unwrap_or(0),
        ))
    }
}

/// Initiates a non-blocking connect on the loop thread and, when it does not
/// complete synchronously, hands the writability wait to the vibeio reactor on
/// this loop's own runtime. Returns the loop Future the caller awaits.
#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
#[cfg(unix)]
pub(super) fn fast_sock_connect<'py>(
    slf: &Py<PyLoop>,
    py: Python<'py>,
    sock: Py<PyAny>,
    address: Py<PyAny>,
) -> PyResult<Bound<'py, PyAny>> {
    let future = crate::python_names::call_method0(
        py,
        slf.bind(py).as_any(),
        crate::python_names::create_future(py),
    )?;
    let future_bound = future.bind(py).clone();
    let fd = fd_ops::fileobj_to_fd(py, sock.bind(py))?;

    // Prefer a direct libc connect (numeric address); otherwise fall back to
    // Python's socket.connect for hostnames / scoped IPv6.
    let errno = match libc_connect_numeric(fd, address.bind(py))? {
        Some(errno) => errno,
        None => match sock.call_method1(py, "connect", (address.bind(py),)) {
            Ok(_) => 0,
            Err(err) => err
                .value(py)
                .getattr(crate::python_names::errno(py))
                .ok()
                .and_then(|value| value.extract::<i32>().ok())
                .unwrap_or(0),
        },
    };

    if errno == 0 || is_already_connected_errno(errno) {
        future_bound.call_method1("set_result", (py.None(),))?;
        return Ok(future_bound);
    }
    if !is_connect_in_progress_errno(errno) {
        let message = std::io::Error::from_raw_os_error(errno).to_string();
        let oserror = pyo3::exceptions::PyOSError::new_err((errno, message)).into_value(py);
        future_bound.call_method1("set_exception", (oserror,))?;
        return Ok(future_bound);
    }

    // Connect is in progress: watch for writability on this loop's own reactor
    // (loop thread), so the completion is delivered without a cross-thread wake.
    let core = slf.borrow(py).core.clone();
    if !core.spawn_io(crate::transport::stream::run_connect_watch_task(
        Arc::clone(&core),
        fd,
        future.clone_ref(py),
    )) {
        return Err(PyRuntimeError::new_err(
            "event loop is not running; cannot start connect watch",
        ));
    }
    Ok(future_bound)
}

#[cfg(kani)]
mod verification {
    use super::{WSAEISCONN, is_already_connected_errno, is_connect_in_progress_errno};

    #[kani::proof]
    fn merge_socket_connect_errno_classes_are_exact_and_disjoint() {
        let errno: i32 = kani::any();
        let already = is_already_connected_errno(errno);
        let progress = is_connect_in_progress_errno(errno);

        assert_eq!(already, errno == libc::EISCONN || errno == WSAEISCONN);
        assert_eq!(
            progress,
            errno == libc::EINPROGRESS || errno == libc::EALREADY || errno == libc::EWOULDBLOCK
        );
        assert!(!(already && progress));
    }
}
#[cfg(test)]
mod tests {
    #[cfg(unix)]
    use pyo3::types::{PyString, PyTuple};

    use super::*;

    #[test]
    fn connect_errno_classification_is_exact() {
        assert!(is_connect_in_progress_errno(libc::EINPROGRESS));
        assert!(is_connect_in_progress_errno(libc::EALREADY));
        assert!(is_connect_in_progress_errno(libc::EWOULDBLOCK));
        assert!(!is_connect_in_progress_errno(libc::ECONNREFUSED));

        assert!(is_already_connected_errno(libc::EISCONN));
        assert!(is_already_connected_errno(WSAEISCONN));
        assert!(!is_already_connected_errno(libc::EINPROGRESS));
    }

    #[test]
    fn python_socket_errors_preserve_errno_and_connected_classification() {
        crate::initialize_python_for_tests();
        Python::attach(|py| {
            for errno in [libc::EISCONN, WSAEISCONN] {
                let err = pyo3::exceptions::PyOSError::new_err((errno, "connected"));
                assert!(
                    is_already_connected_socket_error(py, &err).expect("classify connected error")
                );
            }
            assert!(
                !is_already_connected_socket_error(
                    py,
                    &pyo3::exceptions::PyValueError::new_err("not an OS error")
                )
                .expect("classify unrelated error")
            );

            let err = socket_os_error(py, libc::ECONNREFUSED)
                .expect_err("nonzero SO_ERROR should become OSError");
            assert!(err.is_instance_of::<pyo3::exceptions::PyOSError>(py));
            assert_eq!(
                err.value(py)
                    .getattr("errno")
                    .expect("errno attribute")
                    .extract::<i32>()
                    .expect("integer errno"),
                libc::ECONNREFUSED
            );
        });
    }

    #[cfg(unix)]
    #[test]
    fn numeric_connect_parser_falls_back_for_names_and_rejects_out_of_range_fds() {
        crate::initialize_python_for_tests();
        Python::attach(|py| {
            let hostname = PyTuple::new(
                py,
                [
                    PyString::new(py, "localhost").into_any(),
                    80_u16.into_pyobject(py).expect("port").into_any(),
                ],
            )
            .expect("hostname address");
            assert_eq!(
                libc_connect_numeric(i64::MAX, hostname.as_any()).expect("hostname fallback"),
                None
            );

            let numeric = PyTuple::new(
                py,
                [
                    PyString::new(py, "127.0.0.1").into_any(),
                    80_u16.into_pyobject(py).expect("port").into_any(),
                ],
            )
            .expect("numeric address");
            let err = libc_connect_numeric(i64::MAX, numeric.as_any())
                .expect_err("out-of-range descriptor should fail before libc connect");
            assert!(err.is_instance_of::<PyRuntimeError>(py));
        });
    }

    #[cfg(unix)]
    #[test]
    fn direct_so_error_read_works_for_a_connected_socket() {
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

        let mut fds = [-1; 2];
        // SAFETY: `fds` has room for both descriptors returned by `socketpair`.
        assert_eq!(
            unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) },
            0
        );
        // SAFETY: successful `socketpair` returned two newly owned descriptors.
        let (socket, _peer) =
            unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        crate::initialize_python_for_tests();
        Python::attach(|py| {
            assert_eq!(
                connect_so_error(i64::from(socket.as_raw_fd()), &py.None()).expect("read SO_ERROR"),
                0
            );
        });
    }
}
