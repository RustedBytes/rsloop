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
- Read [How It Works](how-it-works.md) if you want the big picture.
- Read [Project Structure](project-structure.md) if you want to explore the codebase.
- Read [Development](development.md) if you want to build or test the project locally.
