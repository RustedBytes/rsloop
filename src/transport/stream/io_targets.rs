//! Concrete I/O endpoints a transport can be attached to.
//!
//! A transport is created from a TCP socket, a Unix socket, or a pipe/file, and
//! its reader and writer halves are handed separate owned values. These enums
//! keep the per-target `match` in one place so the reader, writer, and TLS
//! paths can stay generic over the endpoint kind.
//!
//! `LazyWriterTarget` exists because most connections never need a dedicated
//! writer thread: the duplicate descriptor is only materialised once a write
//! actually has to be queued.

#[cfg(unix)]
use std::os::fd::AsRawFd;
#[cfg(unix)]
use std::os::unix::net::UnixStream as StdUnixStream;
use std::{
    io::{self, Read},
    net::{Shutdown, TcpStream as StdTcpStream},
    sync::Arc,
    thread,
    time::Duration,
};

use pyo3::prelude::*;

#[cfg(unix)]
use super::duplicate_unix_direct_writer;
#[cfg(unix)]
use super::platform::unix_raw_fd;
use super::{
    duplicate_configured_tcp_stream,
    platform::{file_raw_fd, tcp_stream_raw_fd},
    write_queue::WriterReceiver,
};
use crate::fd_ops;

pub(super) enum TaskedDirectWriter {
    Tcp(Arc<StdTcpStream>),
    #[cfg(unix)]
    Unix(StdUnixStream),
}

impl TaskedDirectWriter {
    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "TaskedDirectWriter")
    )]
    pub(super) fn fd(&self) -> fd_ops::RawFd {
        match self {
            Self::Tcp(stream) => tcp_stream_raw_fd(stream),
            #[cfg(unix)]
            Self::Unix(stream) => unix_raw_fd(stream.as_raw_fd()),
        }
    }

    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "TaskedDirectWriter")
    )]
    pub(super) fn shutdown_close(&self) -> io::Result<()> {
        match self {
            Self::Tcp(stream) => shutdown_tcp_stream(stream, Shutdown::Both),
            #[cfg(unix)]
            Self::Unix(stream) => shutdown_unix_stream(stream, Shutdown::Both),
        }
    }

    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "TaskedDirectWriter")
    )]
    pub(super) fn shutdown_write(&self) -> io::Result<()> {
        match self {
            Self::Tcp(stream) => shutdown_tcp_stream(stream, Shutdown::Write),
            #[cfg(unix)]
            Self::Unix(stream) => shutdown_unix_stream(stream, Shutdown::Write),
        }
    }
}

pub enum ReaderTarget {
    File(std::fs::File),
    Tcp(Arc<StdTcpStream>),
    #[cfg(unix)]
    Unix(StdUnixStream),
}

impl ReaderTarget {
    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "ReaderTarget")
    )]
    pub(super) fn fd(&self) -> fd_ops::RawFd {
        match self {
            Self::File(file) => file_raw_fd(file),
            Self::Tcp(stream) => tcp_stream_raw_fd(stream),
            #[cfg(unix)]
            Self::Unix(stream) => unix_raw_fd(stream.as_raw_fd()),
        }
    }

    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "ReaderTarget")
    )]
    #[cfg(windows)]
    pub(super) fn pollable(&self) -> bool {
        !matches!(self, Self::File(_))
    }

    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "ReaderTarget")
    )]
    #[cfg(not(windows))]
    pub(super) fn pollable(&self) -> bool {
        true
    }
}

impl Read for ReaderTarget {
    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "<ReaderTarget as Read>")
    )]
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::File(file) => file.read(buf),
            Self::Tcp(stream) => stream.as_ref().read(buf),
            #[cfg(unix)]
            Self::Unix(stream) => stream.read(buf),
        }
    }
}

pub(super) enum WriterTarget {
    File(std::fs::File),
    Tcp(StdTcpStream),
    #[cfg(unix)]
    Unix(StdUnixStream),
    Sink(io::Sink),
}

pub(super) struct LazyWriterConfig {
    pub(super) target: LazyWriterTarget,
    pub(super) writer_rx: WriterReceiver,
}

pub(super) enum LazyWriterTarget {
    Tcp(fd_ops::RawFd),
    #[cfg(unix)]
    Unix(fd_ops::RawFd),
}

impl LazyWriterTarget {
    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "LazyWriterTarget")
    )]
    pub(super) fn materialize(self) -> PyResult<WriterTarget> {
        match self {
            Self::Tcp(fd) => duplicate_configured_tcp_stream(fd).map(WriterTarget::Tcp),
            #[cfg(unix)]
            Self::Unix(fd) => duplicate_unix_direct_writer(fd).map(WriterTarget::Unix),
        }
    }
}

impl WriterTarget {
    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "WriterTarget")
    )]
    pub(super) fn fd(&self) -> Option<fd_ops::RawFd> {
        match self {
            Self::File(file) => Some(file_raw_fd(file)),
            Self::Tcp(stream) => Some(tcp_stream_raw_fd(stream)),
            #[cfg(unix)]
            Self::Unix(stream) => Some(unix_raw_fd(stream.as_raw_fd())),
            Self::Sink(_) => None,
        }
    }

    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "WriterTarget")
    )]
    #[cfg(windows)]
    pub(super) fn pollable(&self) -> bool {
        !matches!(self, Self::File(_))
    }

    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "WriterTarget")
    )]
    #[cfg(not(windows))]
    pub(super) fn pollable(&self) -> bool {
        true
    }

    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "WriterTarget")
    )]
    pub(super) fn shutdown_write(&self) -> io::Result<()> {
        match self {
            Self::Tcp(stream) => shutdown_tcp_stream(stream, Shutdown::Write),
            #[cfg(unix)]
            Self::Unix(stream) => shutdown_unix_stream(stream, Shutdown::Write),
            Self::File(_) | Self::Sink(_) => Ok(()),
        }
    }

    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "WriterTarget")
    )]
    pub(super) fn shutdown_close(&self) -> io::Result<()> {
        match self {
            Self::Tcp(stream) => shutdown_tcp_stream(stream, Shutdown::Both),
            #[cfg(unix)]
            Self::Unix(stream) => shutdown_unix_stream(stream, Shutdown::Both),
            Self::File(_) | Self::Sink(_) => Ok(()),
        }
    }
}

