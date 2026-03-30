# Rust Async Extension Example

This example shows how to keep `rsloop` as the Python event loop while adding
your own async Rust functions in a separate PyO3 extension module.

The extension uses `rsloop::rust_async::future_into_py(...)` to turn a Rust
future into a Python awaitable attached to the current running loop.

## Files

- `src/lib.rs`: example Rust extension module
- `demo.py`: Python code that awaits the Rust functions while running on `rsloop`
- `Cargo.toml` / `pyproject.toml`: local build config for `maturin`

## Run

From the folder, the simplest end-to-end run is:

```bash
uv run --with maturin maturin develop --release
uv run --with rsloop python demo.py
```

If you only want to check the Rust builds, use:

```bash
cargo check
cargo check --manifest-path examples/rust/Cargo.toml
```

In your own extension crate, the important pattern is:

```rust
#[pyfunction]
fn my_async_fn(py: Python<'_>) -> PyResult<Bound<'_, PyAny>> {
    rsloop::rust_async::future_into_py(py, async move {
        // your async Rust work here
        Ok("done")
    })
}
```
