# Rust Extensions

This page explains how to add your own async Rust code and use it alongside
`rsloop` in Python.

The idea is:

- Python still uses `rsloop` as the event loop
- your own PyO3 extension module exposes extra functions
- those functions return Python awaitables backed by Rust futures

`rsloop` provides a small Rust helper module for this:

- `rsloop::rust_async::get_current_locals(...)`
- `rsloop::rust_async::future_into_py(...)`
- `rsloop::rust_async::future_into_py_with_locals(...)`
- `rsloop::rust_async::local_future_into_py(...)`
- `rsloop::rust_async::local_future_into_py_with_locals(...)`
- `rsloop::rust_async::TaskLocals`
- `rsloop::rust_async::into_future_with_locals(...)`

## When to use this

Use this pattern when:

- you want to keep the main application in Python
- you want some operations implemented in Rust
- those Rust operations should be `await`-able from Python
- you want them to run under the same active `rsloop` loop

This is not for modifying `rsloop` itself. It is for building a separate Rust
extension crate that depends on `rsloop`.

## What the developer builds

You usually create a second crate with:

1. a `Cargo.toml` for your PyO3 extension
2. a `pyproject.toml` for `maturin`
3. Rust functions marked with `#[pyfunction]`
4. a `#[pymodule]` that exports those functions

The important part is that each async Rust function should return a Python
awaitable using `rsloop::rust_async::future_into_py(...)`.

## Minimal Rust example

```rust
use std::time::Duration;

use pyo3::prelude::*;

#[pyfunction]
fn sleep_and_tag(py: Python<'_>, label: String, delay_ms: u64) -> PyResult<Bound<'_, PyAny>> {
    rsloop::rust_async::future_into_py(py, async move {
        async_std::task::sleep(Duration::from_millis(delay_ms)).await;
        Ok(format!("rust finished: {label}"))
    })
}

#[pymodule(gil_used = true)]
fn my_rust_ext(m: &Bound<'_, PyModule>) -> PyResult<()> {
    m.add_function(wrap_pyfunction!(sleep_and_tag, m)?)?;
    Ok(())
}
```

In this example:

- the Python caller gets an awaitable back immediately
- the real work happens in the Rust future
- the future is attached to the currently running Python event loop

## Free-threaded interpreters

`gil_used = true` is the conservative default for a new extension, and it is
what the example above uses. Be aware of what it costs on a free-threaded
interpreter: importing *any* module that declares it makes CPython re-enable
the GIL for the whole process, which undoes rsloop's own free-threading support
for every loop in that process.

Once your extension's own state is safe under concurrent access — no raw
pointers into Python-visible mutable buffers held across calls, shared Rust
state behind mutexes or atomics, module-level caches in `PyOnceLock` rather
than plain `static mut` — switch it to `gil_used = false`. rsloop itself makes
that declaration, so a mixed process is only as free-threaded as its most
conservative extension.

## Python side usage

From Python, the Rust function looks like a normal async operation:

```python
import asyncio
import rsloop
import my_rust_ext


async def main() -> None:
    loop = asyncio.get_running_loop()
    print(type(loop).__name__)

    result = await my_rust_ext.sleep_and_tag("hello from python", 50)
    print(result)


rsloop.run(main())
```

This works because `rsloop.run(...)` creates the running loop first, and then
the Rust extension captures that loop when `future_into_py(...)` is called.

## Project setup

Your extension crate should depend on:

```toml
[dependencies]
async-std = "1"
pyo3 = "0.29.2"
rsloop = { version = "0.1.52" }
```

Keep your PyO3 version aligned with the version used by `rsloop`, because the
interop helpers expose PyO3 types in their public signatures. When developing
against a local checkout, replace the version dependency with a path dependency,
as the bundled example does:

```toml
rsloop = { path = "../path/to/rsloop" }
```

Your `pyproject.toml` should use `maturin` in the normal PyO3 way:

```toml
[build-system]
requires = ["maturin>=1.9.4,<2"]
build-backend = "maturin"

[tool.maturin]
module-name = "my_rust_ext"
```

## How the bridge works

`future_into_py(...)` does two important things:

1. It captures the current Python event loop and contextvars.
2. It converts the Rust future into a Python awaitable.

That means your Rust future can be awaited from Python code that is already
running on `rsloop`.

If you need lower-level control, use `get_current_locals(...)` and
`future_into_py_with_locals(...)` directly. That is useful when you capture the
loop/context once and reuse it across multiple Rust operations.

## Calling Python awaitables from Rust

`rsloop` also re-exports `into_future_with_locals(...)`.

Use that when your Rust future needs to await a Python awaitable:

```rust
use pyo3::prelude::*;

fn call_python_awaitable(
    py: Python<'_>,
    awaitable: Py<PyAny>,
) -> PyResult<Bound<'_, PyAny>> {
    let locals = rsloop::rust_async::get_current_locals(py)?;
    rsloop::rust_async::future_into_py_with_locals(py, locals.clone(), async move {
        let python_future = Python::attach(|py| {
            rsloop::rust_async::into_future_with_locals(&locals, awaitable.bind(py).clone())
        })?;
        let value = python_future.await?;
        Ok(value)
    })
}
```

That pattern is more advanced, but it is useful when Rust is coordinating both
Rust futures and Python coroutines.

## Full example in this repository

The repository includes a complete example in:

- `examples/rust/src/lib.rs`
- `examples/rust/demo.py`
- `examples/rust/README.md`

If you want the shortest end-to-end check from the repository root, run:

```bash
uv run --with . --with ./examples/rust python examples/rust/demo.py
```

## Practical advice

- Keep the Rust extension separate from the `rsloop` crate itself.
- Start with small `#[pyfunction]` wrappers that return one awaitable each.
- Prefer returning normal Python-friendly values such as strings, integers, dictionaries, and lists.
- Use `future_into_py(...)` first. Drop to the `*_with_locals(...)` variants only when you actually need them.
- Make sure your Rust function is called while a Python event loop is already running.

If you call the helper outside a running event loop, capturing the current loop
will fail, because there is no active Python loop to attach to.
