//! Native `loop.getaddrinfo()` for exact loops.
//!
//! Through `run_in_executor`, `socket.getaddrinfo` needs the GIL on the
//! worker before and after the lookup, so every resolution of a host name
//! queues behind the loop thread twice. uvloop resolves in libuv's pool
//! without the GIL. Here the common argument shapes are encoded on the loop
//! thread as CPython's `_socket.getaddrinfo` encodes them, `getaddrinfo(3)`
//! runs on a blocking-pool worker without the GIL, and the loop thread builds
//! the rows or the error `socket.getaddrinfo` would produce. Every other shape
//! keeps the Python path.

use std::{
    ffi::{CStr, CString, c_char, c_int},
    net::Ipv4Addr,
    ptr,
};

use pyo3::{
    exceptions::PyValueError,
    prelude::*,
    sync::PyOnceLock,
    types::{PyBytes, PyInt, PyList, PyModule, PyString, PyTuple},
};

use super::{PyLoop, executor::AddrInfoRequest};
use crate::engine::{LoopCommand, LoopWork};

/// Arguments already encoded for `getaddrinfo(3)`.
struct Query {
    host: Option<CString>,
    port: Option<CString>,
    family: i32,
    sock_type: i32,
    proto: i32,
    flags: i32,
}

enum Address {
    V4(String, u16),
    V6(String, u16, u32, u32),
    /// Any other family: CPython reports `(family, sa_data)`.
    Other(i32, Vec<u8>),
}

struct Row {
    family: i32,
    sock_type: i32,
    proto: i32,
    canonname: Vec<u8>,
    /// None when the entry carries no address (`ai_addrlen == 0`).
    address: Option<Address>,
}

enum Outcome {
    Rows(Vec<Row>),
    /// A `getaddrinfo` error code other than `EAI_SYSTEM`.
    Gai(i32),
    /// `EAI_SYSTEM` with its `errno`.
    Os(i32),
}

/// Resolves `request` natively when the loop and the arguments allow it.
/// Returns None to keep the Python path: subclasses and custom default
/// executors stay authoritative, and unusual arguments keep CPython's
/// conversion and errors.
#[cfg_attr(feature = "profile", hotpath::measure)]
pub(super) fn getaddrinfo<'py>(
    slf: &Py<PyLoop>,
    py: Python<'py>,
    request: &AddrInfoRequest,
) -> PyResult<Option<Bound<'py, PyAny>>> {
    if !slf.bind(py).is_exact_instance_of::<PyLoop>() {
        return Ok(None);
    }
    {
        let loop_ref = slf.borrow(py);
        let state = loop_ref.core.state.lock().expect("poisoned loop state");
        if state.default_executor.is_some() || state.executor_shutdown_called {
            return Ok(None);
        }
    }
    if !stdlib_getaddrinfo_in_place(py)? {
        return Ok(None);
    }
    let host = request.host.as_ref().map(|host| host.bind(py));
    let port = request.port.as_ref().map(|port| port.bind(py));
    let (Some(host_arg), Some(port_arg)) = (
        encode_optional(host, encode_host),
        encode_optional(port, encode_port),
    ) else {
        return Ok(None);
    };
    let query = Query {
        host: host_arg,
        port: port_arg,
        family: request.family,
        sock_type: request.sock_type,
        proto: request.proto,
        flags: request.flags,
    };

    let future = super::tasks::create_future(slf.clone_ref(py), py)?;
    let none = py.None().into_bound(py);
    let audited = audit(py, host.unwrap_or(&none), port.unwrap_or(&none), request);
    match audited {
        Ok(_) => {
            let core = slf.borrow(py).core.clone();
            let pending = future.clone_ref(py);
            crate::blocking::spawn_bounded(move || {
                let outcome = query.resolve();
                let work = LoopWork(Box::new(move |py| complete(py, &pending, outcome)));
                // A closed loop drops the work; nobody awaits its future any
                // more.
                let _ = core.send_command(LoopCommand::LoopWork(Box::new(work)));
            });
        }
        Err(err) => {
            future.call_method1(py, "set_exception", (err.into_value(py),))?;
        }
    }
    Ok(Some(future.into_bound(py)))
}