pub(super) enum StreamKind {
    Tcp(StdTcpStream),
    #[cfg(unix)]
    Unix(StdUnixStream),
}

impl StreamKind {
    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "StreamKind")
    )]
    pub(super) fn fd(&self) -> fd_ops::RawFd {
        match self {
            Self::Tcp(stream) => tcp_stream_raw_fd(stream),
            #[cfg(unix)]
            Self::Unix(stream) => unix_raw_fd(stream.as_raw_fd()),
        }
    }

    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "StreamKind")
    )]
    #[cfg(windows)]
    pub(super) fn pollable(&self) -> bool {
        true
    }

    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "StreamKind")
    )]
    #[cfg(not(windows))]
    pub(super) fn pollable(&self) -> bool {
        true
    }

    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "StreamKind")
    )]
    pub(super) fn shutdown_close(&self) -> io::Result<()> {
        match self {
            Self::Tcp(stream) => shutdown_tcp_stream(stream, Shutdown::Both),
            #[cfg(unix)]
            Self::Unix(stream) => shutdown_unix_stream(stream, Shutdown::Both),
        }
    }
}

impl Read for StreamKind {
    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "<StreamKind as Read>")
    )]
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Tcp(stream) => stream.read(buf),
            #[cfg(unix)]
            Self::Unix(stream) => stream.read(buf),
        }
    }
}

impl io::Write for StreamKind {
    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "<StreamKind as io :: Write>")
    )]
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Tcp(stream) => stream.write(buf),
            #[cfg(unix)]
            Self::Unix(stream) => stream.write(buf),
        }
    }

    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "<StreamKind as io :: Write>")
    )]
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Tcp(stream) => stream.flush(),
            #[cfg(unix)]
            Self::Unix(stream) => stream.flush(),
        }
    }
}

impl io::Write for WriterTarget {
    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "<WriterTarget as io :: Write>")
    )]
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::File(file) => file.write(buf),
            Self::Tcp(stream) => stream.write(buf),
            #[cfg(unix)]
            Self::Unix(stream) => stream.write(buf),
            Self::Sink(sink) => sink.write(buf),
        }
    }

    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "<WriterTarget as io :: Write>")
    )]
    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::File(file) => file.flush(),
            Self::Tcp(stream) => stream.flush(),
            #[cfg(unix)]
            Self::Unix(stream) => stream.flush(),
            Self::Sink(sink) => sink.flush(),
        }
    }
}

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
pub(super) fn shutdown_tcp_stream(stream: &StdTcpStream, how: Shutdown) -> io::Result<()> {
    for attempt in 0..=100 {
        match stream.shutdown(how) {
            Ok(()) => return Ok(()),
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::NotConnected | io::ErrorKind::BrokenPipe
                ) =>
            {
                return Ok(());
            }
            Err(err) if is_no_buffer_space(&err) && attempt < 100 => {
                thread::sleep(Duration::from_millis(1));
            }
            Err(err) => return Err(err),
        }
    }
    unreachable!("bounded shutdown retry loop always returns")
}

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
#[cfg(unix)]
pub(super) fn shutdown_unix_stream(stream: &StdUnixStream, how: Shutdown) -> io::Result<()> {
    for attempt in 0..=100 {
        match stream.shutdown(how) {
            Ok(()) => return Ok(()),
            Err(err)
                if matches!(
                    err.kind(),
                    io::ErrorKind::NotConnected | io::ErrorKind::BrokenPipe
                ) =>
            {
                return Ok(());
            }
            Err(err) if is_no_buffer_space(&err) && attempt < 100 => {
                thread::sleep(Duration::from_millis(1));
            }
            Err(err) => return Err(err),
        }
    }
    unreachable!("bounded shutdown retry loop always returns")
}

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
#[inline]
pub(super) fn is_no_buffer_space_code(raw_os_error: Option<i32>) -> bool {
    #[cfg(unix)]
    {
        raw_os_error == Some(libc::ENOBUFS)
    }
    #[cfg(windows)]
    {
        raw_os_error == Some(10_055) // WSAENOBUFS
    }
}

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
#[inline]
fn is_no_buffer_space(err: &io::Error) -> bool {
    is_no_buffer_space_code(err.raw_os_error())
}

#[cfg(kani)]
mod verification {
    use super::is_no_buffer_space_code;

    #[kani::proof]
    fn merge_no_buffer_space_code_is_exact() {
        let raw_os_error: Option<i32> = kani::any();

        #[cfg(unix)]
        assert_eq!(
            is_no_buffer_space_code(raw_os_error),
            raw_os_error == Some(libc::ENOBUFS)
        );
        #[cfg(windows)]
        assert_eq!(
            is_no_buffer_space_code(raw_os_error),
            raw_os_error == Some(10_055)
        );
    }
}
#[cfg(test)]
mod shutdown_tests {
    #[cfg(unix)]
    #[test]
    fn no_buffer_space_is_retryable_during_shutdown() {
        assert!(super::is_no_buffer_space(
            &std::io::Error::from_raw_os_error(libc::ENOBUFS)
        ));
    }
}
