<img src="./docs/rsloop.png" alt="rsloop logo" align="center">

# An event loop for asyncio written in Rust

[![PyPI - Version](https://img.shields.io/pypi/v/rsloop)](https://pypi.org/project/rsloop/)
[![Tests](https://github.com/RustedBytes/rsloop/actions/workflows/tests.yml/badge.svg)](https://github.com/RustedBytes/rsloop/actions/workflows/tests.yml)
[![PyPI Downloads](https://static.pepy.tech/personalized-badge/rsloop?period=total&units=INTERNATIONAL_SYSTEM&left_color=BLACK&right_color=GREEN&left_text=downloads)](https://pepy.tech/projects/rsloop)

`rsloop` is a PyO3-based `asyncio` event loop implemented in Rust.

Each `rsloop.Loop` owns a dedicated Rust runtime thread for loop coordination
and I/O work. That thread runs an rsloop-specialized `vibeio` runtime, using
io_uring on Linux, IOCP on Windows, and native kqueue readiness on macOS. Plain
TCP / Unix socket reads and non-TLS server accepts run on that runtime. Python
callbacks, tasks, and coroutines still run on the thread that calls
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

- `profile(...)`, `profiler_running()`, `start_profiler()`, `stop_profiler()`
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

The runtime is centered on one `vibeio` runtime per loop:

- the loop coordination thread is always the central scheduler
- plain TCP / Unix socket reads and non-TLS accept loops use `vibeio` on that
  thread across supported platforms
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
- The transport runtime model is still in transition:
  plain socket reads and non-TLS accepts now run on the loop runtime thread on
  all supported platforms, but generic descriptor watches, writes, and
  TLS-heavy paths are not fully collapsed onto that same single-threaded I/O
  path yet.

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

Build the published-wheel configuration with profile-guided optimization:

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
Tagged and manually dispatched wheel workflows use PGO on every supported
platform except Windows ARM64. Rust profile-generation binaries currently
crash on that target ([rust-lang/rust#156675](https://github.com/rust-lang/rust/issues/156675)),
so it temporarily falls back to the normal fat-LTO release build.

[`scripts/build-wheels.sh`](./scripts/build-wheels.sh) currently defaults to
CPython `3.10 3.11 3.12 3.13 3.14`, and
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

This starts a Tracy client inside the process. Build a release binary, open the
Tracy desktop profiler, then connect to the running process while the profiled
code is executing.

Release wheels do not include profiler support. Build locally with
`--features profiler` to enable it. The Tracy feature set is aimed at local
profiling: `enable`, `only-localhost`, and `sampling`.

For very short-lived runs you can force the process to block on exit until a
server has connected and drained all data by setting `TRACY_NO_EXIT=1` in the
environment.

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

- [`examples/fastapi_service.py`](./examples/fastapi_service.py) for running the same
  FastAPI app on stdlib `asyncio`, `uvloop`, or `rsloop`
- [`benches/compare_event_loops.py`](./benches/compare_event_loops.py)
  for callback, task, and TCP stream comparisons

## Benchmark

```bash
uv run --with maturin maturin develop --release
uv run --with uvloop python benches/compare_event_loops.py
```

An example output from that script on macOS (arm64) with CPython 3.14:

```
callbacks (200,000 ops)
loop           median_s       best_s      ops_per_s     peak_rss   vs_fastest    slower_by
rsloop         0.033083     0.032710      6,045,401     67.5 MiB        1.00x         0.0%
uvloop         0.040958     0.040721      4,883,026     72.8 MiB        1.24x        23.8%
asyncio        0.082233     0.082093      2,432,114     65.3 MiB        2.49x       148.6%

tasks (50,000 ops)
loop           median_s       best_s      ops_per_s     peak_rss   vs_fastest    slower_by
rsloop         0.063593     0.063286        786,247     37.6 MiB        1.00x         0.0%
uvloop         0.069614     0.069420        718,251     38.4 MiB        1.09x         9.5%
asyncio        0.108114     0.107502        462,473     36.1 MiB        1.70x        70.0%

tcp_streams (5,000 ops)
loop           median_s       best_s      ops_per_s     peak_rss   vs_fastest    slower_by
rsloop         0.090940     0.083355         54,981     32.2 MiB        1.00x         0.0%
uvloop         0.133182     0.127404         37,543     31.5 MiB        1.46x        46.5%
asyncio        0.302337     0.299813         16,538     29.6 MiB        3.32x       232.5%
```

The production-shaped workload matrix exercises HTTP, WebSocket libraries,
TLS, mixed message sizes, backpressure, and connection lifecycle behavior:

```bash
uv run --with uvloop python benches/workload_matrix.py \
  --loops rsloop,uvloop \
  --warmups 1 \
  --repeat 5
```

Representative output from the same macOS arm64 (Apple M2) / CPython 3.14
release build on August 18, 2026 is below, as the per-scenario median of five
runs of that command on an otherwise quiet machine. Throughput is traffic-only
operations per second, except for `bulk_transfer`, which reports traffic MiB/s.

| Scenario | rsloop | uvloop | rsloop difference | rsloop p95 | uvloop p95 |
| --- | ---: | ---: | ---: | ---: | ---: |
| HTTP keep-alive | 88,616 | 68,040 | +30.2% | 0.186 ms | 0.283 ms |
| TLS HTTP | 72,530 | 36,772 | +97.2% | 0.273 ms | 0.498 ms |
| Raw WebSocket | 6,242 | 6,458 | -3.3% | 4.271 ms | 3.075 ms |
| Raw WebSocket over TLS | 6,026 | 6,054 | -0.5% | 2.951 ms | 3.403 ms |
| `websockets` | 40,232 | 40,210 | +0.1% | 0.494 ms | 0.452 ms |
| `websockets` over TLS | 41,436 | 26,895 | +54.1% | 0.445 ms | 0.693 ms |
| aiohttp WebSocket | 51,484 | 51,128 | +0.7% | 0.397 ms | 0.367 ms |
| aiohttp WebSocket over TLS | 53,765 | 32,035 | +67.8% | 0.357 ms | 0.568 ms |
| Starlette WebSocket | 29,711 | 20,412 | +45.6% | 0.700 ms | 1.059 ms |
| Starlette WebSocket over TLS | 34,421 | 21,290 | +61.7% | 0.539 ms | 0.856 ms |
| Mixed streams | 79,411 | 48,551 | +63.6% | 0.253 ms | 0.456 ms |
| Bulk transfer (MiB/s) | 4,993.6 | 2,823.6 | +76.9% | 6.347 ms | 11.291 ms |
| Idle activation | 21,189 | 21,275 | -0.4% | 7.843 ms | 7.835 ms |

Read the idle-activation row as a tie rather than a measurement: its traffic
phase is roughly ten milliseconds, and it swung by more than 2x per loop across
those five runs. Every other row held within a few percent.

These ordinary matrix defaults are intentionally short enough for local smoke
and CI runs. Use `--sustained` and compare repeated runs before drawing
performance conclusions for a deployment — competing desktop load matters more
than it looks, because rsloop trades helper-thread CPU for loop-thread work and
so has more to lose when cores are contended.

See [`benches/README.md`](./benches/README.md) for workload details and
extra flags, and [`examples/README.md`](./examples/README.md) for the FastAPI
loop comparison example.

## Acknowledgements

`rsloop` builds on the Python `asyncio` model and is implemented with
[PyO3](https://pyo3.rs/) on the Rust side. Runtime and socket I/O are powered by
[vibeio](https://crates.io/crates/vibeio).

## License

This project is licensed under the Apache License, Version 2.0. See
[`LICENSE`](./LICENSE) for the full text.
