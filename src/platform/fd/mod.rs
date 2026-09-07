//! Cross-platform raw descriptor and readiness operations.

use std::io;

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;
#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use self::windows::{duplicate_handle, duplicate_handle_from_fd, poll_fd};

pub type RawFd = i64;

#[cfg(windows)]
const FD_POLL_INTERVAL_MS: i32 = 50;

pub fn fileobj_to_fd(_py: Python<'_>, fileobj: &Bound<'_, PyAny>) -> PyResult<RawFd> {
    if let Ok(fd) = fileobj.extract::<RawFd>() {
        return Ok(fd);
    }

    fileobj.call_method0("fileno")?.extract::<RawFd>()
}

pub fn fileobj_keepalive(fileobj: &Bound<'_, PyAny>) -> Py<PyAny> {
    fileobj.clone().unbind().into_any()
}

pub fn dup_raw_fd(fd: RawFd) -> io::Result<RawFd> {
    #[cfg(unix)]
    {
        let fd = raw_fd_to_c_int(fd)?;
        // SAFETY: `fd` was range-checked as a C file descriptor. `dup` returns a new descriptor
        // or `-1` with errno set and does not retain Rust references.
        let duped = unsafe { libc::dup(fd) };
        if duped < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(RawFd::from(duped))
    }

    #[cfg(windows)]
    {
        windows::dup_raw_fd(fd)
    }
}

#[cfg(unix)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum PollReadiness {
    Invalid,
    Ready { read: bool, write: bool },
}

#[cfg(unix)]
fn decode_poll_revents(read: bool, write: bool, revents: i32) -> PollReadiness {
    if revents & i32::from(libc::POLLNVAL) != 0 {
        return PollReadiness::Invalid;
    }

    let error_bits = i32::from(libc::POLLERR | libc::POLLHUP);
    PollReadiness::Ready {
        read: read && (revents & (i32::from(libc::POLLIN) | error_bits)) != 0,
        write: write && (revents & (i32::from(libc::POLLOUT) | error_bits)) != 0,
    }
}

#[cfg(unix)]
pub fn poll_fd(fd: RawFd, read: bool, write: bool, timeout_ms: i32) -> io::Result<(bool, bool)> {
    if !read && !write {
        return Ok((false, false));
    }

    let fd = raw_fd_to_c_int(fd)?;
    let mut events = 0;
    if read {
        events |= libc::POLLIN;
    }
    if write {
        events |= libc::POLLOUT;
    }

    let mut pollfd = libc::pollfd {
        fd,
        events,
        revents: 0,
    };

    loop {
        // SAFETY: `pollfd` points to one initialized `libc::pollfd` and the count is `1`; `poll`
        // only mutates the `revents` field and reports errors through errno.
        let ready = unsafe { libc::poll(&mut pollfd, 1, timeout_ms) };
        if ready >= 0 {
            break;
        }

        let err = io::Error::last_os_error();
        if err.kind() == io::ErrorKind::Interrupted {
            continue;
        }
        return Err(err);
    }

    match decode_poll_revents(read, write, i32::from(pollfd.revents)) {
        PollReadiness::Invalid => Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "file descriptor is not open",
        )),
        PollReadiness::Ready { read, write } => Ok((read, write)),
    }
}

pub async fn wait_writable(fd: RawFd) -> PyResult<()> {
    wait_for_interest(fd, false, true).await
}

async fn wait_for_interest(fd: RawFd, read: bool, write: bool) -> PyResult<()> {
    #[cfg(windows)]
    {
        crate::blocking::run(format!("rsloop-fd-wait-{fd}"), move || {
            loop {
                match poll_fd(fd, read, write, FD_POLL_INTERVAL_MS)? {
                    (read_ready, write_ready)
                        if (!read || read_ready) && (!write || write_ready) =>
                    {
                        return Ok::<(), io::Error>(());
                    }
                    _ => {}
                }
            }
        })
        .await
        .map_err(PyRuntimeError::new_err)?
        .map_err(|err| PyRuntimeError::new_err(err.to_string()))
    }

    #[cfg(not(windows))]
    {
        use std::os::fd::BorrowedFd;

        // Register the descriptor with async-io's shared, process-wide reactor
        // thread instead of spawning a fresh OS thread per wait. The previous
        // thread-per-wait approach dominated connection-setup latency: a burst
        // of N concurrent connects spawned N `poll()` threads. async-io drives
        // the same epoll/kqueue reactor smol/async-std already run, so this adds
        // no extra threads and deregisters as soon as the wait resolves.
        let raw = raw_fd_to_c_int(fd).map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
        // SAFETY: the caller keeps `fd` open for the duration of this await
        // (the owning Python socket outlives the connect/recv/send operation).
        let borrowed = unsafe { BorrowedFd::borrow_raw(raw) };
        let async_fd = async_io::Async::new_nonblocking(borrowed)
            .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;

        let result = if write {
            futures::future::poll_fn(|cx| async_fd.poll_writable(cx)).await
        } else if read {
            futures::future::poll_fn(|cx| async_fd.poll_readable(cx)).await
        } else {
            Ok(())
        };
        result.map_err(|err| PyRuntimeError::new_err(err.to_string()))
    }
}