unsafe extern "C" {
    // CPython's C audit API (pyo3-ffi does not bind it).
    fn PySys_Audit(event: *const c_char, format: *const c_char, ...) -> c_int;
}

/// Raises the audit event `_socket.getaddrinfo` raises before resolving,
/// through the C API as CPython does, so replacing the Python-level
/// `sys.audit` cannot skip a registered hook. A hook's error refuses the
/// lookup.
#[cfg_attr(feature = "profile", hotpath::measure)]
fn audit(
    py: Python<'_>,
    host: &Bound<'_, PyAny>,
    port: &Bound<'_, PyAny>,
    request: &AddrInfoRequest,
) -> PyResult<()> {
    // SAFETY: the GIL is held, the format matches the arguments (two
    // borrowed objects and three C ints, as in CPython's
    // socket_getaddrinfo), and a -1 return leaves the hook's error set.
    let code = unsafe {
        PySys_Audit(
            c"socket.getaddrinfo".as_ptr(),
            c"OOiii".as_ptr(),
            host.as_ptr(),
            port.as_ptr(),
            request.family,
            request.sock_type,
            request.proto,
        )
    };
    if code < 0 {
        return Err(PyErr::fetch(py));
    }
    Ok(())
}

/// Whether `socket.getaddrinfo` is still the standard library's function
/// and the `_socket.getaddrinfo` it delegates to is still CPython's,
/// checked on every lookup. The Python path calls whatever is installed
/// there, as asyncio does, so a test double or wrapper at either level keeps
/// receiving every name lookup. `functools.wraps` copies a function's name,
/// module and `__dict__`, so the function is recognised by what a wrapper
/// cannot copy: a plain function whose globals are the `socket` module's
/// namespace, whose code is named `getaddrinfo`, and which wraps nothing.
#[cfg_attr(feature = "profile", hotpath::measure)]
fn stdlib_getaddrinfo_in_place(py: Python<'_>) -> PyResult<bool> {
    static STDLIB: PyOnceLock<Py<PyAny>> = PyOnceLock::new();
    let socket = py.import("socket")?;
    let installed = socket.getattr("getaddrinfo")?;
    let is_stdlib = STDLIB
        .get(py)
        .is_some_and(|stdlib| installed.is(stdlib.bind(py)))
        || {
            let is_stdlib = installed
                .get_type()
                .is(&py.import("types")?.getattr("FunctionType")?)
                && installed
                    .getattr("__globals__")?
                    .is(&socket.getattr("__dict__")?)
                && installed
                    .getattr("__code__")?
                    .getattr("co_name")?
                    .eq("getaddrinfo")?
                && !installed.hasattr("__wrapped__")?;
            if is_stdlib {
                // A reloaded module's new function is recognised again above.
                let _ = STDLIB.set(py, installed.unbind());
            }
            is_stdlib
        };
    Ok(is_stdlib && builtin_delegate_in_place(py, &socket)?)
}

/// Whether the `_socket.getaddrinfo` that `socket.getaddrinfo` calls (the
/// `socket` module's `_socket` global) is CPython's builtin: a builtin
/// function of the `_socket` extension module named `getaddrinfo`. A
/// replaced delegate keeps the Python path.
#[cfg_attr(feature = "profile", hotpath::measure)]
fn builtin_delegate_in_place(py: Python<'_>, socket: &Bound<'_, PyModule>) -> PyResult<bool> {
    static BUILTIN: PyOnceLock<Py<PyAny>> = PyOnceLock::new();
    let delegate = socket.getattr("_socket")?.getattr("getaddrinfo")?;
    if BUILTIN
        .get(py)
        .is_some_and(|builtin| delegate.is(builtin.bind(py)))
    {
        return Ok(true);
    }
    let is_builtin = delegate
        .get_type()
        .is(&py.import("types")?.getattr("BuiltinFunctionType")?)
        && delegate.getattr("__self__")?.is(&py.import("_socket")?)
        && delegate.getattr("__name__")?.eq("getaddrinfo")?;
    if is_builtin {
        let _ = BUILTIN.set(py, delegate.unbind());
    }
    Ok(is_builtin)
}

