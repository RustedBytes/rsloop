//! Opt-in transport counters exposed to Python.
//!
//! The counters are always present but only written when
//! `RSLOOP_TRANSPORT_STATS` is set, so the read and write hot paths pay a
//! single cached bool check rather than an atomic increment per event.

use std::sync::atomic::{AtomicU64, Ordering};

use pyo3::{prelude::*, types::PyDict};

pub(super) static TRANSPORT_READ_EVENTS: AtomicU64 = AtomicU64::new(0);
pub(super) static TRANSPORT_READ_BYTES: AtomicU64 = AtomicU64::new(0);
pub(super) static TRANSPORT_READ_WAKEUPS: AtomicU64 = AtomicU64::new(0);
pub(super) static TRANSPORT_PYTHON_READ_DRAINS: AtomicU64 = AtomicU64::new(0);
pub(super) static TRANSPORT_STAGED_WRITES: AtomicU64 = AtomicU64::new(0);
pub(super) static TRANSPORT_DIRECT_WRITE_ATTEMPTS: AtomicU64 = AtomicU64::new(0);
pub(super) static TRANSPORT_POLL_REBINDS: AtomicU64 = AtomicU64::new(0);

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
pub(super) fn transport_stats_enabled() -> bool {
    static ENABLED: std::sync::OnceLock<bool> = std::sync::OnceLock::new();
    *ENABLED.get_or_init(|| {
        std::env::var("RSLOOP_TRANSPORT_STATS")
            .ok()
            .is_some_and(|value| matches!(value.trim(), "1" | "true" | "yes" | "on"))
    })
}

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
#[pyfunction]
pub fn transport_stats(py: Python<'_>) -> PyResult<Py<PyDict>> {
    let stats = PyDict::new(py);
    stats.set_item("enabled", transport_stats_enabled())?;
    stats.set_item("read_events", TRANSPORT_READ_EVENTS.load(Ordering::Relaxed))?;
    stats.set_item("read_bytes", TRANSPORT_READ_BYTES.load(Ordering::Relaxed))?;
    stats.set_item(
        "read_wakeups",
        TRANSPORT_READ_WAKEUPS.load(Ordering::Relaxed),
    )?;
    stats.set_item(
        "python_read_drains",
        TRANSPORT_PYTHON_READ_DRAINS.load(Ordering::Relaxed),
    )?;
    stats.set_item(
        "staged_writes",
        TRANSPORT_STAGED_WRITES.load(Ordering::Relaxed),
    )?;
    stats.set_item(
        "direct_write_attempts",
        TRANSPORT_DIRECT_WRITE_ATTEMPTS.load(Ordering::Relaxed),
    )?;
    stats.set_item(
        "poll_rebinds",
        TRANSPORT_POLL_REBINDS.load(Ordering::Relaxed),
    )?;
    Ok(stats.unbind())
}

#[cfg_attr(feature = "hotpath-profile", hotpath::measure)]
#[pyfunction]
pub fn reset_transport_stats() {
    TRANSPORT_READ_EVENTS.store(0, Ordering::Relaxed);
    TRANSPORT_READ_BYTES.store(0, Ordering::Relaxed);
    TRANSPORT_READ_WAKEUPS.store(0, Ordering::Relaxed);
    TRANSPORT_PYTHON_READ_DRAINS.store(0, Ordering::Relaxed);
    TRANSPORT_STAGED_WRITES.store(0, Ordering::Relaxed);
    TRANSPORT_DIRECT_WRITE_ATTEMPTS.store(0, Ordering::Relaxed);
    TRANSPORT_POLL_REBINDS.store(0, Ordering::Relaxed);
}
