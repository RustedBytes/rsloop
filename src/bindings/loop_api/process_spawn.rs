//! Subprocess spawning: `Popen`-style keyword translation and the shared
//! `subprocess_shell` / `subprocess_exec` pipeline.
//!
//! `asyncio` forwards arbitrary `subprocess.Popen` keywords through to the
//! loop, so the keyword handling below mirrors `Popen`'s semantics (including
//! `umask=-1` meaning "leave unchanged" and `preexec_fn` being rejected rather
//! than emulated). The Unix-only keywords are collected into
//! [`UnixPreExecConfig`] and applied between fork and exec by
//! [`super::pre_exec`].
//!
//! Every keyword `Popen` accepts has to be accepted here at its default value,
//! even the ones rsloop cannot act on: callers wrap this API and forward the
//! whole set unconditionally, so rejecting a defaulted keyword breaks them for
//! no gain. Unrecognised keywords are still an error, which is what catches a
//! typo or a genuinely unsupported option.

#[cfg(unix)]
use std::os::unix::process::CommandExt;
#[cfg(windows)]
use std::os::windows::process::CommandExt;
use std::process::Command;

use pyo3::{
    exceptions::{PyNotImplementedError, PyRuntimeError, PyTypeError, PyValueError},
    prelude::*,
    types::{PyDict, PyTuple},
};

use super::{
    PyLoop, pre_exec,
    process_stdio::{ProcessStdioSpecs, apply_stdio},
    spawn_env::{LoopSpawnEnv, is_asyncio_subprocess_stream_protocol, transport_protocol_pair},
};
use crate::transport::process::{
    ProcessTextConfig, ProcessTransportParams, spawn_process_transport,
};

const PROCESS_UMASK_MAX: i64 = 0o777;

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProcessTextMode {
    Binary,
    Text,
    Conflict,
}

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
fn process_text_mode(
    universal_newlines: bool,
    text: Option<bool>,
    has_encoding: bool,
    has_errors: bool,
) -> ProcessTextMode {
    if universal_newlines && text == Some(false) {
        ProcessTextMode::Conflict
    } else if universal_newlines || text == Some(true) || has_encoding || has_errors {
        ProcessTextMode::Text
    } else {
        ProcessTextMode::Binary
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum NormalizedUmask {
    Unchanged,
    Value(u32),
    Invalid,
}

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
fn normalize_process_umask(mask: i64) -> NormalizedUmask {
    if mask == -1 {
        NormalizedUmask::Unchanged
    } else if (0..=PROCESS_UMASK_MAX).contains(&mask) {
        NormalizedUmask::Value(
            u32::try_from(mask).expect("validated umask is nonnegative and fits u32"),
        )
    } else {
        NormalizedUmask::Invalid
    }
}

#[derive(Clone)]
pub(super) struct UnixPreExecConfig {
    pub(super) restore_signals: bool,
    pub(super) start_new_session: bool,
    pub(super) process_group: Option<i32>,
    pub(super) pass_fds: Vec<i32>,
    pub(super) gid: Option<u32>,
    pub(super) extra_groups: Option<Vec<u32>>,
    pub(super) uid: Option<u32>,
    pub(super) umask: Option<u32>,
}

#[derive(Clone)]
struct ProcessSpawnConfig {
    text: Option<ProcessTextConfig>,
    unix: UnixPreExecConfig,
}

impl Default for UnixPreExecConfig {
    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "<UnixPreExecConfig as Default>")
    )]
    fn default() -> Self {
        Self {
            restore_signals: true,
            start_new_session: false,
            process_group: None,
            pass_fds: Vec::new(),
            gid: None,
            extra_groups: None,
            uid: None,
            umask: None,
        }
    }
}

