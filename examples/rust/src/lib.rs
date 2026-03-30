use std::time::Duration;

use pyo3::exceptions::PyValueError;
use pyo3::prelude::*;

#[pyfunction]
fn sleep_and_tag(py: Python<'_>, label: String, delay_ms: u64) -> PyResult<Bound<'_, PyAny>> {
    rsloop::rust_async::future_into_py(py, async move {
        async_std::task::sleep(Duration::from_millis(delay_ms)).await;
        Ok(format!("rust finished: {label}"))
    })
}

#[pyfunction]
fn race_sum(py: Python<'_>, values: Vec<u64>) -> PyResult<Bound<'_, PyAny>> {
    if values.is_empty() {
        return Err(PyValueError::new_err("values must not be empty"));
    }

    rsloop::rust_async::future_into_py(py, async move {
        let mut tasks = Vec::with_capacity(values.len());
        for value in values {
            tasks.push(async_std::task::spawn(async move {
                async_std::task::sleep(Duration::from_millis(value * 10)).await;
                value
            }));
        }

        let mut total = 0_u64;
        for task in tasks {
            total += task.await;
        }
        Ok(total)
    })
}

#[pymodule(gil_used = false)]
fn rsloop_rust_example(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(sleep_and_tag, m)?)?;
    m.add_function(wrap_pyfunction!(race_sum, m)?)?;
    Ok(())
}
