//! Zero-copy I/O utilities using `splice` and `sendfile`.
//!
//! This module provides async-aware zero-copy I/O operations:
//! - `splice()`: transfer data between file descriptors without copying to userspace.
//! - `splice_exact()`: transfer up to `len` bytes using `splice`, stopping at EOF.
//! - `sendfile_exact()`: transfer data from a file to a socket using a pipe as an intermediary.
//!
//! These operations are only available on Linux with the `splice` feature enabled.
//!
//! # Examples
//!
//! See "Splice through EOF" in `tools/vibeio-check/EXAMPLES.md` for an
//! executable pipe-to-socket transfer that needs only the `splice` feature.

use std::os::fd::{AsRawFd, OwnedFd};

use mio::Interest;

use crate::vibeio::{fd_inner::InnerRawHandle, io::AsInnerRawHandle, op::SpliceOp};

/// Transfer data from one file descriptor to another using `splice`.
///
/// This function uses the kernel's `splice` system call to transfer data
/// between file descriptors without copying to userspace.
/// A single call requests at most `i32::MAX` bytes to fit the completion result;
/// short transfers are permitted. Use `splice_exact` to keep transferring up to
/// the requested length or EOF.
///
/// With a readiness-based driver, an empty source is watched for readability;
/// otherwise a blocked transfer watches the destination for writability. The
/// source watch uses a temporary duplicated descriptor and is removed when the
/// operation finishes or is cancelled. Do not concurrently read from the source.
/// Sockets used with a readiness-based driver must be nonblocking; this function
/// does not change the source descriptor's status flags. Regular-file access may
/// still block on storage I/O.
///
/// Completion-based transfers retain owned duplicates of both descriptors until
/// the kernel finishes. Dropping the future does not roll back bytes already
/// transferred or guarantee that a queued transfer will not run.
pub async fn splice<'a, 'b>(
    from: &'a impl AsRawFd,
    to: &'b impl AsInnerRawHandle<'b>,
    len: usize,
) -> Result<usize, std::io::Error> {
    let to_handle = to.as_inner_raw_handle();

    let mut op = SpliceOp::new(from.as_raw_fd(), to_handle, len);
    let result = std::future::poll_fn(move |cx| to_handle.poll_op(cx, &mut op)).await;
    result
}

/// Transfer exactly `len` bytes from one file descriptor to another using `splice`.
///
/// This function calls `splice()` repeatedly until `len` bytes have been transferred
/// or EOF is reached. Interrupted calls are retried without resetting progress.
pub async fn splice_exact<'a, 'b>(
    from: &'a impl AsRawFd,
    to: &'b impl AsInnerRawHandle<'b>,
    len: u64,
) -> Result<u64, std::io::Error> {
    let mut total = 0;
    while total < len {
        let requested = (len - total).min(usize::MAX as u64) as usize;
        let n = retry_splice(|| splice(from, to, requested)).await?;
        if n == 0 {
            break;
        }
        total += n as u64;
    }

    Ok(total)
}

/// Transfer data from a file to a socket using `sendfile` semantics.
///
/// This function implements `sendfile`-like behavior using `splice` with an
/// intermediate pipe, allowing data to be transferred from a regular file to
/// a socket without copying to userspace.
/// Returns the transferred count if the source reaches EOF before `len` bytes.
/// Reports `WriteZero` if draining a nonempty staging pipe makes no progress.
/// Interrupted fills and drains are retried without discarding staged bytes.
pub async fn sendfile_exact<'a, 'b>(
    from: &'a impl AsRawFd,
    to: &'b impl AsInnerRawHandle<'b>,
    len: u64,
) -> Result<u64, std::io::Error> {
    // splice() requires at least one of the file descriptors to be a pipe.
    // Therefore, we need to create a pipe and use it as the destination.
    let (pipe_reader, pipe_writer) = std::io::pipe()?;

    // We only need to poll the pipe writer for writability.
    let pipe_writer_handle = WriteOwnedFd::new(pipe_writer.into())?;

    transfer_batches(
        len,
        |count| splice(from, &pipe_writer_handle, count),
        |count| splice(&pipe_reader, to, count),
    )
    .await
}

async fn retry_splice<F>(mut transfer: impl FnMut() -> F) -> std::io::Result<usize>
where
    F: std::future::Future<Output = std::io::Result<usize>>,
{
    loop {
        match transfer().await {
            Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
            result => return result,
        }
    }
}