/// Everything `subprocess_shell` and `subprocess_exec` resolve on the calling
/// thread before the spawn future starts.
pub(super) struct SubprocessParams {
    pub(super) protocol_factory: Py<PyAny>,
    pub(super) specs: ProcessStdioSpecs,
    pub(super) text_config: Option<ProcessTextConfig>,
    pub(super) kwargs: Option<Py<PyDict>>,
    /// `asyncio` helper this call came from, used for the text-mode message.
    pub(super) api_name: &'static str,
}

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
/// Shared tail of both subprocess methods: build the protocol, spawn the child,
/// and wrap it in a transport. Only the `Command` construction differs, so it
/// arrives as a closure that runs under the GIL on the spawning thread.
pub(super) fn spawn_subprocess<'py, F>(
    slf: &Py<PyLoop>,
    py: Python<'py>,
    params: SubprocessParams,
    build_command: F,
) -> PyResult<Bound<'py, PyAny>>
where
    F: FnOnce(Python<'_>) -> PyResult<Command> + Send + 'static,
{
    let locals = PyLoop::task_locals(py, slf)?;
    let env = LoopSpawnEnv::capture(py, slf)?;
    let SubprocessParams {
        protocol_factory,
        specs,
        text_config,
        kwargs,
        api_name,
    } = params;

    pyo3_async_runtimes::async_std::future_into_py_with_locals(py, locals, async move {
        let protocol = Python::attach(|py| env.call_protocol_factory(py, &protocol_factory))?;
        if text_config.is_some()
            && Python::attach(|py| is_asyncio_subprocess_stream_protocol(py, &protocol))?
        {
            return Err(PyValueError::new_err(format!(
                "text mode is not supported with asyncio.{api_name}() in rust-impl yet"
            )));
        }

        let (child, spawn_config, stdout_override, stderr_override) =
            Python::attach(|py| -> PyResult<_> {
                let mut command = build_command(py)?;
                let (stdout_override, stderr_override) = apply_stdio(&mut command, specs)?;
                let spawn_config = apply_common_process_kwargs(
                    py,
                    &mut command,
                    kwargs.as_ref().map(|kwargs| kwargs.bind(py)),
                )?;
                let child = command
                    .spawn()
                    .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
                Ok((child, spawn_config, stdout_override, stderr_override))
            })?;

        let transport = Python::attach(|py| {
            spawn_process_transport(
                py,
                ProcessTransportParams::new(env.spawn_context(py, &protocol), child)
                    .with_text_config(text_config.or(spawn_config.text))
                    .with_stdio_overrides(stdout_override, stderr_override),
            )
        })?;

        Python::attach(|py| transport_protocol_pair(py, transport.into_any(), &protocol))
    })
}

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
/// `/bin/sh -c <cmd>` on Unix, `%COMSPEC% /c "<cmd>"` on Windows.
pub(super) fn shell_command(py: Python<'_>, cmd: &Py<PyAny>) -> PyResult<Command> {
    let shell_cmd = cmd.bind(py).extract::<String>()?;
    #[cfg(unix)]
    {
        let mut command = Command::new("/bin/sh");
        command.arg("-c");
        command.arg(&shell_cmd);
        Ok(command)
    }
    #[cfg(windows)]
    {
        let mut command =
            Command::new(std::env::var_os("COMSPEC").unwrap_or_else(|| "cmd.exe".into()));
        command.raw_arg(format!(" /c \"{shell_cmd}\""));
        Ok(command)
    }
}

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
pub(super) fn exec_command(
    py: Python<'_>,
    program: &Py<PyAny>,
    argv: &Py<PyTuple>,
) -> PyResult<Command> {
    let mut command = Command::new(program.bind(py).extract::<String>()?);
    for arg in argv.bind(py).iter() {
        command.arg(arg.extract::<String>()?);
    }
    Ok(command)
}

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
fn resolve_numeric_id(
    py: Python<'_>,
    value: &Bound<'_, PyAny>,
    module_name: &str,
    lookup: &str,
    field: &str,
    label: &str,
) -> PyResult<u32> {
    if let Ok(id) = value.extract::<u32>() {
        return Ok(id);
    }
    if let Ok(name) = value.extract::<String>() {
        let module = py.import(module_name)?;
        let entry = module.getattr(lookup)?.call1((name,))?;
        return entry.getattr(field)?.extract::<u32>();
    }
    Err(PyTypeError::new_err(format!(
        "{label} must be an int or str"
    )))
}

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
fn resolve_extra_groups(py: Python<'_>, value: &Bound<'_, PyAny>) -> PyResult<Vec<u32>> {
    let mut groups = Vec::new();
    for item in value.try_iter()? {
        let item = item?;
        groups.push(resolve_numeric_id(
            py,
            &item,
            "grp",
            "getgrnam",
            "gr_gid",
            "extra_groups entries",
        )?);
    }
    Ok(groups)
}

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
pub(super) fn parse_process_text_config(
    py: Python<'_>,
    universal_newlines: bool,
    encoding: Option<Py<PyAny>>,
    errors: Option<Py<PyAny>>,
    text: Option<bool>,
) -> PyResult<Option<ProcessTextConfig>> {
    match process_text_mode(
        universal_newlines,
        text,
        encoding.is_some(),
        errors.is_some(),
    ) {
        ProcessTextMode::Conflict => {
            return Err(PyValueError::new_err(
                "text and universal_newlines have different values",
            ));
        }
        ProcessTextMode::Binary => return Ok(None),
        ProcessTextMode::Text => {}
    }
    let encoding = if let Some(encoding) = encoding {
        encoding.bind(py).extract::<String>()?
    } else {
        py.import("locale")?
            .getattr("getpreferredencoding")?
            .call1((false,))?
            .extract::<String>()?
    };
    let errors = if let Some(errors) = errors {
        errors.bind(py).extract::<String>()?
    } else {
        "strict".to_owned()
    };
    Ok(Some(ProcessTextConfig {
        encoding,
        errors,
        translate_newlines: true,
    }))
}

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
fn apply_process_basic_kw(
    py: Python<'_>,
    command: &mut Command,
    key: &str,
    value: &Bound<'_, PyAny>,
) -> PyResult<bool> {
    match key {
        "cwd" => apply_process_cwd(py, command, value).map(|()| true),
        "env" => apply_process_env(command, value).map(|()| true),
        "executable" => apply_process_executable(py, command, value).map(|()| true),
        _ => Ok(false),
    }
}

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
/// `Popen` keywords that are platform-specific or already implied by the way
/// rsloop spawns. Wrappers around `loop.subprocess_exec()` — `AnyIO` is the one
/// that caught this — forward every one of them unconditionally at its
/// documented default, so accepting the default has to be a no-op. A
/// non-default value gets the same error `Popen` raises, or an explicit refusal
/// where rsloop cannot honour it, rather than being silently dropped.
fn apply_platform_process_kw(
    command: &mut Command,
    key: &str,
    value: &Bound<'_, PyAny>,
) -> PyResult<bool> {
    match key {
        // rsloop inherits only the redirected stdio plus whatever `pass_fds`
        // names, which is what close_fds=True asks for. `std`'s Command has no
        // way to ask for the opposite.
        "close_fds" => {
            if !value.is_none() && !value.is_truthy()? {
                return Err(PyValueError::new_err(
                    "close_fds=False is not supported: rsloop relies on close-on-exec, \
                     so name the descriptors the child needs in pass_fds instead",
                ));
            }
        }
        // A pipe capacity hint, and only that. CPython drops it too on any
        // platform without F_SETPIPE_SZ (macOS included), so accepting and
        // ignoring it keeps working callers working instead of failing them on
        // a performance knob.
        "pipesize" => {}
        "startupinfo" => {
            if !value.is_none() {
                return Err(windows_only_process_kw_error("startupinfo"));
            }
        }
        "creationflags" => {
            let flags = if value.is_none() {
                0
            } else {
                value.extract::<i64>()?
            };
            if flags != 0 {
                apply_process_creation_flags(command, flags)?;
            }
        }
        _ => return Ok(false),
    }
    Ok(true)
}

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
#[cfg(windows)]
fn windows_only_process_kw_error(key: &str) -> PyErr {
    PyNotImplementedError::new_err(format!(
        "{key} is accepted only at its default value: rsloop cannot forward it to CreateProcess yet"
    ))
}

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
#[cfg(not(windows))]
fn windows_only_process_kw_error(key: &str) -> PyErr {
    // Same wording as subprocess.Popen so callers recognise it.
    PyValueError::new_err(format!("{key} is only supported on Windows platforms"))
}

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
#[cfg(windows)]
fn apply_process_creation_flags(command: &mut Command, flags: i64) -> PyResult<()> {
    let flags = u32::try_from(flags).map_err(|_| {
        PyValueError::new_err("creationflags must fit in an unsigned 32-bit integer")
    })?;
    // `std` ORs this with the flags it needs itself, so passing the caller's
    // value straight through is safe.
    command.creation_flags(flags);
    Ok(())
}

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
#[cfg(not(windows))]
fn apply_process_creation_flags(_command: &mut Command, _flags: i64) -> PyResult<()> {
    Err(windows_only_process_kw_error("creationflags"))
}

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
fn process_fspath(py: Python<'_>, value: &Bound<'_, PyAny>) -> PyResult<String> {
    py.import("os")?
        .getattr("fspath")?
        .call1((value.clone(),))?
        .extract::<String>()
}

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
fn apply_process_cwd(
    py: Python<'_>,
    command: &mut Command,
    value: &Bound<'_, PyAny>,
) -> PyResult<()> {
    if !value.is_none() {
        command.current_dir(process_fspath(py, value)?);
    }
    Ok(())
}

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
fn apply_process_env(command: &mut Command, value: &Bound<'_, PyAny>) -> PyResult<()> {
    if !value.is_none() {
        for (env_key, env_value) in value.cast::<PyDict>()?.iter() {
            command.env(env_key.extract::<String>()?, env_value.extract::<String>()?);
        }
    }
    Ok(())
}

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
fn apply_process_executable(
    py: Python<'_>,
    command: &mut Command,
    value: &Bound<'_, PyAny>,
) -> PyResult<()> {
    if !value.is_none() {
        let executable = process_fspath(py, value)?;
        #[cfg(unix)]
        command.arg0(executable);
        #[cfg(windows)]
        {
            let _ = command;
            drop(executable);
            return Err(PyNotImplementedError::new_err(
                "subprocess executable override is not implemented on Windows",
            ));
        }
    }
    Ok(())
}

struct UnixProcessKw<'a, 'py> {
    unix: &'a mut UnixPreExecConfig,
    key: &'a str,
    value: &'a Bound<'py, PyAny>,
}

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
fn apply_unix_process_kw(
    py: Python<'_>,
    unix: &mut UnixPreExecConfig,
    key: &str,
    value: &Bound<'_, PyAny>,
) -> PyResult<bool> {
    let mut kw = UnixProcessKw { unix, key, value };
    if apply_unix_bool_process_kw(&mut kw)? {
        return Ok(true);
    }
    if kw.value.is_none() {
        return Ok(is_known_unix_process_kw(key));
    }
    if apply_unix_fd_process_kw(&mut kw)? {
        return Ok(true);
    }
    if apply_unix_identity_process_kw(py, &mut kw)? {
        return Ok(true);
    }
    apply_unix_misc_process_kw(&mut kw)
}

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
fn is_known_unix_process_kw(key: &str) -> bool {
    matches!(
        key,
        "process_group" | "pass_fds" | "group" | "extra_groups" | "user" | "umask" | "preexec_fn"
    )
}

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
fn apply_unix_fd_process_kw(kw: &mut UnixProcessKw<'_, '_>) -> PyResult<bool> {
    match kw.key {
        "process_group" => kw.unix.process_group = Some(kw.value.extract::<i32>()?),
        "pass_fds" => {
            kw.unix.pass_fds = kw
                .value
                .try_iter()?
                .map(|item| item.and_then(|value| value.extract::<i32>()))
                .collect::<PyResult<Vec<_>>>()?;
        }
        _ => return Ok(false),
    }
    Ok(true)
}

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
fn apply_unix_identity_process_kw(
    py: Python<'_>,
    kw: &mut UnixProcessKw<'_, '_>,
) -> PyResult<bool> {
    match kw.key {
        "group" => {
            kw.unix.gid = Some(resolve_numeric_id(
                py, kw.value, "grp", "getgrnam", "gr_gid", "group",
            )?);
        }
        "extra_groups" => {
            kw.unix.extra_groups = Some(resolve_extra_groups(py, kw.value)?);
        }
        "user" => {
            kw.unix.uid = Some(resolve_numeric_id(
                py, kw.value, "pwd", "getpwnam", "pw_uid", "user",
            )?);
        }
        _ => return Ok(false),
    }
    Ok(true)
}

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
fn apply_unix_misc_process_kw(kw: &mut UnixProcessKw<'_, '_>) -> PyResult<bool> {
    match kw.key {
        "umask" => {
            let mask = kw.value.extract::<i64>()?;
            match normalize_process_umask(mask) {
                NormalizedUmask::Unchanged => {}
                NormalizedUmask::Value(mask) => kw.unix.umask = Some(mask),
                NormalizedUmask::Invalid => {
                    return Err(PyValueError::new_err("umask must be between 0 and 0o777"));
                }
            }
        }
        "preexec_fn" => {
            return Err(PyNotImplementedError::new_err(
                "preexec_fn remains unsupported in rust-impl because it is unsafe in this runtime model",
            ));
        }
        _ => return Ok(false),
    }
    Ok(true)
}

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
fn apply_unix_bool_process_kw(kw: &mut UnixProcessKw<'_, '_>) -> PyResult<bool> {
    match kw.key {
        "restore_signals" => kw.unix.restore_signals = kw.value.is_truthy()?,
        "start_new_session" => kw.unix.start_new_session = kw.value.is_truthy()?,
        _ => return Ok(false),
    }
    Ok(true)
}

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
fn apply_common_process_kwargs(
    py: Python<'_>,
    command: &mut Command,
    kwargs: Option<&Bound<'_, PyDict>>,
) -> PyResult<ProcessSpawnConfig> {
    let mut spawn_config = ProcessSpawnConfig {
        text: None,
        unix: UnixPreExecConfig::default(),
    };
    let Some(kwargs) = kwargs else {
        return Ok(spawn_config);
    };

    for (key, value) in kwargs.iter() {
        let key = key.extract::<String>()?;
        if apply_process_basic_kw(py, command, &key, &value)? {
            continue;
        }
        if apply_platform_process_kw(command, &key, &value)? {
            continue;
        }
        if !apply_unix_process_kw(py, &mut spawn_config.unix, &key, &value)? {
            return Err(PyTypeError::new_err(format!(
                "unsupported subprocess keyword: {key}"
            )));
        }
    }

    pre_exec::apply(command, spawn_config.unix.clone());
    Ok(spawn_config)
}

#[cfg(kani)]
mod verification {
    use super::*;

    #[kani::proof]
    fn merge_process_text_mode_precedence_is_exact() {
        let universal_newlines: bool = kani::any();
        let text: Option<bool> = kani::any();
        let has_encoding: bool = kani::any();
        let has_errors: bool = kani::any();

        let mode = process_text_mode(universal_newlines, text, has_encoding, has_errors);
        if universal_newlines && text == Some(false) {
            assert_eq!(mode, ProcessTextMode::Conflict);
        } else if universal_newlines || text == Some(true) || has_encoding || has_errors {
            assert_eq!(mode, ProcessTextMode::Text);
        } else {
            assert_eq!(mode, ProcessTextMode::Binary);
        }
    }

    #[kani::proof]
    fn merge_process_umask_normalization_is_exact() {
        let mask: i64 = kani::any();
        match normalize_process_umask(mask) {
            NormalizedUmask::Unchanged => assert_eq!(mask, -1),
            NormalizedUmask::Value(value) => {
                assert!((0..=PROCESS_UMASK_MAX).contains(&mask));
                assert_eq!(i64::from(value), mask);
            }
            NormalizedUmask::Invalid => {
                assert_ne!(mask, -1);
                assert!(!(0..=PROCESS_UMASK_MAX).contains(&mask));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use pyo3::types::{PyDict, PyString, PyTuple};

    use super::*;

    fn py_string(py: Python<'_>, value: &str) -> Py<PyAny> {
        PyString::new(py, value).unbind().into_any()
    }

    #[test]
    fn shell_and_exec_commands_preserve_program_and_argument_boundaries() {
        crate::initialize_python_for_tests();
        Python::attach(|py| {
            let shell = shell_command(py, &py_string(py, "printf '%s' hello world"))
                .expect("build shell command");
            #[cfg(unix)]
            {
                assert_eq!(shell.get_program(), OsStr::new("/bin/sh"));
                assert_eq!(
                    shell.get_args().collect::<Vec<_>>(),
                    [OsStr::new("-c"), OsStr::new("printf '%s' hello world")]
                );
            }
            #[cfg(windows)]
            assert_eq!(
                shell.get_program(),
                std::env::var_os("COMSPEC").unwrap_or_else(|| "cmd.exe".into())
            );

            let argv = PyTuple::new(py, ["one", "two words"])
                .expect("argv")
                .unbind();
            let command =
                exec_command(py, &py_string(py, "program"), &argv).expect("build exec command");
            assert_eq!(command.get_program(), OsStr::new("program"));
            assert_eq!(
                command.get_args().collect::<Vec<_>>(),
                [OsStr::new("one"), OsStr::new("two words")]
            );
        });
    }

    #[test]
    fn text_mode_parsing_handles_defaults_overrides_and_conflicts() {
        crate::initialize_python_for_tests();
        Python::attach(|py| {
            assert!(
                parse_process_text_config(py, false, None, None, None)
                    .expect("binary mode")
                    .is_none()
            );

            let explicit = parse_process_text_config(
                py,
                false,
                Some(py_string(py, "utf-16")),
                Some(py_string(py, "replace")),
                Some(true),
            )
            .expect("explicit text mode")
            .expect("text config");
            assert_eq!(explicit.encoding, "utf-16");
            assert_eq!(explicit.errors, "replace");
            assert!(explicit.translate_newlines);

            let defaults = parse_process_text_config(py, true, None, None, None)
                .expect("default text mode")
                .expect("text config");
            assert!(!defaults.encoding.is_empty());
            assert_eq!(defaults.errors, "strict");

            let err = parse_process_text_config(py, true, None, None, Some(false))
                .err()
                .expect("conflicting text flags should fail");
            assert!(err.is_instance_of::<PyValueError>(py));
        });
    }

    #[test]
    fn common_kwargs_apply_cwd_and_environment_and_reject_unknown_keys() {
        crate::initialize_python_for_tests();
        Python::attach(|py| {
            let kwargs = PyDict::new(py);
            kwargs.set_item("cwd", "/private/tmp").expect("cwd keyword");
            let env = PyDict::new(py);
            env.set_item("RSLOOP_TEST_ENV", "present")
                .expect("environment entry");
            kwargs.set_item("env", env).expect("env keyword");
            let mut command = Command::new("program");

            apply_common_process_kwargs(py, &mut command, Some(&kwargs))
                .expect("apply common kwargs");

            assert_eq!(
                command.get_current_dir(),
                Some(std::path::Path::new("/private/tmp"))
            );
            assert!(command.get_envs().any(|(key, value)| {
                key == OsStr::new("RSLOOP_TEST_ENV") && value == Some(OsStr::new("present"))
            }));

            let unknown = PyDict::new(py);
            unknown
                .set_item("definitely_unknown", true)
                .expect("unknown keyword");
            let err = apply_common_process_kwargs(py, &mut Command::new("program"), Some(&unknown))
                .err()
                .expect("unknown keyword should not be silently ignored");
            assert!(err.is_instance_of::<PyTypeError>(py));
            assert!(err.to_string().contains("definitely_unknown"));
        });
    }

    /// Regression test for #68: `AnyIO` forwards the whole `Popen` keyword set
    /// to `loop.subprocess_exec()`, so each keyword has to be accepted at
    /// the default value a caller gets when it says nothing.
    #[test]
    fn defaulted_popen_keywords_are_all_accepted() {
        crate::initialize_python_for_tests();
        Python::attach(|py| {
            let kwargs = PyDict::new(py);
            kwargs.set_item("close_fds", true).expect("close_fds");
            kwargs.set_item("restore_signals", true).expect("restore");
            kwargs
                .set_item("start_new_session", false)
                .expect("session");
            for (key, value) in [("creationflags", 0i64), ("pipesize", -1), ("umask", -1)] {
                kwargs.set_item(key, value).expect("keyword");
            }
            for key in [
                "cwd",
                "env",
                "executable",
                "extra_groups",
                "group",
                "preexec_fn",
                "process_group",
                "startupinfo",
                "user",
            ] {
                kwargs.set_item(key, py.None()).expect("keyword");
            }
            kwargs
                .set_item("pass_fds", PyTuple::empty(py))
                .expect("pass_fds keyword");

            apply_common_process_kwargs(py, &mut Command::new("program"), Some(&kwargs))
                .expect("defaulted Popen keywords must be accepted");
        });
    }

    #[test]
    fn windows_only_keywords_are_refused_when_set() {
        crate::initialize_python_for_tests();
        Python::attach(|py| {
            for key in ["startupinfo", "creationflags"] {
                let kwargs = PyDict::new(py);
                if key == "creationflags" {
                    kwargs.set_item(key, 8i64).expect("creationflags");
                } else {
                    // Any non-None object stands in for a STARTUPINFO here.
                    kwargs.set_item(key, PyDict::new(py)).expect("startupinfo");
                }
                let result =
                    apply_common_process_kwargs(py, &mut Command::new("program"), Some(&kwargs));

                #[cfg(windows)]
                if key == "creationflags" {
                    // std can forward creation flags to CreateProcess.
                    result.expect("creationflags applies on Windows");
                    continue;
                }

                let err = result.err().expect("a set Windows-only keyword must fail");
                #[cfg(windows)]
                assert!(err.is_instance_of::<PyNotImplementedError>(py));
                #[cfg(not(windows))]
                {
                    assert!(err.is_instance_of::<PyValueError>(py));
                    assert!(
                        err.to_string()
                            .contains("only supported on Windows platforms")
                    );
                }
            }
        });
    }

    #[test]
    fn close_fds_false_is_refused_rather_than_ignored() {
        crate::initialize_python_for_tests();
        Python::attach(|py| {
            let kwargs = PyDict::new(py);
            kwargs.set_item("close_fds", false).expect("close_fds");
            let err = apply_common_process_kwargs(py, &mut Command::new("program"), Some(&kwargs))
                .err()
                .expect("close_fds=False cannot be honoured");
            assert!(err.is_instance_of::<PyValueError>(py));
            assert!(err.to_string().contains("pass_fds"));
        });
    }

    #[cfg(unix)]
    #[test]
    fn unix_kwargs_parse_flags_fds_identity_and_umask() {
        crate::initialize_python_for_tests();
        Python::attach(|py| {
            let mut config = UnixPreExecConfig::default();
            assert!(
                apply_unix_process_kw(
                    py,
                    &mut config,
                    "restore_signals",
                    &false.into_pyobject(py).expect("bool")
                )
                .expect("restore_signals")
            );
            assert!(
                apply_unix_process_kw(
                    py,
                    &mut config,
                    "start_new_session",
                    &true.into_pyobject(py).expect("bool")
                )
                .expect("start_new_session")
            );
            let fds = PyTuple::new(py, [3, 8]).expect("pass_fds");
            apply_unix_process_kw(py, &mut config, "pass_fds", fds.as_any()).expect("pass_fds");
            apply_unix_process_kw(
                py,
                &mut config,
                "process_group",
                17_i32.into_pyobject(py).expect("process group").as_any(),
            )
            .expect("process_group");
            apply_unix_process_kw(
                py,
                &mut config,
                "umask",
                0o27_i32.into_pyobject(py).expect("umask").as_any(),
            )
            .expect("umask");
            apply_unix_process_kw(
                py,
                &mut config,
                "user",
                123_u32.into_pyobject(py).expect("uid").as_any(),
            )
            .expect("user");
            apply_unix_process_kw(
                py,
                &mut config,
                "group",
                456_u32.into_pyobject(py).expect("gid").as_any(),
            )
            .expect("group");

            assert!(!config.restore_signals);
            assert!(config.start_new_session);
            assert_eq!(config.pass_fds, [3, 8]);
            assert_eq!(config.process_group, Some(17));
            assert_eq!(config.umask, Some(0o27));
            assert_eq!(config.uid, Some(123));
            assert_eq!(config.gid, Some(456));

            let invalid = 0o1000_i32.into_pyobject(py).expect("invalid umask");
            let err = apply_unix_process_kw(py, &mut config, "umask", invalid.as_any())
                .expect_err("out-of-range umask should fail");
            assert!(err.is_instance_of::<PyValueError>(py));

            let preexec = py
                .eval(pyo3::ffi::c_str!("lambda: None"), None, None)
                .expect("preexec function");
            let err = apply_unix_process_kw(py, &mut config, "preexec_fn", &preexec)
                .expect_err("preexec_fn should remain unsupported");
            assert!(err.is_instance_of::<PyNotImplementedError>(py));
        });
    }
}