pub fn is_retryable_socket_error(py: Python<'_>, err: &PyErr) -> PyResult<bool> {
    let builtins = py.import("builtins")?;
    let blocking = builtins.getattr("BlockingIOError")?;
    let interrupted = builtins.getattr("InterruptedError")?;
    Ok(err.is_instance(py, &blocking) || err.is_instance(py, &interrupted))
}

fn raw_fd_fits_c_int(fd: RawFd) -> bool {
    fd >= RawFd::from(libc::c_int::MIN) && fd <= RawFd::from(libc::c_int::MAX)
}

fn raw_fd_to_c_int(fd: RawFd) -> io::Result<libc::c_int> {
    if raw_fd_fits_c_int(fd) {
        Ok(libc::c_int::try_from(fd)
            .expect("descriptor accepted by raw_fd_fits_c_int must fit in c_int"))
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "descriptor out of range",
        ))
    }
}

/// A `connect()` attempt that is still completing in the background.
#[cfg(unix)]
#[inline]
pub fn is_connect_in_progress_errno(errno: i32) -> bool {
    errno == libc::EINPROGRESS || errno == libc::EALREADY || errno == libc::EWOULDBLOCK
}

/// The socket is already connected (a benign outcome for `connect()`).
#[cfg(unix)]
#[inline]
pub fn is_already_connected_errno(errno: i32) -> bool {
    errno == libc::EISCONN
}

/// Reads the pending `SO_ERROR` for a socket via a direct `getsockopt`, so the
/// connect-completion path resolves without acquiring the GIL.
#[cfg(unix)]
#[inline]
pub fn socket_so_error(fd: RawFd) -> io::Result<i32> {
    let fd = raw_fd_to_c_int(fd)?;
    let mut value: libc::c_int = 0;
    let mut len: libc::socklen_t = std::mem::size_of::<libc::c_int>()
        .try_into()
        .expect("socklen_t can represent c_int size");
    let value_ptr = (&mut value as *mut libc::c_int).cast();
    let result = {
        // SAFETY: `fd` is a socket and the correctly sized out-parameters live for the call.
        unsafe { libc::getsockopt(fd, libc::SOL_SOCKET, libc::SO_ERROR, value_ptr, &mut len) }
    };
    if result == 0 {
        Ok(value)
    } else {
        Err(io::Error::last_os_error())
    }
}

#[cfg(kani)]
mod verification {
    use super::*;

    #[kani::proof]
    fn merge_raw_descriptor_range_matches_c_int() {
        let fd: RawFd = kani::any();
        let expected = fd >= RawFd::from(libc::c_int::MIN) && fd <= RawFd::from(libc::c_int::MAX);
        assert_eq!(raw_fd_fits_c_int(fd), expected);
        if expected {
            assert_eq!(RawFd::from(fd as libc::c_int), fd);
        }
    }

    #[cfg(unix)]
    #[kani::proof]
    fn merge_connect_errno_classification_is_exact() {
        let errno: i32 = kani::any();
        assert_eq!(
            is_connect_in_progress_errno(errno),
            errno == libc::EINPROGRESS || errno == libc::EALREADY || errno == libc::EWOULDBLOCK
        );
        assert_eq!(is_already_connected_errno(errno), errno == libc::EISCONN);
        assert!(!(is_connect_in_progress_errno(errno) && is_already_connected_errno(errno)));
    }

