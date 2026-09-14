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

Repository metadata currently targets Python `>=3.10`.
The native runtime requires Linux 6.1+, macOS 13+, or Windows 10+ so its hot
paths can rely on modern completion, timer, and scheduler primitives.
Free-threaded CPython (`3.14t`) is supported: the extension declares
`gil_used = false`, so importing it no longer re-enables the GIL. See
[Free-Threaded CPython](#free-threaded-cpython) for what that does and does not
buy you.

## Documentation

Project documentation now lives in [`docs/`](./docs/).

If you are new to the repository, start with:

- [`docs/index.md`](./docs/index.md)
- [`docs/getting-started.md`](./docs/getting-started.md)
- [`docs/how-it-works.md`](./docs/how-it-works.md)
- [`docs/project-structure.md`](./docs/project-structure.md)

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

## Custom Async Rust Extensions

`rsloop` now exposes a small Rust interop API for downstream PyO3 extensions.
That lets you write your own async Rust code, return it to Python as an
awaitable, and run it under the active `rsloop` event loop.

The public entry point is `rsloop::rust_async`:

- `get_current_locals(...)`
- `future_into_py(...)`
- `future_into_py_with_locals(...)`
- `local_future_into_py(...)`
- `local_future_into_py_with_locals(...)`
- re-exports of `TaskLocals` and `into_future_with_locals(...)`

See [`examples/rust/README.md`](./examples/rust/README.md) for a complete
extension example built with `maturin`.

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

- Python 3.15's external `profiling.sampling` profiler
- opt-in transport counters through `transport_stats()` and
  `reset_transport_stats()`

Set `RSLOOP_TRANSPORT_STATS=1` before importing rsloop to enable the transport
counters. They report read completions and bytes, Python-thread read drains,
wakeups, staged and direct writes, and Windows completion-to-poll rebinds.
Counters remain disabled by default so diagnostics add only one predictable
branch to transport hot paths.

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

On that path the reader handed to your code is the native
`PyFastStreamReader` rather than `asyncio.StreamReader`. It implements the
reading surface protocols actually use:

- `read(n=-1)`, `readexactly(n)`
- `readline()`, `readuntil(separator=b"\n")`, including the tuple-of-separators
  form CPython 3.13+ accepts
- `at_eof()`, `exception()`, `feed_data()`, `feed_eof()`, `set_exception()`

These match `asyncio.StreamReader` down to the exception types and their
attributes — `IncompleteReadError.partial`, `LimitOverrunError.consumed`, the
`ValueError` that `readline()` raises on limit overrun — and down to what is
left in the buffer afterwards.
[`tests/test_stream_reader.py`](./tests/test_stream_reader.py) pins that by
driving the same feed scripts through both readers and comparing the results.

The implementation lives in
[`src/transport/stream/fast.rs`](./src/transport/stream/fast.rs) and
is backed by the lower level transport code in
[`src/transport/stream/mod.rs`](./src/transport/stream/mod.rs).

## Free-Threaded CPython

`rsloop` builds and runs on free-threaded CPython 3.14 (`3.14t`). The extension
declares `#[pymodule(gil_used = false)]`, which is what keeps CPython from
silently switching the GIL back on for the whole process at import time:

```python
import sys
import rsloop

assert not sys._is_gil_enabled()
assert rsloop.build_info()["free_threaded"]
```

What that buys you is that separate `rsloop.Loop` instances on separate threads
run *concurrently* rather than taking turns. A loop is still single-threaded
internally, and asyncio objects are still not thread-safe, so the model is one
loop per thread — not one loop shared across threads. `call_soon_threadsafe()`
remains the supported way to hand work to a loop from another thread, and it
keeps its FIFO ordering guarantee.

The pieces that made this safe:

- the generic stream-reader fast path writes into `StreamReader._buffer`
  through a raw pointer; the size read, resize, and copy now run inside a
  critical section on that `bytearray`, so a concurrent mutation cannot leave
  the copy writing into a freed allocation
- the ready-queue refill preserves scheduling order when a drain slice leaves
  older callbacks in the batch. Under the GIL a cross-thread producer could
  only enqueue while the loop thread was parked, so the reordering was
  essentially unreachable; without the GIL producers append throughout the
  drain and it became routine

`tests/test_free_threading.py` covers this: parallel loops over both the native
and stdlib stream reader paths, `call_soon_threadsafe()` fan-in from eight
threads, and a check that importing rsloop leaves the GIL off.

Wheels are built for `3.14t` alongside the GIL builds, and the test matrix runs
it as its own entry.

## Runtime Model

Each loop combines a coordination runtime with a loop-thread I/O runtime:

- the coordination thread handles commands, timers, and cross-thread work
- on Unix, generic TCP protocol readers run on the Python loop thread;
  native fast streams and Unix-domain readers retain coordination-thread I/O
- non-TLS accept loops use `vibeio` on the thread that starts them
- bounded ready-callback turns service loop-thread I/O even when Python tasks
  continually yield with `sleep(0)`
- Windows TCP transports, including custom `asyncio.Protocol` implementations,
  start in IOCP completion mode and rebind to readiness mode before `start_tls`
  synchronously reclaims a socket
- generic `add_reader` / `add_writer` descriptors use cancellable OS-poll
  workers because `vibeio` does not expose arbitrary raw-descriptor registration
- some transport paths still fall back to helper threads, especially TLS I/O,
  TLS server accept, and parts of the legacy transport write path

The runtime dependency is now unified, but the codebase has not finished
eliminating every helper thread yet.

Transport overload safeguards use conservative defaults: inbound reads pause
at 1 MiB of pending data per connection, buffered writes are capped at 64 MiB,
and a TLS server admits at most 256 simultaneous handshakes. The last two limits
can be adjusted before importing `rsloop` with
`RSLOOP_MAX_WRITE_BUFFER_BYTES` and `RSLOOP_MAX_PENDING_TLS_HANDSHAKES`.

## Current Limitations

These gaps are visible in the current implementation.

- TLS uses a `rustls` backend with a narrower compatibility surface than
  CPython's OpenSSL-backed `ssl` module. In particular, encrypted private keys
  are not supported yet, and the fast-stream monkeypatch still falls back to
  stdlib helpers whenever `ssl` is enabled. TLS transport internals also still
  use helper-thread paths instead of the runtime-thread `vibeio` socket
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
- The transport runtime model is still in transition: protocol readers on Unix
  avoid a coordination-thread hop, but native streams, generic descriptor
  watches, and TLS-heavy paths do not share one single-threaded I/O path.

## Build

Local development uses Python 3.14.7, pinned in `.python-version`. Install that
interpreter before running the `uv` commands below. This development pin does
not change the package's Python 3.10+ support or the multi-version test matrix.

Local builds and build/test CI use Rust `1.98.1`, pinned in
[`rust-toolchain.toml`](./rust-toolchain.toml). Rustup selects it automatically
inside this repository. LLVM tools remain optional for PGO builds.

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

Optionally build wheels with profile-guided optimization:

```bash
rustup component add llvm-tools-preview
scripts/build-pgo-wheels.sh
```

For each requested Python ABI, the PGO wrapper creates an instrumented wheel,
trains it on sustained HTTP, TLS, WebSocket, mixed-stream, bulk-transfer,
idle-connection, callback, task, and TCP workloads, merges the resulting LLVM
profiles, and builds that ABI's final wheel with its matching profile. Per-ABI
training avoids discarding counters when PyO3's generated control flow differs
between Python versions or free-threaded builds. The target must be native
because the instrumented extension runs during training.

Set `RSLOOP_PGO_SCENARIOS` to override the comma-separated network scenarios.
The **Wheels** CI workflow disables PGO by default: tagged releases and ordinary
manual runs use the normal release-wheel builder. To opt in, enable the `pgo`
checkbox when manually running the workflow. LLVM tools are installed only for
PGO runs; source-distribution and publishing steps are unchanged.
When enabled, PGO is used on every supported platform except Windows ARM64.
Rust profile-generation binaries currently
crash on that target ([rust-lang/rust#156675](https://github.com/rust-lang/rust/issues/156675)),
so it temporarily falls back to the normal fat-LTO release build.

[`scripts/build-wheels.sh`](./scripts/build-wheels.sh) currently defaults to
CPython `3.10 3.11 3.12 3.13 3.14 3.14t 3.15`, and
uses `uv python install` / `uv python find` to locate interpreters.

## Profiling

Python 3.15 includes a low-overhead sampling profiler that can run rsloop
without a special build or in-process instrumentation. Generate an interactive
flame graph with:

```bash
uv run --python 3.15 --with maturin maturin develop --release
uv run --python 3.15 python -m profiling.sampling run \
  --all-threads --native --flamegraph \
  -o rsloop-profile.html examples/01_basics.py
```

`--all-threads` includes rsloop's runtime thread and `--native` marks time below
the Python/native boundary. The profiler and target must use the same Python
3.15 interpreter. Python 3.15 does not allow these options together with
`--async-aware`; use a separate async-aware pass when coroutine reconstruction
is more important than native and multi-thread visibility.

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
