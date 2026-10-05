//! Raw descriptor conversions used by stream transports.

#[cfg(unix)]
use std::os::fd::{AsRawFd, FromRawFd};
#[cfg(windows)]
use std::os::windows::io::{
    AsRawHandle, AsRawSocket, FromRawHandle, FromRawSocket, RawHandle, RawSocket,
};
use std::{
    fs::File,
    net::{TcpListener, TcpStream},
};

use pyo3::{exceptions::PyRuntimeError, prelude::*};
use socket2::Socket;

use crate::fd_ops;

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
#[cfg(unix)]
pub(super) fn file_raw_fd(file: &File) -> fd_ops::RawFd {
    fd_ops::RawFd::from(file.as_raw_fd())
}

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
#[cfg(windows)]
pub(super) fn file_raw_fd(file: &File) -> fd_ops::RawFd {
    file.as_raw_handle() as isize as fd_ops::RawFd
}

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
#[cfg(unix)]
#[inline]
pub(super) fn tcp_stream_raw_fd(stream: &TcpStream) -> fd_ops::RawFd {
    fd_ops::RawFd::from(stream.as_raw_fd())
}

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
#[cfg(windows)]
#[inline]
pub(super) fn tcp_stream_raw_fd(stream: &TcpStream) -> fd_ops::RawFd {
    stream.as_raw_socket().cast_signed()
}

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
#[cfg(unix)]
pub(super) fn tcp_listener_raw_fd(listener: &TcpListener) -> fd_ops::RawFd {
    fd_ops::RawFd::from(listener.as_raw_fd())
}

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
#[cfg(windows)]
pub(super) fn tcp_listener_raw_fd(listener: &TcpListener) -> fd_ops::RawFd {
    listener.as_raw_socket().cast_signed()
}

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
#[cfg(unix)]
#[inline]
pub(super) fn unix_raw_fd(fd: std::os::fd::RawFd) -> fd_ops::RawFd {
    fd_ops::RawFd::from(fd)
}

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
#[cfg(unix)]
fn raw_fd_for_std(fd: fd_ops::RawFd) -> PyResult<std::os::fd::RawFd> {
    fd.try_into()
        .map_err(|_| PyRuntimeError::new_err("fd out of range"))
}

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
#[cfg(unix)]
pub(super) fn from_owned_raw_fd<T: FromRawFd>(fd: fd_ops::RawFd) -> PyResult<T> {
    let fd = raw_fd_for_std(fd)?;
    // SAFETY: the caller transfers one owned descriptor to the returned IO object.
    Ok(unsafe { T::from_raw_fd(fd) })
}

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
#[cfg(windows)]
pub(super) fn from_owned_raw_socket<T: FromRawSocket>(socket: RawSocket) -> T {
    // SAFETY: the caller transfers one owned socket to the returned IO object.
    unsafe { T::from_raw_socket(socket) }
}

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
#[cfg(windows)]
pub(super) fn from_owned_raw_handle<T: FromRawHandle>(handle: RawHandle) -> T {
    // SAFETY: the caller transfers one owned handle to the returned IO object.
    unsafe { T::from_raw_handle(handle) }
}

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
#[cfg(unix)]
pub(super) fn socket_from_owned_raw(fd: fd_ops::RawFd) -> PyResult<Socket> {
    from_owned_raw_fd(fd)
}

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
#[cfg(windows)]
pub(super) fn socket_from_owned_raw(fd: fd_ops::RawFd) -> PyResult<Socket> {
    let fd: RawSocket = fd
        .try_into()
        .map_err(|_| PyRuntimeError::new_err("socket handle out of range"))?;
    Ok(from_owned_raw_socket(fd))
}
