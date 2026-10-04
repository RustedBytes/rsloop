//! Explicit profiler lifetime for Python-driven workloads.

use std::sync::Mutex;

use hotpath::{HotpathGuard, HotpathGuardBuilder};
use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;

static PROFILE: Mutex<Option<HotpathGuard>> = Mutex::new(None);

#[pyfunction]
pub(crate) fn hotpath_start(output_path: String) -> PyResult<()> {
    let mut profile = PROFILE.lock().expect("poisoned hotpath profile");
    if profile.is_some() {
        return Err(PyRuntimeError::new_err(
            "hotpath profiling is already active",
        ));
    }
    *profile = Some(
        HotpathGuardBuilder::new("rsloop")
            .functions_limit(50)
            .output_path(output_path)
            .build(),
    );
    Ok(())
}

#[pyfunction]
pub(crate) fn hotpath_stop() -> PyResult<()> {
    let guard = PROFILE.lock().expect("poisoned hotpath profile").take();
    let Some(guard) = guard else {
        return Err(PyRuntimeError::new_err("hotpath profiling is not active"));
    };
    drop(guard);
    Ok(())
}