/// `Some(None)` for a None argument, None when `encode` declines the value.
#[cfg_attr(feature = "profile", hotpath::measure)]
fn encode_optional(
    value: Option<&Bound<'_, PyAny>>,
    encode: fn(&Bound<'_, PyAny>) -> Option<CString>,
) -> Option<Option<CString>> {
    match value {
        None => Some(None),
        Some(value) if value.is_none() => Some(None),
        Some(value) => encode(value).map(Some),
    }
}

/// A `str` host in the IDNA codec's ASCII fast path (encoded unchanged), or
/// `bytes`. Other strings need the codec itself.
#[cfg_attr(feature = "profile", hotpath::measure)]
fn encode_host(host: &Bound<'_, PyAny>) -> Option<CString> {
    if let Ok(text) = host.cast_exact::<PyString>() {
        let name = text.to_str().ok()?;
        return idna_ascii_unchanged(name)
            .then(|| CString::new(name).ok())
            .flatten();
    }
    let bytes = host.cast_exact::<PyBytes>().ok()?;
    CString::new(bytes.as_bytes()).ok()
}

/// Whether `encodings.idna` returns `name.encode("ascii")` for `name`: ASCII,
/// no empty label before the last one, and no label of 64 bytes or more.
#[cfg_attr(feature = "profile", hotpath::measure)]
fn idna_ascii_unchanged(name: &str) -> bool {
    if name.is_empty() || !name.is_ascii() {
        return false;
    }
    let labels: Vec<&str> = name.split('.').collect();
    let last = labels.len() - 1;
    labels
        .iter()
        .enumerate()
        .all(|(index, label)| label.len() < 64 && (index == last || !label.is_empty()))
}

/// An `int` port in range (formatted as CPython formats it), or a `str` /
/// `bytes` service.
#[cfg_attr(feature = "profile", hotpath::measure)]
fn encode_port(port: &Bound<'_, PyAny>) -> Option<CString> {
    if let Ok(number) = port.cast_exact::<PyInt>() {
        return CString::new(number.extract::<u16>().ok()?.to_string()).ok();
    }
    if let Ok(text) = port.cast_exact::<PyString>() {
        return CString::new(text.to_str().ok()?).ok();
    }
    let bytes = port.cast_exact::<PyBytes>().ok()?;
    CString::new(bytes.as_bytes()).ok()
}

impl Query {
    /// Calls `getaddrinfo(3)`; runs on a pool worker without the GIL.
    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "Query"))]
    fn resolve(&self) -> Outcome {
        // SAFETY: an all-zero addrinfo is the documented empty hints value.
        let mut hints: libc::addrinfo = unsafe { std::mem::zeroed() };
        hints.ai_family = self.family;
        hints.ai_socktype = self.sock_type;
        hints.ai_protocol = self.proto;
        hints.ai_flags = self.flags;
        let mut list: *mut libc::addrinfo = ptr::null_mut();
        // SAFETY: host and port are NUL-terminated or null, hints is
        // initialized, and list receives a result freed below.
        let code = unsafe {
            libc::getaddrinfo(
                self.host.as_ref().map_or(ptr::null(), |host| host.as_ptr()),
                self.port.as_ref().map_or(ptr::null(), |port| port.as_ptr()),
                &raw const hints,
                &raw mut list,
            )
        };
        if code == libc::EAI_SYSTEM {
            return Outcome::Os(std::io::Error::last_os_error().raw_os_error().unwrap_or(0));
        }
        if code != 0 {
            return Outcome::Gai(code);
        }
        let mut rows = Vec::new();
        let mut entry = list;
        while !entry.is_null() {
            // SAFETY: entry is a node of the list getaddrinfo returned.
            let info = unsafe { &*entry };
            rows.push(Row {
                family: info.ai_family,
                sock_type: info.ai_socktype,
                proto: info.ai_protocol,
                canonname: if info.ai_canonname.is_null() {
                    Vec::new()
                } else {
                    // SAFETY: a non-null ai_canonname is NUL-terminated.
                    unsafe { CStr::from_ptr(info.ai_canonname) }
                        .to_bytes()
                        .to_vec()
                },
                address: address(info),
            });
            entry = info.ai_next;
        }
        // SAFETY: list came from a successful getaddrinfo and is freed once.
        unsafe { libc::freeaddrinfo(list) };
        Outcome::Rows(rows)
    }
}

