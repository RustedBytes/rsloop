#[cfg(feature = "profiler")]
mod imp {
    use std::fs::File;
    use std::path::PathBuf;
    use std::sync::{Mutex, OnceLock};

    use pprof::ProfilerGuard;
    use pyo3::exceptions::{PyOSError, PyRuntimeError, PyValueError};
    use pyo3::prelude::*;

    struct ActiveProfiler {
        guard: ProfilerGuard<'static>,
    }

    static ACTIVE_PROFILER: OnceLock<Mutex<Option<ActiveProfiler>>> = OnceLock::new();

    fn active_profiler() -> &'static Mutex<Option<ActiveProfiler>> {
        ACTIVE_PROFILER.get_or_init(|| Mutex::new(None))
    }

    fn build_guard(frequency: i32) -> Result<ProfilerGuard<'static>, pprof::Error> {
        let builder = pprof::ProfilerGuardBuilder::default().frequency(frequency);
        #[cfg(any(
            target_arch = "x86_64",
            target_arch = "aarch64",
            target_arch = "riscv64",
            target_arch = "loongarch64"
        ))]
        let builder = builder.blocklist(&["libc", "libgcc", "pthread", "vdso"]);
        builder.build()
    }

    fn map_io_error(err: std::io::Error) -> PyErr {
        PyOSError::new_err(err.to_string())
    }

    fn map_profiler_error(err: pprof::Error) -> PyErr {
        PyRuntimeError::new_err(err.to_string())
    }

    fn ensure_supported_format(format: &str) -> PyResult<()> {
        match format {
            "flamegraph" => Ok(()),
            other => Err(PyValueError::new_err(format!(
                "unsupported profiler output format: {other}"
            ))),
        }
    }

    #[pyfunction(signature = (*, frequency = 999))]
    pub fn start_profiler(frequency: i32) -> PyResult<()> {
        if frequency <= 0 {
            return Err(PyValueError::new_err(
                "profiler frequency must be greater than zero",
            ));
        }

        let mut active = active_profiler()
            .lock()
            .map_err(|_| PyRuntimeError::new_err("profiler state mutex is poisoned"))?;
        if active.is_some() {
            return Err(PyRuntimeError::new_err("profiler is already running"));
        }

        let guard = build_guard(frequency).map_err(map_profiler_error)?;
        *active = Some(ActiveProfiler { guard });
        Ok(())
    }

    #[pyfunction]
    pub fn profiler_running() -> bool {
        active_profiler()
            .lock()
            .map(|active| active.is_some())
            .unwrap_or(false)
    }

    #[pyfunction(signature = (path, *, format = "flamegraph"))]
    pub fn stop_profiler(path: String, format: &str) -> PyResult<String> {
        ensure_supported_format(format)?;

        let active = active_profiler()
            .lock()
            .map_err(|_| PyRuntimeError::new_err("profiler state mutex is poisoned"))?
            .take()
            .ok_or_else(|| PyRuntimeError::new_err("profiler is not running"))?;

        let report = active.guard.report().build().map_err(map_profiler_error)?;
        let path = PathBuf::from(path);
        if let Some(parent) = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
        {
            std::fs::create_dir_all(parent).map_err(map_io_error)?;
        }

        match format {
            "flamegraph" => {
                let file = File::create(&path).map_err(map_io_error)?;
                report.flamegraph(file).map_err(map_profiler_error)?;
            }
            _ => unreachable!("validated profiler output format"),
        }

        Ok(path.display().to_string())
    }
}

#[cfg(not(feature = "profiler"))]
mod imp {
    use pyo3::exceptions::PyRuntimeError;
    use pyo3::prelude::*;

    const PROFILER_DISABLED_MESSAGE: &str =
        "profiler support is disabled; rebuild with `--features profiler`";

    #[pyfunction]
    pub fn profiler_running() -> bool {
        false
    }

    #[pyfunction(signature = (*, frequency = 999))]
    pub fn start_profiler(frequency: i32) -> PyResult<()> {
        let _ = frequency;
        Err(PyRuntimeError::new_err(PROFILER_DISABLED_MESSAGE))
    }

    #[pyfunction(signature = (path, *, format = "flamegraph"))]
    pub fn stop_profiler(path: String, format: &str) -> PyResult<String> {
        let _ = (path, format);
        Err(PyRuntimeError::new_err(PROFILER_DISABLED_MESSAGE))
    }
}

pub use imp::{profiler_running, start_profiler, stop_profiler};
