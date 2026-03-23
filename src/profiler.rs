#[cfg(feature = "profiler")]
mod imp {
    use std::cell::RefCell;
    use std::sync::{Mutex, OnceLock};

    use pyo3::exceptions::{PyRuntimeError, PyValueError};
    use pyo3::prelude::*;
    use tracy_client::{Client, Span};

    struct ActiveProfiler {
        client: Client,
    }

    thread_local! {
        static SESSION_SPAN: RefCell<Option<Span>> = const { RefCell::new(None) };
    }

    static ACTIVE_PROFILER: OnceLock<Mutex<Option<ActiveProfiler>>> = OnceLock::new();

    fn active_profiler() -> &'static Mutex<Option<ActiveProfiler>> {
        ACTIVE_PROFILER.get_or_init(|| Mutex::new(None))
    }

    #[pyfunction]
    pub fn start_profiler() -> PyResult<()> {
        let mut active = active_profiler()
            .lock()
            .map_err(|_| PyRuntimeError::new_err("profiler state mutex is poisoned"))?;
        if active.is_some() {
            return Err(PyRuntimeError::new_err("profiler is already running"));
        }

        let client = Client::start();
        client.set_thread_name("python-main");
        let session_span = client.clone().span_alloc(
            Some("rsloop.profile_session"),
            "start_profiler",
            file!(),
            line!(),
            0,
        );
        SESSION_SPAN.with(|slot| {
            *slot.borrow_mut() = Some(session_span);
        });
        *active = Some(ActiveProfiler { client });
        Ok(())
    }

    #[pyfunction]
    pub fn profiler_running() -> bool {
        active_profiler()
            .lock()
            .map(|active| active.is_some())
            .unwrap_or(false)
    }

    #[pyfunction]
    pub fn stop_profiler() -> PyResult<()> {
        let active = active_profiler()
            .lock()
            .map_err(|_| PyRuntimeError::new_err("profiler state mutex is poisoned"))?
            .take()
            .ok_or_else(|| PyRuntimeError::new_err("profiler is not running"))?;
        SESSION_SPAN.with(|slot| {
            slot.borrow_mut().take();
        });
        drop(active.client);
        Ok(())
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

    #[pyfunction]
    pub fn start_profiler() -> PyResult<()> {
        Err(PyRuntimeError::new_err(PROFILER_DISABLED_MESSAGE))
    }

    #[pyfunction]
    pub fn stop_profiler() -> PyResult<()> {
        Err(PyRuntimeError::new_err(PROFILER_DISABLED_MESSAGE))
    }
}

pub use imp::{profiler_running, start_profiler, stop_profiler};
