<img src="./docs/rsloop.png" alt="rsloop logo" align="center">

# An event loop for asyncio written in Rust

[![PyPI - Version](https://img.shields.io/pypi/v/rsloop)](https://pypi.org/project/rsloop/)
[![Tests](https://github.com/RustedBytes/rsloop/actions/workflows/tests.yml/badge.svg)](https://github.com/RustedBytes/rsloop/actions/workflows/tests.yml)
[![PyPI Downloads](https://static.pepy.tech/personalized-badge/rsloop?period=total&units=INTERNATIONAL_SYSTEM&left_color=BLACK&right_color=GREEN&left_text=downloads)](https://pepy.tech/projects/rsloop)

`rsloop` is a PyO3-based `asyncio` event loop implemented in Rust.

Each `rsloop.Loop` owns a dedicated Rust runtime thread for loop coordination
and I/O work. That thread runs an rsloop-specialized `vibeio` runtime, using
io_uring on Linux, IOCP on Windows, and native kqueue readiness on macOS.
Native-stream TCP reads and Unix-domain socket reads run on that runtime. On Unix, generic
TCP protocol readers use a second `vibeio` runtime on the Python loop thread
(io_uring on Linux), avoiding cross-thread delivery of each read. Non-TLS accepts
run on either runtime depending on where the server starts. Python callbacks,
tasks, and coroutines run on the thread that calls
`run_forever()` or `run_until_complete()` (usually the main Python thread).

The package exposes:

- a native extension module at `rsloop._loop`
- a Python wrapper in [`python/rsloop/__init__.py`](./python/rsloop/__init__.py)
- `rsloop.Loop`, `rsloop.EventLoopPolicy`, `rsloop.new_event_loop()`,
  `rsloop.run(...)`, `rsloop.install()`, `rsloop.uninstall()`, and
  `rsloop.build_info()`

Repository metadata supports CPython 3.10 through 3.15.
The native runtime requires Linux 6.1+, macOS 13+, or Windows 10+ so its hot
paths can rely on modern completion, timer, and scheduler primitives.
Free-threaded CPython (`3.14t`) is supported: the extension declares
`gil_used = false`, so importing it no longer re-enables the GIL. See
[Free-Threaded CPython](./docs/free-threading.md) for what that does and does not
buy you.

## Documentation

Project documentation now lives in [`docs/`](./docs/).

If you are new to the repository, start with:

- [`docs/index.md`](./docs/index.md)
- [`docs/getting-started.md`](./docs/getting-started.md)
- [`docs/supported-features.md`](./docs/supported-features.md)
- [`docs/fast-streams.md`](./docs/fast-streams.md)
- [`docs/free-threading.md`](./docs/free-threading.md)
- [`docs/rust-extensions.md`](./docs/rust-extensions.md)
- [`docs/how-it-works.md`](./docs/how-it-works.md)
- [`docs/project-structure.md`](./docs/project-structure.md)
- [`docs/development.md`](./docs/development.md) for building, testing, and profiling

To browse the docs locally with MkDocs:

```bash
uvx --from mkdocs mkdocs serve
```

## Install

From PyPI:

```bash
pip install rsloop
```

With `uv`:

```bash
uv add rsloop
```

From [conda-forge](https://conda-forge.org), using [pixi](https://pixi.prefix.dev/latest/#installation):

```bash
pixi add rsloop
```

## Usage

Simple entry point:

```python
import rsloop


async def main(): ...


rsloop.run(main())
```

Install as the default asyncio event loop policy:

```python
import asyncio
import rsloop

rsloop.install()
try:
    asyncio.run(main())
finally:
    rsloop.uninstall()
```

Manual loop creation also works:

```python
import asyncio
import rsloop

loop = rsloop.new_event_loop()
asyncio.set_event_loop(loop)
try:
    loop.run_until_complete(...)
finally:
    asyncio.set_event_loop(None)
    loop.close()
```

Importing `rsloop` also patches `asyncio.set_event_loop()` so Python 3.10 can
accept an `rsloop.Loop` instance, matching the behavior exercised by
[`tests/test_run.py`](./tests/test_run.py).

## Examples

Run the repository examples from the project root:

```bash
uv run python examples/01_basics.py
uv run python examples/02_fd_and_sockets.py
uv run python examples/03_streams.py
uv run python examples/04_unix_and_accepted_socket.py
uv run python examples/05_pipes_signals_subprocesses.py
```

Example files:
[`examples/01_basics.py`](./examples/01_basics.py),
[`examples/02_fd_and_sockets.py`](./examples/02_fd_and_sockets.py),
[`examples/03_streams.py`](./examples/03_streams.py),
[`examples/04_unix_and_accepted_socket.py`](./examples/04_unix_and_accepted_socket.py),
[`examples/05_pipes_signals_subprocesses.py`](./examples/05_pipes_signals_subprocesses.py).

The repository also includes:

- [`examples/fastapi_service.py`](./examples/fastapi_service.py) for running the same
  FastAPI app on stdlib `asyncio`, `uvloop`, or `rsloop`
- [`examples/granian_service.py`](./examples/granian_service.py) for registering
  `rsloop` as Granian's worker event loop
- [`benches/compare_event_loops.py`](./benches/compare_event_loops.py)
  for callback, task, and TCP stream comparisons
- [`benches/compare_granian.py`](./benches/compare_granian.py) for Granian HTTP
  throughput and latency comparisons with `oha`

## Benchmarks

See [benchmark results and reproduction commands](./docs/benchmarks.md).

## Acknowledgements

`rsloop` builds on the Python `asyncio` model and is implemented with
[PyO3](https://pyo3.rs/) on the Rust side. Runtime and socket I/O are powered by
[vibeio](https://crates.io/crates/vibeio).

## License

This project is licensed under the Apache License, Version 2.0. See
[`LICENSE`](./LICENSE) for the full text.
