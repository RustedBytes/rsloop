use std::{
    panic::AssertUnwindSafe,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use pyo3::types::PyDict;

use super::*;

static CANCELLED: AtomicBool = AtomicBool::new(false);
struct DropProbe;
impl Drop for DropProbe {
    fn drop(&mut self) {
        CANCELLED.store(true, Ordering::SeqCst);
    }
}
#[pyfunction]
fn cancelled() -> bool {
    CANCELLED.load(Ordering::SeqCst)
}

#[pyfunction]
fn bridge<'py>(py: Python<'py>, mode: &str) -> PyResult<Bound<'py, PyAny>> {
    match mode {
        "cancel" | "local-cancel" => {
            CANCELLED.store(false, Ordering::SeqCst);
            let probe = DropProbe;
            if mode == "local-cancel" {
                let local = Rc::new(probe);
                local_future_into_py(py, async move {
                    let _keep = local;
                    std::future::pending::<()>().await;
                    Ok(0)
                })
            } else {
                future_into_py(py, async move {
                    let _keep = probe;
                    std::future::pending::<()>().await;
                    Ok(0)
                })
            }
        }
        "local-panic" => local_future_into_py(py, async {
            panic!("bridge panic probe");
            #[allow(unreachable_code)]
            Ok(0)
        }),
        "panic" => future_into_py(py, async {
            panic!("bridge panic probe");
            #[allow(unreachable_code)]
            Ok(0)
        }),
        "local" => {
            let value = Rc::new(42);
            let thread = std::thread::current().id();
            local_future_into_py(py, async move {
                smol::Timer::after(Duration::from_millis(2)).await;
                assert_eq!(thread, std::thread::current().id());
                Ok(*value)
            })
        }
        "context" => future_into_py(py, async {
            smol::Timer::after(Duration::from_millis(2)).await;
            let locals = Python::attach(get_current_locals)?;
            Python::attach(|py| {
                let context = locals.context(py);
                context.call_method0("copy")?;
                Ok(py
                    .import("builtins")?
                    .getattr("list")?
                    .call1((context.call_method0("items")?,))?
                    .repr()?
                    .to_string())
            })
        }),
        _ => future_into_py(py, async {
            smol::Timer::after(Duration::from_millis(2)).await;
            Ok(42)
        }),
    }
}

#[test]
fn python_bridge_preserves_results_panics_locals_and_local_thread() {
    crate::initialize_python_for_tests();
    Python::attach(|py| {
        let module = PyModule::new(py, "bridge_test").unwrap();
        module
            .add_function(wrap_pyfunction!(bridge, &module).unwrap())
            .unwrap();
        module
            .add_function(wrap_pyfunction!(cancelled, &module).unwrap())
            .unwrap();
        let globals = PyDict::new(py);
        globals
            .set_item("cancelled", module.getattr("cancelled").unwrap())
            .unwrap();
        globals
            .set_item("bridge", module.getattr("bridge").unwrap())
            .unwrap();
        py.run(
            c"
import asyncio, contextvars
probe = contextvars.ContextVar('bridge_probe')
async def task(value):
    probe.set(value)
    result = await bridge('context')
    assert repr(value) in result, result
async def main():
    assert await bridge('send') == 42
    assert await bridge('local') == 42
    await asyncio.gather(task('first-context'), task('second-context'))
    for mode in ('cancel', 'local-cancel'):
        fut = bridge(mode)
        await asyncio.sleep(0.01)
        fut.cancel()
        await asyncio.sleep(0.01)
        assert cancelled(), mode
    for mode in ('panic', 'local-panic'):
        try:
            await bridge(mode)
        except BaseException as exc:
            assert type(exc).__name__ == 'RustPanic', type(exc).__name__
        else:
            raise AssertionError('panic was not propagated')
for _ in range(3):
    asyncio.run(main())
",
            Some(&globals),
            None,
        )
        .unwrap();
    });
}

#[test]
fn discarded_bridge_handles_keep_running() {
    use pyo3_async_runtimes::generic::Runtime;
    let (tx, rx) = futures::channel::oneshot::channel();
    drop(SmolRuntime::spawn(async move {
        let _ = tx.send(42);
    }));
    assert_eq!(smol::block_on(rx).unwrap(), 42);
    let done = Arc::new(AtomicBool::new(false));
    let done_worker = done.clone();
    drop(SmolRuntime::spawn_blocking(move || {
        done_worker.store(true, Ordering::SeqCst);
    }));
    smol::block_on(async {
        timeout(Duration::from_secs(5), async {
            while !done.load(Ordering::SeqCst) {
                smol::Timer::after(Duration::from_millis(1)).await;
            }
        })
        .await
        .unwrap();
    });
}

#[test]
fn timer_timeout_handles_both_outcomes() {
    smol::block_on(async {
        assert_eq!(timeout(Duration::from_secs(1), async { 42 }).await, Ok(42));
        assert!(
            timeout(Duration::from_millis(1), std::future::pending::<()>())
                .await
                .is_err()
        );
    });
}

#[test]
fn explicit_local_bridge_works_before_loop_starts() {
    crate::initialize_python_for_tests();
    Python::attach(|py| {
        let event_loop = py
            .import("asyncio")
            .unwrap()
            .call_method0("new_event_loop")
            .unwrap();
        let locals = TaskLocals::new(event_loop.clone())
            .copy_context(py)
            .unwrap();
        let value = Rc::new(42);
        let awaitable = local_future_into_py_with_locals(py, locals, async move {
            smol::Timer::after(Duration::from_millis(1)).await;
            Ok(*value)
        })
        .unwrap();
        let result = event_loop
            .call_method1("run_until_complete", (awaitable,))
            .unwrap();
        assert_eq!(result.extract::<usize>().unwrap(), 42);
        event_loop.call_method0("close").unwrap();
    });
}

#[test]
fn task_context_restores_after_unwind() {
    use pyo3_async_runtimes::generic::ContextExt;
    crate::initialize_python_for_tests();
    let locals = Python::attach(|py| {
        TaskLocals::new(
            py.import("asyncio")
                .unwrap()
                .call_method0("new_event_loop")
                .unwrap(),
        )
    });
    assert!(SmolRuntime::get_task_locals().is_none());
    let result = std::panic::catch_unwind(AssertUnwindSafe(|| {
        smol::block_on(SmolRuntime::scope(locals.clone(), async {
            assert!(SmolRuntime::get_task_locals().is_some());
            panic!("context unwind probe");
        }));
    }));
    assert!(result.is_err());
    assert!(SmolRuntime::get_task_locals().is_none());
    Python::attach(|py| {
        locals.event_loop(py).call_method0("close").unwrap();
    });
}