    #[cfg(unix)]
    #[kani::proof]
    fn merge_poll_readiness_decoding_respects_interests_and_error_bits() {
        let read: bool = kani::any();
        let write: bool = kani::any();
        let revents: i32 = kani::any();
        let decoded = decode_poll_revents(read, write, revents);

        if revents & i32::from(libc::POLLNVAL) != 0 {
            assert_eq!(decoded, PollReadiness::Invalid);
            return;
        }

        let PollReadiness::Ready {
            read: read_ready,
            write: write_ready,
        } = decoded
        else {
            unreachable!("valid poll bits were classified as invalid");
        };
        let error_bits = i32::from(libc::POLLERR | libc::POLLHUP);
        assert_eq!(
            read_ready,
            read && revents & (i32::from(libc::POLLIN) | error_bits) != 0
        );
        assert_eq!(
            write_ready,
            write && revents & (i32::from(libc::POLLOUT) | error_bits) != 0
        );
        assert!(!read_ready || read);
        assert!(!write_ready || write);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_objects_accept_raw_integers_and_fileno_methods() {
        crate::initialize_python_for_tests();
        Python::attach(|py| {
            assert_eq!(
                fileobj_to_fd(py, 42_i64.into_pyobject(py).expect("integer").as_any())
                    .expect("integer fd"),
                42
            );
            let socket = py
                .import("socket")
                .expect("import socket")
                .call_method0("socket")
                .expect("create socket");
            let fd = fileobj_to_fd(py, &socket).expect("socket fileno");
            assert!(fd >= 0);
            let keepalive = fileobj_keepalive(&socket);
            assert!(keepalive.bind(py).is(&socket));
            socket.call_method0("close").expect("close socket");
        });
    }

    #[test]
    fn retryable_python_socket_errors_are_classified_narrowly() {
        crate::initialize_python_for_tests();
        Python::attach(|py| {
            assert!(
                is_retryable_socket_error(
                    py,
                    &pyo3::exceptions::PyBlockingIOError::new_err("would block")
                )
                .expect("classify BlockingIOError")
            );
            assert!(
                is_retryable_socket_error(
                    py,
                    &pyo3::exceptions::PyInterruptedError::new_err("interrupted")
                )
                .expect("classify InterruptedError")
            );
            assert!(
                !is_retryable_socket_error(py, &pyo3::exceptions::PyValueError::new_err("other"))
                    .expect("classify ValueError")
            );
        });
    }

    #[cfg(unix)]
    #[test]
    fn errno_helpers_cover_progress_connected_and_unrelated_errors() {
        assert!(is_connect_in_progress_errno(libc::EINPROGRESS));
        assert!(is_connect_in_progress_errno(libc::EALREADY));
        assert!(is_connect_in_progress_errno(libc::EWOULDBLOCK));
        assert!(!is_connect_in_progress_errno(libc::ECONNREFUSED));
        assert!(is_already_connected_errno(libc::EISCONN));
        assert!(!is_already_connected_errno(libc::EINPROGRESS));
    }

    #[cfg(unix)]
    #[test]
    fn poll_reports_pipe_readiness_hangup_and_invalid_descriptors() {
        use std::io::Write;
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

        let mut fds = [-1; 2];
        // SAFETY: `fds` has room for both descriptors returned by `pipe`.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        // SAFETY: successful `pipe` returned two owned descriptors.
        let (read_end, write_end) =
            unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };
        assert_eq!(
            poll_fd(i64::from(read_end.as_raw_fd()), true, false, 0).expect("poll empty pipe"),
            (false, false)
        );
        assert_eq!(
            poll_fd(i64::from(write_end.as_raw_fd()), false, true, 0).expect("poll writable pipe"),
            (false, true)
        );

        let mut writer = std::fs::File::from(write_end);
        writer.write_all(b"x").expect("write pipe byte");
        assert_eq!(
            poll_fd(i64::from(read_end.as_raw_fd()), true, false, 0).expect("poll readable pipe"),
            (true, false)
        );
        drop(writer);
        assert_eq!(
            poll_fd(i64::from(read_end.as_raw_fd()), true, false, 0).expect("poll pipe hangup"),
            (true, false)
        );

        let err = poll_fd(i64::from(i32::MAX), true, false, 0)
            .expect_err("unopened descriptor should fail");
        assert_eq!(err.kind(), io::ErrorKind::InvalidInput);
        assert_eq!(
            poll_fd(-1, true, false, 0).expect("negative fd is ignored"),
            (false, false)
        );
        assert_eq!(
            poll_fd(-1, false, false, 0).expect("no interests"),
            (false, false)
        );
        assert!(raw_fd_to_c_int(i64::MAX).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn connected_socket_has_no_pending_so_error() {
        use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

        let mut fds = [-1; 2];
        // SAFETY: `fds` has room for the connected pair returned by `socketpair`.
        assert_eq!(
            unsafe { libc::socketpair(libc::AF_UNIX, libc::SOCK_STREAM, 0, fds.as_mut_ptr()) },
            0
        );
        // SAFETY: successful `socketpair` returned two owned descriptors.
        let (client, _server) =
            unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) };

        assert_eq!(
            socket_so_error(i64::from(client.as_raw_fd())).expect("read SO_ERROR"),
            0
        );
    }
}
