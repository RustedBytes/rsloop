//! Socket readiness waits for the blocking worker threads.
//!
//! Workers block on `poll()` with a bounded interval so a stop request is
//! noticed promptly. Descriptors that cannot be polled (Windows files) fall
//! back to a short sleep, which keeps the same call shape for every target.

use std::{io, thread, time::Duration};

use super::tuning::BLOCKING_POLL_INTERVAL_MS;
use crate::fd_ops;

#[cfg_attr(feature = "profile", hotpath::measure)]
fn clamp_poll_timeout_ms(remaining_ms: u128) -> i32 {
    let timeout = remaining_ms.clamp(1, u128::from(i32::MAX.unsigned_abs()));
    i32::try_from(timeout).expect("poll timeout is clamped to i32::MAX")
}

#[cfg_attr(feature = "profile", hotpath::measure)]
pub(super) fn wait_socket_ready(
    fd: fd_ops::RawFd,
    pollable: bool,
    read: bool,
    write: bool,
) -> io::Result<()> {
    if pollable {
        loop {
            match fd_ops::poll_fd(fd, read, write, BLOCKING_POLL_INTERVAL_MS) {
                Ok((read_ready, write_ready))
                    if (!read || read_ready) && (!write || write_ready) =>
                {
                    return Ok(());
                }
                Ok(_) => continue,
                Err(err) => return Err(err),
            }
        }
    }

    thread::sleep(Duration::from_millis(10));
    Ok(())
}

#[cfg_attr(feature = "profile", hotpath::measure)]
pub(super) fn wait_socket_ready_once(
    fd: fd_ops::RawFd,
    pollable: bool,
    read: bool,
    write: bool,
) -> io::Result<bool> {
    if pollable {
        return fd_ops::poll_fd(fd, read, write, BLOCKING_POLL_INTERVAL_MS)
            .map(|(read_ready, write_ready)| (!read || read_ready) && (!write || write_ready));
    }
    thread::sleep(Duration::from_millis(10));
    Ok(true)
}

#[cfg_attr(feature = "profile", hotpath::measure)]
pub(super) fn poll_read_ready(fd: fd_ops::RawFd) -> io::Result<bool> {
    fd_ops::poll_fd(fd, true, false, BLOCKING_POLL_INTERVAL_MS).map(|(ready, _)| ready)
}

#[cfg_attr(feature = "profile", hotpath::measure)]
pub(super) fn wait_socket_ready_until(
    fd: fd_ops::RawFd,
    pollable: bool,
    read: bool,
    write: bool,
    deadline: std::time::Instant,
) -> io::Result<()> {
    if pollable {
        loop {
            let now = std::time::Instant::now();
            if now >= deadline {
                return Err(io::Error::new(
                    io::ErrorKind::TimedOut,
                    "SSL shutdown timed out",
                ));
            }

            let remaining_ms = deadline.saturating_duration_since(now).as_millis();
            let remaining_ms = clamp_poll_timeout_ms(remaining_ms);
            match fd_ops::poll_fd(fd, read, write, remaining_ms) {
                Ok((read_ready, write_ready))
                    if (!read || read_ready) && (!write || write_ready) =>
                {
                    return Ok(());
                }
                Ok(_) => continue,
                Err(err) => return Err(err),
            }
        }
    }

    if std::time::Instant::now() >= deadline {
        return Err(io::Error::new(
            io::ErrorKind::TimedOut,
            "SSL shutdown timed out",
        ));
    }
    thread::sleep(Duration::from_millis(10));
    Ok(())
}

#[cfg(kani)]
mod verification {
    use super::clamp_poll_timeout_ms;

    #[kani::proof]
    fn merge_poll_timeout_is_positive_and_fits_c_int() {
        let remaining_ms: u128 = kani::any();
        let timeout = clamp_poll_timeout_ms(remaining_ms);

        assert!(timeout >= 1);
        assert!(timeout <= i32::MAX);
        if remaining_ms == 0 {
            assert_eq!(timeout, 1);
        } else if remaining_ms > i32::MAX as u128 {
            assert_eq!(timeout, i32::MAX);
        } else {
            assert_eq!(timeout as u128, remaining_ms);
        }
    }
}