async fn transfer_batches<F, D, Fill, Drain>(
    len: u64,
    mut fill: Fill,
    mut drain: Drain,
) -> std::io::Result<u64>
where
    Fill: FnMut(usize) -> F,
    Drain: FnMut(usize) -> D,
    F: std::future::Future<Output = std::io::Result<usize>>,
    D: std::future::Future<Output = std::io::Result<usize>>,
{
    let mut total = 0;
    while total < len {
        let requested = (len - total).min(usize::MAX as u64) as usize;
        let mut pending = retry_splice(|| fill(requested)).await?;
        if pending == 0 {
            break;
        }
        // Never refill until this batch is fully drained. A partial socket
        // transfer may leave the pipe full, and no other task drains this pipe.
        while pending > 0 {
            let written = retry_splice(|| drain(pending)).await?;
            if written == 0 {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::WriteZero,
                    "splice made no progress draining the staging pipe",
                ));
            }
            pending -= written;
            total += written as u64;
        }
    }
    Ok(total)
}

struct WriteOwnedFd {
    // Field order releases registration before closing its descriptor.
    handle: InnerRawHandle,
    _writer: OwnedFd,
}

impl WriteOwnedFd {
    fn new(writer: OwnedFd) -> std::io::Result<Self> {
        let handle = InnerRawHandle::new(writer.as_raw_fd(), Interest::WRITABLE)?;
        crate::vibeio::fd_inner::set_nonblocking(writer.as_raw_fd(), !handle.uses_completion())?;
        Ok(Self {
            handle,
            _writer: writer,
        })
    }
}

