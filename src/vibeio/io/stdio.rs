//! Standard I/O utilities for stdin, stdout, and stderr.
//!
//! This module provides async-aware wrappers around standard I/O streams:
//! - `Stdin`: async stdin reader.
//! - `Stdout`: async stdout writer.
//! - `Stderr`: async stderr writer.
//!
//! These types use a blocking thread pool for I/O operations when inside
//! a runtime context. Outside a runtime, they fall back to synchronous I/O.
//!
//! # Examples
//!
//! See the compile-checked "Standard input echo" example in
//! `tools/vibeio-check/EXAMPLES.md`. Use [`super::copy`] to respect read counts,
//! handle partial writes, and flush stdout at EOF. Writing the entire returned
//! read buffer can emit stale bytes beyond the count from that read.

use std::io::{self, Read, Write};

use crate::vibeio::executor::current_driver;
use crate::vibeio::io::{AsyncRead, AsyncWrite, IoBuf, IoBufMut, iobuf_to_slice, read_into_buf};

/// Async-aware stdin reader.
#[derive(Debug, Default, Clone, Copy)]
pub struct Stdin {
    _private: (),
}

/// Async-aware stdout writer.
#[derive(Debug, Default, Clone, Copy)]
pub struct Stdout {
    _private: (),
}

/// Async-aware stderr writer.
#[derive(Debug, Default, Clone, Copy)]
pub struct Stderr {
    _private: (),
}

/// Get an async-aware stdin reader.
#[inline]
pub fn stdin() -> Stdin {
    Stdin { _private: () }
}

/// Get an async-aware stdout writer.
#[inline]
pub fn stdout() -> Stdout {
    Stdout { _private: () }
}

/// Get an async-aware stderr writer.
#[inline]
pub fn stderr() -> Stderr {
    Stderr { _private: () }
}

#[inline]
fn read_stdin_blocking(buf: &mut [u8]) -> io::Result<usize> {
    let mut stdin = std::io::stdin();
    stdin.read(buf)
}

#[inline]
fn write_stdout_blocking(buf: &[u8]) -> io::Result<usize> {
    let mut stdout = std::io::stdout();
    stdout.write(buf)
}

#[inline]
fn write_stderr_blocking(buf: &[u8]) -> io::Result<usize> {
    let mut stderr = std::io::stderr();
    stderr.write(buf)
}

#[inline]
fn flush_stdout_blocking() -> io::Result<()> {
    let mut stdout = std::io::stdout();
    stdout.flush()
}

#[inline]
fn flush_stderr_blocking() -> io::Result<()> {
    let mut stderr = std::io::stderr();
    stderr.flush()
}

#[inline]
fn blocking_pool_io_error() -> io::Error {
    io::Error::other("can't spawn blocking task for stdio I/O")
}

#[inline]
async fn read_in_blocking_pool<B: IoBufMut>(buf: B) -> (io::Result<usize>, B) {
    stdio_in_blocking_pool(buf, |buf| read_into_buf(buf, read_stdin_blocking)).await
}

async fn stdio_in_blocking_pool<B: Send + 'static>(
    buf: B,
    operation: impl FnOnce(&mut B) -> io::Result<usize> + Send + 'static,
) -> (io::Result<usize>, B) {
    let (result, buf) = crate::vibeio::blocking::with_buffer(buf, operation).await;
    (
        result.unwrap_or_else(|_| Err(blocking_pool_io_error())),
        buf,
    )
}

#[inline]
async fn write_stdout_in_blocking_pool<B: IoBuf>(buf: B) -> (io::Result<usize>, B) {
    stdio_in_blocking_pool(buf, |buf| write_stdout_blocking(iobuf_to_slice(buf))).await
}

