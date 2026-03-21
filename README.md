<img src="./rsloop.png" alt="rsloop logo" align="center">

# `rsloop`: An event loop for asyncio written in Rust

[![PyPI - Version](https://img.shields.io/pypi/v/rsloop)](https://pypi.org/project/rsloop/)
[![PyPI Downloads](https://static.pepy.tech/personalized-badge/rsloop?period=total&units=INTERNATIONAL_SYSTEM&left_color=BLACK&right_color=GREEN&left_text=downloads)](https://pepy.tech/projects/rsloop)

`rsloop` is a PyO3-based `asyncio` event loop. Each `rsloop.Loop` owns a
dedicated Rust runtime thread that handles loop coordination and I/O-related
runtime work, while Python callbacks, tasks, and coroutines are executed on
the thread that calls `run_forever()` / `run_until_complete()` (typically the
main thread). The package exposes a Python module at
`rsloop._loop` plus a small Python wrapper in `python/rsloop/__init__.py`.

The project metadata currently targets Python `>=3.8`. The current
implementation is Unix-focused and intended for Linux and macOS; Windows is
not supported by this codebase.

## Current Surface Area

Today’s codebase provides:

- `rsloop.Loop`, `rsloop.new_event_loop()`, and `rsloop.run(...)`
- loop lifecycle:
  - `run_forever`
  - `run_until_complete`
  - `stop`
  - `close`
  - `time`
  - `is_running`
  - `is_closed`
  - debug flag getters/setters
- callback scheduling:
  - `call_soon`
  - `call_soon_threadsafe`
  - `call_later`
  - `call_at`
  - returned `Handle` and `TimerHandle` objects
- future/task helpers:
  - `create_future`
  - `create_task`
  - `set_task_factory` / `get_task_factory`
  - exception handler storage and dispatch
  - `set_default_executor`
  - `run_in_executor`
  - `shutdown_asyncgens`
  - `shutdown_default_executor`
- callback execution inside captured `contextvars.Context`
- `asyncio.get_running_loop()` support for coroutines driven by `rsloop`
- `asyncio.run(..., loop_factory=rsloop.new_event_loop)` support on Python 3.12+
- Unix fd readiness callbacks via `add_reader` / `remove_reader` /
  `add_writer` / `remove_writer`
- raw socket awaitables:
  - `sock_recv`
  - `sock_recv_into`
  - `sock_sendall`
  - `sock_accept`
  - `sock_connect`
- DNS awaitables:
  - `getaddrinfo`
  - `getnameinfo`
- stream transports and servers:
  - `create_server`
  - `create_connection`
  - `create_unix_server`
  - `create_unix_connection`
  - `connect_accepted_socket`
  - returned `Server` and `StreamTransport` objects
  - `transport.close()` / `transport.abort()` are implemented for socket
    transports
- pipe transports:
  - `connect_read_pipe`
  - `connect_write_pipe`
- subprocess transports:
  - `subprocess_exec`
  - `subprocess_shell`
  - returned `ProcessTransport` and `ProcessPipeTransport` objects
  - higher-level `asyncio.create_subprocess_exec()`
  - higher-level `asyncio.create_subprocess_shell()`
  - low-level support for `cwd`, `env`, `executable`, `pass_fds`,
    `start_new_session`, `process_group`, `user`, `group`, `extra_groups`,
    `umask`, and `restore_signals`
- Unix signal handlers via `add_signal_handler` / `remove_signal_handler`
- a free-threaded module declaration via `#[pymodule(gil_used = false)]`
- profiler entry points:
  - `profile`
  - `profiler_running`
  - `start_profiler`
  - `stop_profiler`

## Fast Streams

Importing `rsloop` patches `asyncio.open_connection()` and
`asyncio.start_server()` by default. That import-time patch is controlled by
`RSLOOP_USE_FAST_STREAMS` and can be disabled with:

```bash
export RSLOOP_USE_FAST_STREAMS=0
```

The patched helpers only take the native `rsloop` fast-stream path when:

- the currently running loop is an `rsloop.Loop`
- `ssl` is unset or `None`

Otherwise they fall back to the stdlib `asyncio` stream helpers. The fast
stream implementation lives in Rust in `src/fast_streams.rs`, while the lower
level transport implementation lives in `src/stream_transport.rs`.

## Current Gaps

- TLS is not implemented:
  - `start_tls()` is stubbed
  - `ssl=...` is rejected for `create_server`, `create_connection`,
    `create_unix_server`, `create_unix_connection`, and
    `connect_accepted_socket`
- stream transport flow control is still partial:
  - `pause_reading()` / `resume_reading()` work
  - `get_write_buffer_size()` returns `0`
  - `get_write_buffer_limits()` returns `(0, 0)`
  - `set_write_buffer_limits()` is a no-op
- several asyncio compatibility parameters are currently accepted only for API
  shape and are not implemented:
  - `create_connection(..., server_hostname=..., happy_eyeballs_delay=..., interleave=..., all_errors=...)`
  - TLS timeout parameters such as `ssl_handshake_timeout` /
    `ssl_shutdown_timeout`
  - `shutdown_default_executor(timeout=...)`
- subprocess support is intentionally incomplete:
  - `preexec_fn` is unsupported
  - text mode is rejected for higher-level
    `asyncio.create_subprocess_exec()` /
    `asyncio.create_subprocess_shell()` because the stdlib stream protocol is
    byte-oriented
  - low-level `loop.subprocess_exec()` / `loop.subprocess_shell()` do support
    text decoding when used with a custom subprocess protocol
- Unix-only APIs remain Unix-only:
  - `create_unix_server`
  - `create_unix_connection`
  - `add_signal_handler` / `remove_signal_handler`

## Build

Quick check:

```bash
cargo check
```

Release build and editable install:

```bash
cargo build --release
uv run --with maturin maturin develop --release
```

Build release wheels for CPython 3.8 through 3.14, plus the free-threaded
3.14 build, into `dist/wheels`:

```bash
scripts/build-wheels.sh
```

## Publishing

PyPI releases are published from Git tags that start with `v`, for example
`v0.1.0`. The GitHub Actions workflow builds wheels by calling
`scripts/build-wheels.sh`, builds an `sdist`, and then publishes the combined
artifacts to PyPI with GitHub trusted publishing.

Before the first release, configure the `rsloop` project on PyPI as a trusted
publisher for this repository/workflow.

Use a release build for benchmarks and runtime comparisons. A debug extension
will make the Rust loop look much slower than it really is.

## Profiling

Profiling support is behind the Cargo feature `profiler` and is disabled by
default. Build/install a release extension with that feature enabled first:

```bash
cargo build --release --features profiler
uv run --with maturin maturin develop --release --features profiler
```

Then wrap the workload you want to inspect:

```python
import rsloop

with rsloop.profile("rsloop-flamegraph.svg", frequency=999):
    rsloop.run(main())
```

You can also manage the session manually:

```python
import rsloop

rsloop.start_profiler(frequency=999)
try:
    rsloop.run(main())
finally:
    rsloop.stop_profiler("rsloop-flamegraph.svg")
```

This writes an SVG flamegraph that you can open directly in a browser. Run the
profile against a release build or the stack samples will be dominated by debug
overhead. If the extension was built without `--features profiler`,
`start_profiler()` and `profile()` will raise a runtime error.
Only `format="flamegraph"` is currently implemented.

## Usage

Simple entry point:

```python
import rsloop

async def main():
    ...

rsloop.run(main())
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

## Examples

```bash
uv run python examples/01_basics.py
uv run python examples/02_fd_and_sockets.py
uv run python examples/03_streams.py
uv run python examples/04_unix_and_accepted_socket.py
uv run python examples/05_pipes_signals_subprocesses.py
```

The repository also includes:

- `demo/fastapi_service.py` for running the same FastAPI app on stdlib
  `asyncio`, `uvloop`, or `rsloop`
- `benchmarks/compare_event_loops.py` for comparing callback, task, and TCP
  stream workloads

## Benchmark

```bash
uv run --with maturin maturin develop --release
uv run --with uvloop python benchmarks/compare_event_loops.py
```

See `benchmarks/README.md` for workload details and extra benchmark flags.
