//! Explicit profiler lifetime for Python-driven workloads.

use std::sync::Mutex;

use hotpath::{Format, HotpathGuard, HotpathGuardBuilder};
use pyo3::{
    exceptions::{PyRuntimeError, PyValueError},
    prelude::*,
};

// hotpath has process-global registries. A fresh process per session keeps
// function, future, and thread statistics from leaking between workloads.
#[allow(
    clippy::large_enum_variant,
    reason = "A single static session stores the guard inline"
)]
enum Session {
    New,
    Active(HotpathGuard),
    Finished,
}
static PROFILE: Mutex<Session> = Mutex::new(Session::New);

#[cfg(feature = "hotpath-alloc-profile")]
#[global_allocator]
static ALLOCATOR: hotpath::CountingAllocator<std::alloc::System> =
    hotpath::CountingAllocator::new();

#[pyfunction]
#[pyo3(signature = (output_path, *, format="json", functions_limit=0, time_sampling_rate=1.0))]
pub(crate) fn hotpath_start(
    output_path: String,
    format: &str,
    functions_limit: usize,
    time_sampling_rate: f64,
) -> PyResult<()> {
    let format = match format {
        "json" => Format::Json,
        "json-pretty" => Format::JsonPretty,
        "table" => Format::Table,
        _ => {
            return Err(PyValueError::new_err(
                "format must be json, json-pretty, or table",
            ));
        }
    };
    if !time_sampling_rate.is_finite()
        || !(0.0..=1.0).contains(&time_sampling_rate)
        || time_sampling_rate == 0.0
    {
        return Err(PyValueError::new_err(
            "time_sampling_rate must be in (0, 1]",
        ));
    }
    let mut profile = PROFILE.lock().expect("poisoned hotpath profile");
    match &*profile {
        Session::New => {}
        Session::Active(_) => {
            return Err(PyRuntimeError::new_err(
                "hotpath profiling is already active",
            ));
        }
        Session::Finished => {
            return Err(PyRuntimeError::new_err(
                "start a fresh process for each hotpath profile",
            ));
        }
    }
    *profile = Session::Active(
        HotpathGuardBuilder::new("rsloop")
            .limit(0)
            .functions_limit(functions_limit)
            .percentiles(&[50.0, 95.0, 99.0])
            .time_sampling_rate(time_sampling_rate)
            .format(format)
            .output_path(output_path)
            .build(),
    );
    Ok(())
}

#[pyfunction]
pub(crate) fn hotpath_stop(py: Python<'_>) -> PyResult<()> {
    let mut profile = PROFILE.lock().expect("poisoned hotpath profile");
    if !matches!(*profile, Session::Active(_)) {
        return Err(PyRuntimeError::new_err("hotpath profiling is not active"));
    }
    let Session::Active(guard) = std::mem::replace(&mut *profile, Session::Finished) else {
        unreachable!()
    };
    drop(profile);
    // Report flushing can block; let other Python-attached workers finish.
    py.detach(|| drop(guard));
    Ok(())
}
