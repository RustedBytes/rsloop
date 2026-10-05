//! Address resolution and socket construction for the loop's transport methods.
//!
//! Everything here runs Python's `socket` module rather than binding addresses
//! in Rust, so that `family`/`proto`/`flags` handling and the resulting
//! `OSError`s match what `asyncio` callers already expect.

#[cfg(unix)]
use pyo3::exceptions::PyRuntimeError;
use pyo3::{
    prelude::*,
    types::{PyDict, PyModule, PyTuple},
};

#[cfg(unix)]
use crate::fd_ops;
use crate::transport::stream::{
    ServerListener, tcp_listener_from_owned_socket_fd, tcp_server_listener,
};
#[cfg(unix)]
use crate::transport::stream::{
    remove_unix_socket_if_present, unix_listener_from_owned_socket_fd, unix_server_listener,
};

/// One `socket.getaddrinfo` row reduced to what socket creation needs:
/// `(family, type, proto, sockaddr)`.
pub(super) type ResolvedStreamAddrinfo = (i32, i32, i32, Py<PyAny>);

pub(super) struct TcpServerSocketOptions {
    pub(super) family: i32,
    pub(super) flags: i32,
    pub(super) backlog: i32,
    pub(super) reuse_address: Option<bool>,
    pub(super) reuse_port: Option<bool>,
    pub(super) keep_alive: Option<bool>,
}

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
pub(super) fn resolve_stream_addrinfos(
    py: Python<'_>,
    host: Option<Py<PyAny>>,
    port: Option<Py<PyAny>>,
    family: i32,
    proto: i32,
    flags: i32,
) -> PyResult<Vec<ResolvedStreamAddrinfo>> {
    let socket_mod = py.import("socket")?;
    let addrinfos = call_getaddrinfo(
        py,
        &socket_mod,
        AddrInfoQuery {
            host,
            port,
            family,
            proto,
            flags,
        },
    )?;

    let mut resolved = Vec::new();
    for entry in addrinfos.try_iter()? {
        resolved.push(parse_stream_addrinfo(entry?)?);
    }
    Ok(resolved)
}

struct AddrInfoQuery {
    host: Option<Py<PyAny>>,
    port: Option<Py<PyAny>>,
    family: i32,
    proto: i32,
    flags: i32,
}

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
fn call_getaddrinfo<'py>(
    py: Python<'py>,
    socket_mod: &Bound<'py, PyModule>,
    query: AddrInfoQuery,
) -> PyResult<Bound<'py, PyAny>> {
    let kwargs = PyDict::new(py);
    kwargs.set_item("family", query.family)?;
    kwargs.set_item("type", socket_mod.getattr("SOCK_STREAM")?)?;
    kwargs.set_item("proto", query.proto)?;
    kwargs.set_item("flags", query.flags)?;

    let host = query.host.unwrap_or_else(|| py.None());
    let port = query.port.unwrap_or_else(|| py.None());
    socket_mod
        .getattr("getaddrinfo")?
        .call((host, port), Some(&kwargs))
}

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
fn parse_stream_addrinfo(entry: Bound<'_, PyAny>) -> PyResult<ResolvedStreamAddrinfo> {
    let tuple = entry.cast::<PyTuple>()?;
    Ok((
        tuple.get_item(0)?.extract::<i32>()?,
        tuple.get_item(1)?.extract::<i32>()?,
        tuple.get_item(2)?.extract::<i32>()?,
        tuple.get_item(4)?.unbind(),
    ))
}

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
pub(super) fn build_stream_socket(
    py: Python<'_>,
    family: i32,
    sock_type: i32,
    proto: i32,
) -> PyResult<Py<PyAny>> {
    let socket_mod = py.import("socket")?;
    let sock = socket_mod
        .getattr("socket")?
        .call1((family, sock_type, proto))?;
    sock.call_method1("setblocking", (false,))?;
    Ok(sock.unbind())
}

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
#[cfg(unix)]
fn set_socket_bool_option_unix(
    py: Python<'_>,
    sock: &Py<PyAny>,
    level: libc::c_int,
    option: libc::c_int,
    enabled: bool,
) -> PyResult<()> {
    let fd = fd_ops::fileobj_to_fd(py, sock.bind(py))?;
    let fd: libc::c_int = fd
        .try_into()
        .map_err(|_| PyRuntimeError::new_err("socket file descriptor out of range"))?;
    let value: libc::c_int = enabled.into();
    let value_len: libc::socklen_t = std::mem::size_of_val(&value)
        .try_into()
        .expect("socklen_t can represent c_int size");
    let value_ptr = (&value as *const libc::c_int).cast();
    // SAFETY: `fd` is range-checked as a socket descriptor, and `value` points to a
    // live `c_int` with the correct length for boolean socket options.
    let result = unsafe { libc::setsockopt(fd, level, option, value_ptr, value_len) };
    if result == 0 {
        Ok(())
    } else {
        Err(PyErr::from(std::io::Error::last_os_error()))
    }
}

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
pub(super) fn listener_sources_from_sockets(
    py: Python<'_>,
    sockets: &[Py<PyAny>],
) -> PyResult<Vec<ServerListener>> {
    let mut listeners = Vec::with_capacity(sockets.len());
    for socket in sockets {
        #[cfg(windows)]
        let fd = socket.call_method0(py, "fileno")?.extract(py)?;
        #[cfg(not(windows))]
        let fd = socket
            .call_method0(py, "dup")?
            .call_method0(py, "detach")?
            .extract(py)?;
        #[cfg(unix)]
        {
            let family = socket.getattr(py, "family")?.extract::<i32>(py)?;
            listeners.push(if family == libc::AF_UNIX {
                unix_server_listener(unix_listener_from_owned_socket_fd(fd)?)
            } else {
                tcp_server_listener(tcp_listener_from_owned_socket_fd(fd)?)
            });
        }
        #[cfg(not(unix))]
        {
            listeners.push(tcp_server_listener(tcp_listener_from_owned_socket_fd(fd)?));
        }
    }
    Ok(listeners)
}

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
pub(super) fn build_tcp_server_sockets(
    py: Python<'_>,
    host: Option<Py<PyAny>>,
    port: Option<Py<PyAny>>,
    options: TcpServerSocketOptions,
) -> PyResult<Vec<Py<PyAny>>> {
    let TcpServerSocketOptions {
        family,
        flags,
        backlog,
        reuse_address,
        reuse_port,
        keep_alive,
    } = options;
    let socket_mod = py.import("socket")?;
    let sol_socket = socket_mod.getattr("SOL_SOCKET")?;
    let so_reuseaddr = socket_mod.getattr("SO_REUSEADDR")?;
    let so_reuseport = socket_mod.getattr("SO_REUSEPORT").ok();
    #[cfg(not(unix))]
    let so_keepalive = socket_mod.getattr("SO_KEEPALIVE")?;
    let addrinfos = resolve_stream_addrinfos(py, host, port, family, 0, flags)?;
    let mut sockets = Vec::with_capacity(addrinfos.len());

    for (addr_family, sock_type, proto, sockaddr) in addrinfos {
        let sock = build_stream_socket(py, addr_family, sock_type, proto)?;
        apply_tcp_server_socket_options(
            py,
            &sock,
            TcpSocketOptionRefs {
                sol_socket: &sol_socket,
                so_reuseaddr: &so_reuseaddr,
                so_reuseport: so_reuseport.as_ref(),
                #[cfg(not(unix))]
                so_keepalive: &so_keepalive,
            },
            reuse_address,
            reuse_port,
            keep_alive,
        )?;
        sock.call_method1(py, "bind", (sockaddr,))?;
        sock.call_method1(py, "listen", (backlog,))?;
        sockets.push(sock);
    }

    Ok(sockets)
}