impl<'a> AsInnerRawHandle<'a> for WriteOwnedFd {
    #[inline]
    fn as_inner_raw_handle(&'a self) -> &'a InnerRawHandle {
        &self.handle
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vibeio::{driver::AnyDriver, executor::Runtime};
    use std::{cell::Cell, future::ready, io};

    #[test]
    fn staging_writer_closes_on_drop_and_failed_registration() {
        use std::io::Read;
        for fail_registration in [false, true] {
            let mut driver = AnyDriver::new_mock();
            let AnyDriver::Mock(mock) = &mut driver else {
                unreachable!()
            };
            mock.registrations = Some(Default::default());
            mock.registrations
                .as_ref()
                .unwrap()
                .results
                .borrow_mut()
                .push_back(if fail_registration {
                    Err(io::ErrorKind::PermissionDenied.into())
                } else {
                    Ok(mio::Token(9))
                });
            Runtime::new(driver).block_on(async move {
                let (mut reader, writer) = std::io::pipe().unwrap();
                crate::vibeio::fd_inner::set_nonblocking(reader.as_raw_fd(), true).unwrap();
                let result = WriteOwnedFd::new(writer.into());
                if fail_registration {
                    assert!(matches!(result, Err(error) if error.kind() == io::ErrorKind::PermissionDenied));
                } else {
                    drop(result.unwrap());
                }
                // EOF proves the owned writer was released on both paths;
                // nonblocking mode makes a leaked writer fail instead of hang.
                assert_eq!(reader.read(&mut [0]).unwrap(), 0);
                let driver = crate::vibeio::executor::current_driver().unwrap();
                let AnyDriver::Mock(mock) = driver.as_ref() else {
                    unreachable!()
                };
                let deregistered = mock.registrations.as_ref().unwrap().deregistered.borrow();
                assert_eq!(deregistered.as_slice(), if fail_registration { &[][..] } else { &[mio::Token(9)][..] });
            });
        }
    }

    #[test]
    fn invalid_raw_source_returns_an_os_error() {
        struct InvalidSource;
        impl AsRawFd for InvalidSource {
            fn as_raw_fd(&self) -> std::os::fd::RawFd {
                -1
            }
        }
        Runtime::new(AnyDriver::new_mio().unwrap()).block_on(async {
            let (_reader, writer) = std::io::pipe().unwrap();
            let destination = WriteOwnedFd::new(writer.into()).unwrap();
            let error = splice(&InvalidSource, &destination, 1).await.unwrap_err();
            assert_eq!(error.raw_os_error(), Some(libc::EBADF));
        });
    }

    #[test]
    fn sendfile_transfers_file_contents_and_reports_early_eof() {
        use std::io::{Read, Seek, Write};
        use std::os::fd::FromRawFd;
        // SAFETY: the name is NUL-terminated and the flag requests owned,
        // close-on-exec storage. No pointers are retained by memfd_create.
        let fd = unsafe { libc::memfd_create(c"vibeio-splice-test".as_ptr(), libc::MFD_CLOEXEC) };
        assert_ne!(fd, -1);
        // SAFETY: memfd_create returned a fresh descriptor owned by this test.
        let mut file = unsafe { std::fs::File::from_raw_fd(fd) };
        file.write_all(b"hello splice").unwrap();
        file.rewind().unwrap();
        let mut peer = Runtime::new(AnyDriver::new_mio().unwrap()).block_on(async move {
            let (socket, peer) = std::os::unix::net::UnixStream::pair().unwrap();
            let socket = crate::vibeio::net::UnixStream::from_std_poll(socket).unwrap();
            assert_eq!(sendfile_exact(&file, &socket, 100).await.unwrap(), 12);
            peer
        });
        let mut received = [0; 12];
        peer.set_read_timeout(Some(std::time::Duration::from_secs(2)))
            .unwrap();
        peer.read_exact(&mut received).unwrap();
        assert_eq!(&received, b"hello splice");
    }

    #[test]
    fn interrupted_fill_and_partial_drain_preserve_transfer_progress() {
        Runtime::new(AnyDriver::new_mock()).block_on(async {
            let mut fills = 0;
            let mut drains = 0;
            let result = transfer_batches(
                10,
                |requested| {
                    fills += 1;
                    ready(match fills {
                        1 => {
                            assert_eq!(requested, 10);
                            Err(io::ErrorKind::Interrupted.into())
                        }
                        2 => {
                            assert_eq!(requested, 10);
                            Ok(5)
                        }
                        3 => {
                            assert_eq!(requested, 5);
                            Ok(0)
                        }
                        _ => panic!("unexpected refill"),
                    })
                },
                |requested| {
                    drains += 1;
                    ready(match drains {
                        1 => {
                            assert_eq!(requested, 5);
                            Ok(2)
                        }
                        2 => {
                            assert_eq!(requested, 3);
                            Err(io::ErrorKind::Interrupted.into())
                        }
                        3 => {
                            assert_eq!(requested, 3);
                            Ok(3)
                        }
                        _ => panic!("unexpected drain"),
                    })
                },
            )
            .await
            .unwrap();
            assert_eq!(result, 5);
            assert_eq!((fills, drains), (3, 3));
        });
    }

    #[test]
    fn partial_drains_finish_before_refill_and_eof_returns_actual_count() {
        let remaining = Cell::new(11usize);
        let pending = Cell::new(0usize);
        Runtime::new(AnyDriver::new_mock()).block_on(async move {
            let result = transfer_batches(
                20,
                |requested| {
                    assert_eq!(pending.get(), 0, "refilling before draining can deadlock");
                    let count = requested.min(4).min(remaining.get());
                    remaining.set(remaining.get() - count);
                    pending.set(count);
                    ready(Ok(count))
                },
                |requested| {
                    assert_eq!(requested, pending.get());
                    pending.set(pending.get() - 1);
                    ready(Ok(1))
                },
            )
            .await
            .unwrap();
            assert_eq!(result, 11);
            assert_eq!(pending.get(), 0);
        });
    }

    #[test]
    fn drain_errors_and_zero_progress_terminate_without_refilling() {
        Runtime::new(AnyDriver::new_mock()).block_on(async {
            for zero_progress in [true, false] {
                let fills = Cell::new(0);
                let result = transfer_batches(
                    8,
                    |_| {
                        fills.set(fills.get() + 1);
                        ready(Ok(4))
                    },
                    |_| {
                        ready(if zero_progress {
                            Ok(0)
                        } else {
                            Err(io::Error::from(io::ErrorKind::BrokenPipe))
                        })
                    },
                )
                .await;
                assert_eq!(fills.get(), 1);
                assert_eq!(
                    result.unwrap_err().kind(),
                    if zero_progress {
                        io::ErrorKind::WriteZero
                    } else {
                        io::ErrorKind::BrokenPipe
                    }
                );
            }
        });
    }

    #[test]
    fn transfer_limit_does_not_read_past_requested_bytes() {
        Runtime::new(AnyDriver::new_mock()).block_on(async {
            let requested = Cell::new(0);
            for limit in [0, 3, 9] {
                requested.set(0);
                let result = transfer_batches(
                    limit,
                    |count| {
                        requested.set(requested.get() + count);
                        ready(Ok(count))
                    },
                    |count| ready(Ok(count)),
                )
                .await
                .unwrap();
                assert_eq!(result, limit);
                assert_eq!(requested.get() as u64, limit);
            }
        });
    }
}
