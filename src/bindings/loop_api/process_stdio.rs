//! Translation of `asyncio`'s `stdin`/`stdout`/`stderr` arguments into `Stdio`.
//!
//! Callers pass either one of `asyncio.subprocess`'s marker constants
//! (`PIPE`/`DEVNULL`/`STDOUT`) or something with a `fileno()`. `STDOUT` is only
//! legal for `stderr`, and the `stdout=PIPE, stderr=STDOUT` combination cannot
//! be expressed with `std::process::Stdio` alone — it needs one pipe handed to
//! both child descriptors, which is why that case builds the pipe itself.

use std::process::Command;

use pyo3::{
    exceptions::{PyRuntimeError, PyValueError},
    prelude::*,
    sync::PyOnceLock,
};

use super::process_handles;
use crate::{fd_ops, transport::process::BoxedProcessReader};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ProcessStdioSpec {
    Inherit,
    Pipe,
    DevNull,
    Fd(fd_ops::RawFd),
    Stdout,
}

/// The three resolved child descriptors for one spawn.
#[derive(Clone, Copy)]
pub(super) struct ProcessStdioSpecs {
    pub(super) stdin: ProcessStdioSpec,
    pub(super) stdout: ProcessStdioSpec,
    pub(super) stderr: ProcessStdioSpec,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProcessStdioPlan {
    Standard,
    SharedOutputPipe,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum ProcessStdioPlanError {
    StdinRedirect,
    StdoutRedirect,
}

#[cfg_attr(feature = "profile", hotpath::measure)]
fn process_stdio_plan(
    stdin: ProcessStdioSpec,
    stdout: ProcessStdioSpec,
    stderr: ProcessStdioSpec,
) -> Result<ProcessStdioPlan, ProcessStdioPlanError> {
    if matches!(stdin, ProcessStdioSpec::Stdout) {
        return Err(ProcessStdioPlanError::StdinRedirect);
    }
    if matches!(stdout, ProcessStdioSpec::Stdout) {
        return Err(ProcessStdioPlanError::StdoutRedirect);
    }
    if matches!(
        (stdout, stderr),
        (ProcessStdioSpec::Pipe, ProcessStdioSpec::Stdout)
    ) {
        Ok(ProcessStdioPlan::SharedOutputPipe)
    } else {
        Ok(ProcessStdioPlan::Standard)
    }
}

impl ProcessStdioSpecs {
    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "ProcessStdioSpecs")
    )]
    pub(super) fn parse(
        py: Python<'_>,
        stdin: &Py<PyAny>,
        stdout: &Py<PyAny>,
        stderr: &Py<PyAny>,
    ) -> PyResult<Self> {
        Ok(Self {
            stdin: parse_process_stdio(py, stdin, false)?,
            stdout: parse_process_stdio(py, stdout, false)?,
            stderr: parse_process_stdio(py, stderr, true)?,
        })
    }
}

static PIPE_CELL: PyOnceLock<Py<PyAny>> = PyOnceLock::new();

/// Default for an omitted `stdin`/`stdout`/`stderr`: `subprocess.PIPE` (== -1),
/// matching `CPython`'s loop methods. An explicit `None` arrives as
/// `Option::None` instead and is honored as "inherit the parent's fd" by
/// `parse_process_stdio`.
#[cfg_attr(feature = "profile", hotpath::measure)]
pub(super) fn default_stdio_pipe() -> Py<PyAny> {
    Python::attach(|py| {
        PIPE_CELL
            .get_or_try_init(py, || {
                py.import("asyncio.subprocess")?
                    .getattr("PIPE")
                    .map(Bound::unbind)
            })
            .expect("asyncio.subprocess is always importable")
            .clone_ref(py)
    })
}

#[cfg_attr(feature = "profile", hotpath::measure)]
fn parse_process_stdio(
    py: Python<'_>,
    value: &Py<PyAny>,
    allow_stdout_redirect: bool,
) -> PyResult<ProcessStdioSpec> {
    let bound = value.bind(py);
    if bound.is_none() {
        return Ok(ProcessStdioSpec::Inherit);
    }
    parse_subprocess_stdio_marker(py, bound, allow_stdout_redirect)?.map_or_else(
        || Ok(ProcessStdioSpec::Fd(fd_ops::fileobj_to_fd(py, bound)?)),
        Ok,
    )
}

#[cfg_attr(feature = "profile", hotpath::measure)]
fn parse_subprocess_stdio_marker(
    py: Python<'_>,
    value: &Bound<'_, PyAny>,
    allow_stdout_redirect: bool,
) -> PyResult<Option<ProcessStdioSpec>> {
    let subprocess = py.import("asyncio.subprocess")?;
    if value.eq(default_stdio_pipe())? {
        return Ok(Some(ProcessStdioSpec::Pipe));
    }
    if value.eq(&subprocess.getattr("DEVNULL")?)? {
        return Ok(Some(ProcessStdioSpec::DevNull));
    }
    let is_stdout = value.eq(&subprocess.getattr("STDOUT")?)?;
    match (allow_stdout_redirect, is_stdout) {
        (true, true) => Ok(Some(ProcessStdioSpec::Stdout)),
        (false, true) => Err(PyValueError::new_err("STDOUT can only be used for stderr")),
        (_, false) => Ok(None),
    }
}