struct TcpSocketOptionRefs<'py, 'a> {
    sol_socket: &'a Bound<'py, PyAny>,
    so_reuseaddr: &'a Bound<'py, PyAny>,
    so_reuseport: Option<&'a Bound<'py, PyAny>>,
    #[cfg(not(unix))]
    so_keepalive: &'a Bound<'py, PyAny>,
}

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
fn apply_tcp_server_socket_options(
    py: Python<'_>,
    sock: &Py<PyAny>,
    options: TcpSocketOptionRefs<'_, '_>,
    reuse_address: Option<bool>,
    reuse_port: Option<bool>,
    keep_alive: Option<bool>,
) -> PyResult<()> {
    if reuse_address == Some(true) {
        sock.call_method1(
            py,
            "setsockopt",
            (options.sol_socket.clone(), options.so_reuseaddr.clone(), 1),
        )?;
    }
    if reuse_port == Some(true)
        && let Some(so_reuseport) = options.so_reuseport
    {
        sock.call_method1(
            py,
            "setsockopt",
            (options.sol_socket.clone(), so_reuseport.clone(), 1),
        )?;
    }
    if let Some(keep_alive) = keep_alive {
        #[cfg(unix)]
        set_tcp_keepalive_option(py, sock, keep_alive)?;
        #[cfg(not(unix))]
        set_tcp_keepalive_option(py, sock, options, keep_alive)?;
    }
    Ok(())
}

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
#[cfg(unix)]
fn set_tcp_keepalive_option(py: Python<'_>, sock: &Py<PyAny>, keep_alive: bool) -> PyResult<()> {
    set_socket_bool_option_unix(py, sock, libc::SOL_SOCKET, libc::SO_KEEPALIVE, keep_alive)
}

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
#[cfg(not(unix))]
fn set_tcp_keepalive_option(
    py: Python<'_>,
    sock: &Py<PyAny>,
    options: TcpSocketOptionRefs<'_, '_>,
    keep_alive: bool,
) -> PyResult<()> {
    sock.call_method1(
        py,
        "setsockopt",
        (
            options.sol_socket.clone(),
            options.so_keepalive.clone(),
            i32::from(keep_alive),
        ),
    )?;
    Ok(())
}

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
#[cfg(unix)]
pub(super) fn build_unix_server_socket(
    py: Python<'_>,
    path: Option<Py<PyAny>>,
    backlog: i32,
) -> PyResult<Py<PyAny>> {
    let Some(path) = path else {
        return Err(PyRuntimeError::new_err(
            "path is required when sock is not provided",
        ));
    };

    let socket_mod = py.import("socket")?;
    let sock = socket_mod.getattr("socket")?.call1((
        socket_mod.getattr("AF_UNIX")?,
        socket_mod.getattr("SOCK_STREAM")?,
    ))?;
    sock.call_method1("setblocking", (false,))?;
    remove_unix_socket_if_present(&path.bind(py).extract::<String>()?)
        .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
    sock.call_method1("bind", (path,))?;
    sock.call_method1("listen", (backlog,))?;
    Ok(sock.unbind())
}

/// Builds the unnamed `AF_UNIX` socket used to dial a Unix server.
#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
#[cfg(unix)]
pub(super) fn build_unix_client_socket(py: Python<'_>) -> PyResult<Py<PyAny>> {
    let socket_mod = py.import("socket")?;
    let sock = socket_mod.getattr("socket")?.call1((
        socket_mod.getattr("AF_UNIX")?,
        socket_mod.getattr("SOCK_STREAM")?,
    ))?;
    sock.call_method1("setblocking", (false,))?;
    Ok(sock.unbind())
}