/// The sockaddr of one entry, decoded as CPython's `makesockaddr` does.
#[cfg_attr(feature = "profile", hotpath::measure)]
fn address(info: &libc::addrinfo) -> Option<Address> {
    if info.ai_addr.is_null() || info.ai_addrlen == 0 {
        return None;
    }
    let length = info.ai_addrlen as usize;
    // SAFETY: ai_addr points to ai_addrlen bytes; each family is read only
    // when it is at least as long as its sockaddr type.
    unsafe {
        let family = i32::from((*info.ai_addr).sa_family);
        if family == libc::AF_INET && length >= size_of::<libc::sockaddr_in>() {
            let v4 = &*info.ai_addr.cast::<libc::sockaddr_in>();
            let ip = Ipv4Addr::from(u32::from_be(v4.sin_addr.s_addr));
            return Some(Address::V4(ip.to_string(), u16::from_be(v4.sin_port)));
        }
        if family == libc::AF_INET6 && length >= size_of::<libc::sockaddr_in6>() {
            let v6 = &*info.ai_addr.cast::<libc::sockaddr_in6>();
            return Some(Address::V6(
                ntop6(v6.sin6_addr.s6_addr),
                u16::from_be(v6.sin6_port),
                u32::from_be(v6.sin6_flowinfo),
                v6.sin6_scope_id,
            ));
        }
        Some(Address::Other(
            family,
            (*info.ai_addr)
                .sa_data
                .iter()
                .map(|byte| *byte as u8)
                .collect(),
        ))
    }
}

/// glibc's `inet_ntop(AF_INET6)`, which CPython uses for IPv6 addresses.
/// Rust's own formatting differs for IPv4-compatible addresses (`::1.2.3.4`).
#[cfg_attr(feature = "profile", hotpath::measure)]
fn ntop6(octets: [u8; 16]) -> String {
    use std::fmt::Write;

    let words: [u16; 8] =
        std::array::from_fn(|index| u16::from_be_bytes([octets[2 * index], octets[2 * index + 1]]));
    // The first longest run of at least two zero words becomes "::".
    // (first word, length) of a run of zero words.
    let mut best: Option<(usize, usize)> = None;
    let mut current: Option<(usize, usize)> = None;
    for (index, word) in words.iter().enumerate() {
        if *word == 0 {
            current = Some(current.map_or((index, 1), |(base, len)| (base, len + 1)));
        } else if let Some(run) = current.take()
            && best.is_none_or(|(_, len)| run.1 > len)
        {
            best = Some(run);
        }
    }
    if let Some(run) = current
        && best.is_none_or(|(_, len)| run.1 > len)
    {
        best = Some(run);
    }
    let best = best.filter(|(_, len)| *len >= 2);
    let mut text = String::with_capacity(46);
    for (index, word) in words.iter().enumerate() {
        if let Some((base, len)) = best
            && (base..base + len).contains(&index)
        {
            if index == base {
                text.push(':');
            }
            continue;
        }
        if index != 0 {
            text.push(':');
        }
        if index == 6
            && let Some((0, len)) = best
            && (len == 6 || (len == 5 && words[5] == 0xffff))
        {
            let _ = write!(
                text,
                "{}",
                Ipv4Addr::new(octets[12], octets[13], octets[14], octets[15])
            );
            return text;
        }
        let _ = write!(text, "{word:x}");
    }
    if best.is_some_and(|(base, len)| base + len == 8) {
        text.push(':');
    }
    text
}