#[cfg_attr(feature = "profile", hotpath::measure)]
fn stdio_from_fd(fd: fd_ops::RawFd) -> PyResult<std::process::Stdio> {
    process_handles::file_from_fd(fd).map(std::process::Stdio::from)
}

#[cfg_attr(feature = "profile", hotpath::measure)]
pub(super) fn apply_stdio(
    command: &mut Command,
    specs: ProcessStdioSpecs,
) -> PyResult<(Option<BoxedProcessReader>, Option<BoxedProcessReader>)> {
    use std::process::Stdio;

    let ProcessStdioSpecs {
        stdin,
        stdout,
        stderr,
    } = specs;
    let plan = process_stdio_plan(stdin, stdout, stderr)
        .map_err(|_| PyValueError::new_err("STDOUT can only be used for stderr"))?;
    command.stdin(output_stdio(stdin)?);

    let mut stdout_override = None;
    let stderr_override = None;

    match plan {
        ProcessStdioPlan::SharedOutputPipe => {
            let (read_end, write_end) = process_handles::new_pipe()?;
            let stderr_end = write_end
                .try_clone()
                .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
            command.stdout(Stdio::from(write_end));
            command.stderr(Stdio::from(stderr_end));
            stdout_override = Some(Box::new(read_end) as BoxedProcessReader);
        }
        ProcessStdioPlan::Standard => {
            command.stdout(output_stdio(stdout)?);
            command.stderr(stderr_stdio(stderr, stdout)?);
        }
    }

    Ok((stdout_override, stderr_override))
}

#[cfg_attr(feature = "profile", hotpath::measure)]
fn output_stdio(spec: ProcessStdioSpec) -> PyResult<std::process::Stdio> {
    use std::process::Stdio;

    match spec {
        ProcessStdioSpec::Inherit => Ok(Stdio::inherit()),
        ProcessStdioSpec::Pipe => Ok(Stdio::piped()),
        ProcessStdioSpec::DevNull => Ok(Stdio::null()),
        ProcessStdioSpec::Fd(fd) => stdio_from_fd(fd),
        ProcessStdioSpec::Stdout => {
            Err(PyValueError::new_err("STDOUT can only be used for stderr"))
        }
    }
}

#[cfg_attr(feature = "profile", hotpath::measure)]
fn stderr_stdio(
    stderr: ProcessStdioSpec,
    stdout: ProcessStdioSpec,
) -> PyResult<std::process::Stdio> {
    if matches!(stderr, ProcessStdioSpec::Stdout) {
        return stderr_stdout_stdio(stdout);
    }
    output_stdio(stderr)
}

#[cfg_attr(feature = "profile", hotpath::measure)]
fn stderr_stdout_stdio(stdout: ProcessStdioSpec) -> PyResult<std::process::Stdio> {
    use std::process::Stdio;

    match stdout {
        ProcessStdioSpec::Inherit => Ok(Stdio::inherit()),
        ProcessStdioSpec::DevNull => Ok(Stdio::null()),
        ProcessStdioSpec::Fd(fd) => stdio_from_fd(fd),
        ProcessStdioSpec::Pipe | ProcessStdioSpec::Stdout => Err(PyRuntimeError::new_err(
            "invalid stderr=STDOUT configuration",
        )),
    }
}

#[cfg(kani)]
mod verification {
    use super::*;

    fn spec(tag: u8) -> ProcessStdioSpec {
        match tag {
            0 => ProcessStdioSpec::Inherit,
            1 => ProcessStdioSpec::Pipe,
            2 => ProcessStdioSpec::DevNull,
            3 => ProcessStdioSpec::Fd(17),
            4 => ProcessStdioSpec::Stdout,
            _ => unreachable!("tag is constrained to a stdio variant"),
        }
    }

    #[kani::proof]
    fn merge_process_stdio_routing_is_total_and_preserves_error_precedence() {
        let stdin_tag: u8 = kani::any();
        let stdout_tag: u8 = kani::any();
        let stderr_tag: u8 = kani::any();
        kani::assume(stdin_tag < 5);
        kani::assume(stdout_tag < 5);
        kani::assume(stderr_tag < 5);

        let stdin = spec(stdin_tag);
        let stdout = spec(stdout_tag);
        let stderr = spec(stderr_tag);
        let plan = process_stdio_plan(stdin, stdout, stderr);

        if stdin == ProcessStdioSpec::Stdout {
            assert_eq!(plan, Err(ProcessStdioPlanError::StdinRedirect));
        } else if stdout == ProcessStdioSpec::Stdout {
            assert_eq!(plan, Err(ProcessStdioPlanError::StdoutRedirect));
        } else if stdout == ProcessStdioSpec::Pipe && stderr == ProcessStdioSpec::Stdout {
            assert_eq!(plan, Ok(ProcessStdioPlan::SharedOutputPipe));
        } else {
            assert_eq!(plan, Ok(ProcessStdioPlan::Standard));
        }
    }
}
