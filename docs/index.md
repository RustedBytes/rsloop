# rsloop

`rsloop` is an `asyncio` event loop written in Rust and packaged for Python.

If you already know normal `asyncio`, the short version is:

- You still write Python coroutines, tasks, and protocols.
- You swap the event loop implementation to `rsloop`.
- Rust handles the lower-level loop and I/O work.

This documentation is intentionally small and practical. It is written for junior Python programmers who want to understand both how to use the project and how the repository is organized.

## What problem does this project solve?

Python's standard `asyncio` event loop is written in Python and C. `rsloop` explores the same idea with a Rust implementation under the hood.

That means:

- your application code stays Python
- the event loop core lives in Rust
- the package aims to support a familiar `asyncio` programming style

## Main ideas

- `rsloop.run(...)` is the easiest way to start.
- `rsloop.new_event_loop()` creates a loop object manually.
- `rsloop.Loop` is the main event loop class.
- Importing `rsloop` also installs a few compatibility patches so it fits better into normal `asyncio` code.

## For Intermediate Python Developers

If you already use `asyncio` comfortably, these are the main things worth knowing early:

- `rsloop` tries to keep the standard `asyncio` mental model, not invent a new one.
- The Python package is mostly a thin wrapper around a Rust extension built with PyO3.
- `rsloop.run(...)` behaves like a focused `asyncio.run(...)` helper that ensures an `rsloop` loop is actually being used.
- Importing `rsloop` can monkeypatch parts of `asyncio`, especially `set_event_loop(...)` and optionally stream helpers such as `open_connection(...)` and `start_server(...)`.
- The runtime model is hybrid: Python coroutines still run as Python code, while lower-level loop coordination and parts of the I/O stack are handled in Rust.
- Compatibility is a project goal, but not every `asyncio` edge case is identical yet, especially around TLS and some transport internals.

If you want to read code instead of only using the package, a good path is:

1. `python/rsloop/_run.py`
2. `python/rsloop/_loop_compat.py`
3. `src/lib.rs`
4. `src/python_api.rs`
5. `src/loop_core.rs`

## What is inside this repository?

At a high level, the repository has five parts:

1. The Python package in `python/rsloop/`
2. The Rust extension in `src/`
3. Example programs in `examples/`
4. Tests in `tests/`
5. Utility material such as `demo/`, `benchmarks/`, and `scripts/`

The rest of the docs explain each part in plain language.

## Typical usage

```python
import rsloop


async def main() -> None:
    print("hello from rsloop")


rsloop.run(main())
```

## Recommended reading order

- Start with [Getting Started](getting-started.md) if you want to use the package.
- Read [Examples](examples.md) if you want copy-paste usage patterns.
- Read [How It Works](how-it-works.md) if you want the big picture.
- Read [Project Structure](project-structure.md) if you want to explore the codebase.
- Read [Development](development.md) if you want to build or test the project locally.