/// Settles the future on the loop thread with what `socket.getaddrinfo`
/// would have returned or raised.
#[cfg_attr(feature = "profile", hotpath::measure)]
fn complete(py: Python<'_>, future: &Py<PyAny>, outcome: Outcome) -> PyResult<()> {
    let future = future.bind(py);
    if crate::python_names::call_method0(py, future, crate::python_names::done(py))?
        .bind(py)
        .extract::<bool>()?
    {
        return Ok(());
    }
    let settled = match outcome {
        Outcome::Rows(rows) => rows_to_list(py, rows),
        Outcome::Gai(code) => Err(gaierror(py, code)),
        Outcome::Os(errno) => Err(os_error(py, errno)),
    };
    match settled {
        Ok(list) => crate::python_names::call_method1(
            py,
            future,
            crate::python_names::set_result(py),
            list.as_any(),
        ),
        Err(err) => crate::python_names::call_method1(
            py,
            future,
            crate::python_names::set_exception(py),
            err.into_value(py).bind(py).as_any(),
        ),
    }?;
    Ok(())
}

#[cfg_attr(feature = "profile", hotpath::measure)]
fn rows_to_list(py: Python<'_>, rows: Vec<Row>) -> PyResult<Bound<'_, PyList>> {
    let socket = py.import("socket")?;
    let families = socket.getattr("AddressFamily")?;
    let kinds = socket.getattr("SocketKind")?;
    let list = PyList::empty(py);
    for row in rows {
        let canonname = match std::str::from_utf8(&row.canonname) {
            Ok(text) => PyString::new(py, text).into_any(),
            // CPython decodes strictly; let the codec raise its own error.
            Err(_) => PyBytes::new(py, &row.canonname).call_method1("decode", ("utf-8",))?,
        };
        let address = match row.address {
            None => py.None().into_bound(py),
            Some(Address::V4(host, port)) => (host, port).into_pyobject(py)?.into_any(),
            Some(Address::V6(host, port, flowinfo, scope_id)) => (host, port, flowinfo, scope_id)
                .into_pyobject(py)?
                .into_any(),
            Some(Address::Other(family, data)) => (family, PyBytes::new(py, &data))
                .into_pyobject(py)?
                .into_any(),
        };
        list.append(PyTuple::new(
            py,
            [
                int_enum(&families, row.family)?,
                int_enum(&kinds, row.sock_type)?,
                row.proto.into_pyobject(py)?.into_any(),
                canonname,
                address,
            ],
        )?)?;
    }
    Ok(list)
}

/// `socket._intenum_converter`: the enum member, or the plain int.
#[cfg_attr(feature = "profile", hotpath::measure)]
fn int_enum<'py>(class: &Bound<'py, PyAny>, value: i32) -> PyResult<Bound<'py, PyAny>> {
    let py = class.py();
    match class.call1((value,)) {
        Ok(member) => Ok(member),
        Err(err) if err.is_instance_of::<PyValueError>(py) => {
            Ok(value.into_pyobject(py)?.into_any())
        }
        Err(err) => Err(err),
    }
}

/// `socket.gaierror(code, gai_strerror(code))`, as CPython raises it.
#[cfg_attr(feature = "profile", hotpath::measure)]
fn gaierror(py: Python<'_>, code: i32) -> PyErr {
    // SAFETY: gai_strerror returns a static NUL-terminated message.
    let message = unsafe { CStr::from_ptr(libc::gai_strerror(code)) }
        .to_string_lossy()
        .into_owned();
    match py
        .import("socket")
        .and_then(|socket| socket.getattr("gaierror"))
        .and_then(|class| class.call1((code, message)))
    {
        Ok(error) => PyErr::from_value(error),
        Err(err) => err,
    }
}

/// The `OSError` `PyErr_SetFromErrno` raises for `EAI_SYSTEM`:
/// `OSError(errno, os.strerror(errno))`, which picks the errno's subclass, or
/// `OSError(0, "Error")` when `errno` was not set.
#[cfg_attr(feature = "profile", hotpath::measure)]
fn os_error(py: Python<'_>, errno: i32) -> PyErr {
    let message = if errno == 0 {
        Ok(PyString::new(py, "Error").into_any())
    } else {
        py.import("os")
            .and_then(|os| os.call_method1("strerror", (errno,)))
    };
    match message.and_then(|message| {
        py.get_type::<pyo3::exceptions::PyOSError>()
            .call1((errno, message))
    }) {
        Ok(error) => PyErr::from_value(error),
        Err(err) => err,
    }
}

