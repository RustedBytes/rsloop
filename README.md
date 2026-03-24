<img src="./rsloop.png" alt="rsloop logo" align="center">

# An event loop for asyncio written in Rust

[![PyPI - Version](https://img.shields.io/pypi/v/rsloop)](https://pypi.org/project/rsloop/)
[![Tests](https://github.com/RustedBytes/rsloop/actions/workflows/tests.yml/badge.svg)](https://github.com/RustedBytes/rsloop/actions/workflows/tests.yml)
[![PyPI Downloads](https://static.pepy.tech/personalized-badge/rsloop?period=total&units=INTERNATIONAL_SYSTEM&left_color=BLACK&right_color=GREEN&left_text=downloads)](https://pepy.tech/projects/rsloop)

`rsloop` is a PyO3-based `asyncio` event loop implemented in Rust.

Each `rsloop.Loop` owns a dedicated Rust runtime thread for loop coordination
and I/O work. On Linux, low-level fd watchers plus plain TCP / Unix socket
readiness, socket reads, and non-TLS server accepts are driven from that thread
through `compio` with `io_uring` support enabled. Python callbacks, tasks, and
coroutines still run on the thread that calls `run_forever()` or
`run_until_complete()` (usually the main Python thread).

The package exposes:

- a native extension module at `rsloop._loop`
- a Python wrapper in [`python/rsloop/__init__.py`](./python/rsloop/__init__.py)
- `rsloop.Loop`, `rsloop.new_event_loop()`, and `rsloop.run(...)`

Repository metadata currently targets Python `>=3.8`. The packaged project now
supports the core event-loop surface on Linux, macOS, and Windows, including
Windows pipe transports and subprocess workflows.

## Install

From PyPI:

```bash
pip install rsloop
```

With `uv`:

```bash
uv add rsloop
```

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

Importing `rsloop` also patches `asyncio.set_event_loop()` so Python 3.8 can
accept an `rsloop.Loop` instance, matching the behavior exercised by
[`tests/test_run.py`](./tests/test_run.py).

## Verified Surface Area

The current codebase implements these user-facing areas.

Loop lifecycle and scheduling:

- `run_forever`, `run_until_complete`, `stop`, `close`
- `time`, `is_running`, `is_closed`
- `get_debug`, `set_debug`
- `call_soon`, `call_soon_threadsafe`, `call_later`, `call_at`
- returned `Handle` and `TimerHandle` objects with `cancel()` / `cancelled()`

Tasks, futures, and execution helpers:

- `create_future`, `create_task`
- `set_task_factory`, `get_task_factory`
- `set_exception_handler`, `get_exception_handler`,
  `call_exception_handler`, `default_exception_handler`
- `set_default_executor`, `run_in_executor`
- `shutdown_asyncgens`, `shutdown_default_executor`
- callback execution under captured `contextvars.Context`
- `asyncio.get_running_loop()` support while running on `rsloop`
- `rsloop.run(...)` helper, with `asyncio.run(..., loop_factory=...)`
  integration on Python 3.12+

I/O and networking:

- `add_reader`, `remove_reader`, `add_writer`, `remove_writer`
- `sock_recv`, `sock_recv_into`, `sock_sendall`, `sock_accept`, `sock_connect`
- `getaddrinfo`, `getnameinfo`
- `create_server`, `create_connection`
- `create_unix_server`, `create_unix_connection`
- `connect_accepted_socket`
- returned `Server` objects with `close()`, `is_serving()`, `get_loop()`,
  and `sockets()`
- returned `StreamTransport` objects with `write()`, `writelines()`, `close()`,
  `abort()`, `is_closing()`, `write_eof()`, `can_write_eof()`,
  `get_extra_info()`, `get_protocol()`, `set_protocol()`,
  `pause_reading()`, `resume_reading()`, `is_reading()`

Pipes, subprocesses, and signals:

- `connect_read_pipe`, `connect_write_pipe`
- `subprocess_exec`, `subprocess_shell`
- returned `ProcessTransport` and `ProcessPipeTransport` objects
- higher-level compatibility with `asyncio.create_subprocess_exec()` and
  `asyncio.create_subprocess_shell()`
- Unix subprocess options including `cwd`, `env`, `executable`, `pass_fds`,
  `start_new_session`, `process_group`, `user`, `group`, `extra_groups`,
  `umask`, and `restore_signals`
- `add_signal_handler`, `remove_signal_handler`

Profiling:

- `profile(...)`, `profiler_running()`, `start_profiler()`, `stop_profiler()`

## Fast Streams

Importing `rsloop` patches `asyncio.open_connection()` and
`asyncio.start_server()` by default.

That import-time behavior is controlled by `RSLOOP_USE_FAST_STREAMS` and can be
disabled with:

```bash
export RSLOOP_USE_FAST_STREAMS=0
```

The native fast-stream path is used only when:

- the running loop is an `rsloop.Loop`
- `ssl` is unset or `None`

Otherwise `rsloop` falls back to the stdlib `asyncio.streams` helpers.

The implementation lives in [`src/fast_streams.rs`](./src/fast_streams.rs) and
is backed by the lower level transport code in
[`src/stream_transport.rs`](./src/stream_transport.rs).

## Runtime Model

Today the runtime is hybrid rather than fully single-threaded:

- the loop coordination thread is always the central scheduler
- on Linux, `add_reader` / `add_writer`, plain socket reads, and non-TLS socket
  accept loops use the `compio` runtime on that thread
- some transport paths still fall back to helper threads, especially TLS I/O,
  TLS server accept, and parts of the legacy transport write path

That means the codebase has started the move toward a single-runtime-thread I/O
model, but has not finished eliminating every helper thread yet.

## Current Limitations

These gaps are visible in the current implementation.

- TLS uses a `rustls` backend with a narrower compatibility surface than
  CPython's OpenSSL-backed `ssl` module. In particular, encrypted private keys
  are not supported yet, and the fast-stream monkeypatch still falls back to
  stdlib helpers whenever `ssl` is enabled. TLS transport internals also still
  use helper-thread paths instead of the newer runtime-thread `compio` socket
  path.
- Subprocess support still has one notable gap:
  `preexec_fn` remains unsupported because running arbitrary Python between
  `fork()` and `exec()` is unsafe in this runtime model.
- Unix-specific APIs remain Unix-specific:
  `create_unix_server`, `create_unix_connection`,
  `add_signal_handler`, `remove_signal_handler`.
- Platform-specific limitations still apply:
  Unix socket APIs and Unix signal handlers remain Unix-only, and several
  subprocess options such as `pass_fds`, `user`, `group`, and `umask` are
  still specific to Unix process spawning.
- The transport runtime model is still in transition:
  plain socket reads and non-TLS accepts now run on the loop runtime thread on
  Linux, but writes and TLS-heavy paths are not fully collapsed onto that same
  single-threaded I/O path yet.

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

Build release wheels into `dist/wheels`:

```bash
scripts/build-wheels.sh
```

[`scripts/build-wheels.sh`](./scripts/build-wheels.sh) currently defaults to
CPython `3.8 3.9 3.10 3.11 3.12 3.13 3.14` plus free-threaded `3.14t`, and
uses `uv python install` / `uv python find` to locate interpreters.

## Profiling

Profiling is behind the Cargo feature `profiler` and is disabled by default.
Build or install with that feature first:

```bash
cargo build --release --features profiler
uv run --with maturin maturin develop --release --features profiler
```

Then wrap the code you want to inspect:

```python
import rsloop

with rsloop.profile():
    rsloop.run(main())
```

Or manage the session manually:

```python
import rsloop

rsloop.start_profiler()
try:
    rsloop.run(main())
finally:
    rsloop.stop_profiler()
```

This starts a Tracy client inside the process. Build a release binary, open
`tracy-profiler.exe`, then connect to the running process while the profiled
code is executing.

The current Tracy feature set is aimed at local Windows profiling:
`enable`, `only-localhost`, `sampling`, and `flush-on-exit`. The last one helps
short-lived runs flush data before exit.

If the extension was built without `--features profiler`, `profile()` and
`start_profiler()` raise a runtime error.

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

- [`demo/fastapi_service.py`](./demo/fastapi_service.py) for running the same
  FastAPI app on stdlib `asyncio`, `uvloop`, or `rsloop`
- [`benchmarks/compare_event_loops.py`](./benchmarks/compare_event_loops.py)
  for callback, task, and TCP stream comparisons

## Benchmark

```bash
uv run --with maturin maturin develop --release
uv run --with uvloop python benchmarks/compare_event_loops.py
```

An example output from that script on Linux with CPython 3.14.0:

```
callbacks (200,000 ops)
loop           median_s       best_s      ops_per_s   vs_fastest
rsloop         0.041608     0.040585      4,806,807        1.00x
uvloop         0.087539     0.086690      2,284,707        2.10x
asyncio        0.229563     0.221348        871,222        5.52x

tasks (50,000 ops)
loop           median_s       best_s      ops_per_s   vs_fastest
uvloop         0.084425     0.083497        592,239        1.00x
rsloop         0.091845     0.090982        544,397        1.09x
asyncio        0.138782     0.137716        360,276        1.64x

tcp_streams (5,000 ops)
loop           median_s       best_s      ops_per_s   vs_fastest
rsloop         0.119483     0.118451         41,847        1.00x
uvloop         0.119582     0.116446         41,812        1.00x
asyncio        0.138408     0.134438         36,125        1.16x
```

See [`benchmarks/README.md`](./benchmarks/README.md) for workload details and
extra flags, and [`demo/README.md`](./demo/README.md) for the FastAPI loop
comparison demo.

## Acknowledgements

`rsloop` builds on the Python `asyncio` model and is implemented with
[PyO3](https://pyo3.rs/) on the Rust side. The runtime and I/O work in the
current implementation rely in part on
[compio](https://github.com/compio-rs/compio). On Windows, parts of the
runtime also rely on
[vibeio](https://crates.io/crates/vibeio).

## License

This project is licensed under the Apache License, Version 2.0. See
[`LICENSE`](./LICENSE) for the full text.