#[inline]
async fn write_stderr_in_blocking_pool<B: IoBuf>(buf: B) -> (io::Result<usize>, B) {
    stdio_in_blocking_pool(buf, |buf| write_stderr_blocking(iobuf_to_slice(buf))).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::vibeio::{DriverKind, RuntimeBuilder, blocking::BlockingThreadPool};

    struct JoiningPool;

    struct RejectingPool;

    impl BlockingThreadPool for RejectingPool {
        fn spawn(&self, _task: Box<dyn FnOnce() + Send + 'static>) {}
    }

    impl BlockingThreadPool for JoiningPool {
        fn spawn(&self, task: Box<dyn FnOnce() + Send + 'static>) {
            // A deterministic test pool: worker panics close the result channel.
            // Joining here is test-only and does not model scheduling latency.
            let _ = std::thread::spawn(task).join();
        }
    }

    #[test]
    fn worker_panic_returns_the_owned_stdio_buffer() {
        let runtime = RuntimeBuilder::new()
            .driver(DriverKind::Mock)
            .blocking_pool(Box::new(JoiningPool))
            .build()
            .unwrap();
        runtime.block_on(async {
            let original = b"buffer".to_vec();
            let ptr = original.as_ptr();
            let (result, returned) = stdio_in_blocking_pool(original, |buf| {
                buf[0] = b'B';
                panic!("injected worker panic")
            })
            .await;
            assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Other);
            assert_eq!(returned, b"Buffer");
            assert_eq!(returned.as_ptr(), ptr);
        });
    }

    #[test]
    fn stdio_buffer_survives_missing_or_rejecting_pool() {
        for rejecting in [false, true] {
            let mut builder = RuntimeBuilder::new().driver(DriverKind::Mock);
            if rejecting {
                builder = builder.blocking_pool(Box::new(RejectingPool));
            }
            let runtime = builder.build().unwrap();
            runtime.block_on(async {
                let original = b"untouched".to_vec();
                let ptr = original.as_ptr();
                let (result, returned) = stdio_in_blocking_pool(original, |_| {
                    panic!("unavailable pool must not execute I/O")
                })
                .await;
                assert_eq!(result.unwrap_err().kind(), io::ErrorKind::Other);
                assert_eq!(returned, b"untouched");
                assert_eq!(returned.as_ptr(), ptr);
            });
        }
    }

    #[test]
    fn stdio_worker_preserves_normal_results_and_buffer_identity() {
        let runtime = RuntimeBuilder::new()
            .driver(DriverKind::Mock)
            .blocking_pool(Box::new(JoiningPool))
            .build()
            .unwrap();
        runtime.block_on(async {
            for fail in [false, true] {
                let original = b"data".to_vec();
                let ptr = original.as_ptr();
                let (result, returned) = stdio_in_blocking_pool(original, move |buf| {
                    buf[0] = b'D';
                    if fail {
                        Err(io::ErrorKind::PermissionDenied.into())
                    } else {
                        Ok(1)
                    }
                })
                .await;
                assert_eq!(
                    result.map_err(|error| error.kind()),
                    if fail {
                        Err(io::ErrorKind::PermissionDenied)
                    } else {
                        Ok(1)
                    }
                );
                assert_eq!(returned, b"Data");
                assert_eq!(returned.as_ptr(), ptr);
            }
        });
    }
}

#[inline]
async fn flush_stdout_in_blocking_pool() -> io::Result<()> {
    crate::vibeio::spawn_blocking(flush_stdout_blocking)
        .await
        .map_err(|_| blocking_pool_io_error())?
}

#[inline]
async fn flush_stderr_in_blocking_pool() -> io::Result<()> {
    crate::vibeio::spawn_blocking(flush_stderr_blocking)
        .await
        .map_err(|_| blocking_pool_io_error())?
}

impl AsyncRead for Stdin {
    #[inline]
    async fn read<B: IoBufMut>(&mut self, mut buf: B) -> (Result<usize, io::Error>, B) {
        if buf.buf_capacity() == 0 {
            return (Ok(0), buf);
        }

        if current_driver().is_some() {
            read_in_blocking_pool(buf).await
        } else {
            let result = read_into_buf(&mut buf, read_stdin_blocking);
            (result, buf)
        }
    }
}

impl AsyncWrite for Stdout {
    #[inline]
    async fn write<B: IoBuf>(&mut self, buf: B) -> (Result<usize, io::Error>, B) {
        if buf.buf_len() == 0 {
            return (Ok(0), buf);
        }

        if current_driver().is_some() {
            write_stdout_in_blocking_pool(buf).await
        } else {
            let slice = iobuf_to_slice(&buf);
            (write_stdout_blocking(slice), buf)
        }
    }

    #[inline]
    async fn flush(&mut self) -> Result<(), io::Error> {
        if current_driver().is_some() {
            flush_stdout_in_blocking_pool().await
        } else {
            flush_stdout_blocking()
        }
    }
}

impl AsyncWrite for Stderr {
    #[inline]
    async fn write<B: IoBuf>(&mut self, buf: B) -> (Result<usize, io::Error>, B) {
        if buf.buf_len() == 0 {
            return (Ok(0), buf);
        }

        if current_driver().is_some() {
            write_stderr_in_blocking_pool(buf).await
        } else {
            let slice = iobuf_to_slice(&buf);
            (write_stderr_blocking(slice), buf)
        }
    }

    #[inline]
    async fn flush(&mut self) -> Result<(), io::Error> {
        if current_driver().is_some() {
            flush_stderr_in_blocking_pool().await
        } else {
            flush_stderr_blocking()
        }
    }
}