#[cfg(test)]
mod tests {
    use pyo3::types::IntoPyDict;

    use super::*;

    fn v6(text: &str) -> String {
        ntop6(text.parse::<std::net::Ipv6Addr>().unwrap().octets())
    }

    #[test]
    fn ipv6_text_matches_glibc_inet_ntop() {
        // Expected values are glibc's (socket.inet_ntop on Linux).
        for (address, expected) in [
            ("::", "::"),
            ("::1", "::1"),
            ("::1.2.3.4", "::1.2.3.4"),
            ("::ffff:1.2.3.4", "::ffff:1.2.3.4"),
            ("::ffff:0:0", "::ffff:0.0.0.0"),
            ("::0.0.0.1", "::1"),
            ("1::", "1::"),
            ("fe80::1", "fe80::1"),
            ("2001:db8::ff00:42:8329", "2001:db8::ff00:42:8329"),
            ("2001:db8:0:0:1:0:0:1", "2001:db8::1:0:0:1"),
            ("2001:0:0:1:0:0:0:1", "2001:0:0:1::1"),
            ("1:0:2:0:3:0:4:0", "1:0:2:0:3:0:4:0"),
            ("1:2:3:4:5:6:7:8", "1:2:3:4:5:6:7:8"),
            ("0:0:1::", "0:0:1::"),
            ("::1:0:0:0:0", "0:0:0:1::"),
            ("64:ff9b::1.2.3.4", "64:ff9b::102:304"),
        ] {
            assert_eq!(v6(address), expected, "{address}");
        }
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn eai_system_errors_match_pyerr_set_from_errno() {
        // socket.getaddrinfo raises EAI_SYSTEM through PyErr_SetFromErrno.
        crate::initialize_python_for_tests();
        Python::attach(|py| {
            let args = |err: &PyErr| {
                err.value(py)
                    .getattr("args")
                    .and_then(|args| args.repr())
                    .unwrap()
                    .to_string()
            };
            for errno in [0, libc::ENOENT, libc::EAGAIN, libc::ENOMEM, libc::EMFILE] {
                // SAFETY: errno belongs to this thread and is read at once by
                // PyErr_SetFromErrno, whose error PyErr::fetch takes back.
                let expected = unsafe {
                    *libc::__errno_location() = errno;
                    pyo3::ffi::PyErr_SetFromErrno(pyo3::ffi::PyExc_OSError);
                    PyErr::fetch(py)
                };
                let actual = os_error(py, errno);
                assert!(
                    actual.get_type(py).is(expected.get_type(py)),
                    "{errno}: {actual} vs {expected}"
                );
                assert_eq!(args(&actual), args(&expected), "{errno}");
            }
        });
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn lookups_run_as_many_at_once_as_asyncios_default_executor() {
        crate::initialize_python_for_tests();
        let expected = Python::attach(|py| {
            // ThreadPoolExecutor's default max_workers on Python 3.13+, whose
            // os.process_cpu_count() counts the affinity mask.
            py.eval(
                c"min(32, (os.process_cpu_count() if hasattr(os, 'process_cpu_count') else len(os.sched_getaffinity(0))) + 4)",
                Some(&[("os", py.import("os").unwrap())].into_py_dict(py).unwrap()),
                None,
            )
            .unwrap()
            .extract::<usize>()
            .unwrap()
        });
        assert_eq!(crate::blocking::bounded_limit(), expected);
    }

    #[test]
    fn idna_fast_path_matches_the_codec() {
        assert!(idna_ascii_unchanged("localhost"));
        assert!(idna_ascii_unchanged("localhost."));
        assert!(idna_ascii_unchanged("a.b.c"));
        assert!(idna_ascii_unchanged(&"x".repeat(63)));
        assert!(!idna_ascii_unchanged(&"x".repeat(64)));
        assert!(!idna_ascii_unchanged(""));
        assert!(!idna_ascii_unchanged("a..b"));
        assert!(!idna_ascii_unchanged(".a"));
        assert!(!idna_ascii_unchanged("ünï.example"));
    }
}
