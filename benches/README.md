# Event Loop Benchmark

This benchmark compares:

- stdlib `asyncio`
- `uvloop`
- the Rust prototype in `rsloop`

Run it from the repository root so Python resolves the editable Rust package cleanly:

```bash
uv run --with maturin maturin develop --release
uv run --with uvloop python benches/compare_event_loops.py
```

Benchmark runner: [`benches/compare_event_loops.py`](./compare_event_loops.py)

The Rust prototype should be installed in release mode before benchmarking.
Using the default debug build will heavily skew the comparison against
`asyncio` and `uvloop`.

Useful quick run:

```bash
uv run --with maturin maturin develop --release
uv run --with uvloop python benches/compare_event_loops.py \
  --warmups 0 \
  --repeat 3 \
  --callbacks 50000 \
  --tasks 10000 \
  --tcp-roundtrips 1000 \
  --payload-size 512
```

To launch an unmeasured Tracy session for each `rsloop` workload before the
measured runs, add a label directory:

```bash
uv run --with maturin maturin develop --release --features profiler
uv run --with uvloop python benches/compare_event_loops.py \
  --loops rsloop \
  --workloads callbacks,tasks,tcp_streams \
  --profile-rsloop-dir benches/profiles
```

No files are written by Tracy. The directory argument is only used to derive a
human-readable label for the unmeasured profiling pass before the warmup and
measured runs. Open the Tracy desktop profiler and connect while that pass is
running.

The runner executes each loop/workload in a fresh subprocess and reports:

- median time
- best time
- operations per second
- relative slowdown versus the fastest loop for that workload, as both a
  multiplier and a percentage

Current workloads:

- `callbacks`: pre-scheduled batch of `call_soon()` callbacks (schedule + dispatch cost)
- `tasks`: many tiny `asyncio.sleep(0)` tasks
- `tcp_streams`: local `asyncio.start_server()` / `asyncio.open_connection()` echo round trips

By default, `tcp_streams` uses `rsloop`'s native fast streams. If you want all
three loops to go through the stdlib `asyncio` streams layer instead, pass:

```bash
uv run --with uvloop python benches/compare_event_loops.py --no-rsloop-fast-streams
```

## Representative workload matrix

[`workload_matrix.py`](./workload_matrix.py) complements the microbenchmarks
with concurrent, production-shaped network traffic. By default, each
loop/scenario pair gets one subprocess so warmups populate the same process
caches used by measured runs. It reports total and traffic-only throughput,
p50/p95/p99 operation latency, and peak RSS.

The scenarios are:

- `http_keepalive`: concurrent HTTP/1.1 keep-alive clients, configurable
  response bodies, and modest application CPU work
- `tls_http`: the same workload over TLS
- `websocket_messages`: concurrent persistent RFC 6455 connections with
  masked client frames and mixed message sizes
- `websocket_tls`: the raw RFC 6455 workload over TLS
- `websockets_messages` / `websockets_tls`: the same message pattern through
  the `websockets` client and server with compression disabled
- `aiohttp_websocket_messages` / `aiohttp_websocket_tls`: an aiohttp server
  driven by the same `websockets` client, also with compression disabled
- `starlette_websocket_messages` / `starlette_websocket_tls`: Starlette's ASGI
  WebSocket path served by uvicorn with per-message deflate disabled
- `mixed_streams`: concurrent connections cycling through 64 B to 64 KiB
  messages
- `bulk_transfer`: concurrent large transfers with `drain()` backpressure
- `idle_connections`: many idle connections activated at the same time

Build rsloop in release mode and run the standard matrix with:

```bash
uv run --with maturin maturin develop --release
uv run --with uvloop python benches/workload_matrix.py  # Unix
uv run --with winloop python benches/workload_matrix.py # Windows
```

The default comparison is `asyncio,uvloop,rsloop` on Unix. Because uvloop is
not available on Windows, the default there is `asyncio,winloop,rsloop`.
Unavailable optional loops are reported and skipped.

For a quick smoke run:

```bash
uv run --with uvloop python benches/workload_matrix.py \
  --warmups 0 \
  --repeat 1 \
  --concurrency 4 \
  --requests-per-connection 5 \
  --bulk-bytes 262144 \
  --idle-connections 20 \
  --idle-seconds 0.01
```

Use `--json-output benches/results/matrix.json` to retain raw measurements.
Pass `--measurement-mode cold` to put every warmup and measured run in a fresh
process when startup cost is the subject of the comparison.
For performance conclusions, add `--sustained`. It raises short loopback
workloads to at least two warmups, seven measured runs, and 500 operations per
connection; the ordinary defaults remain intentionally quick for smoke and CI
runs.
TLS uses the test certificates under `tests/fixtures/tls`; regenerate them
cross-platform when needed with:

```bash
uv run --no-project python scripts/generate_test_tls_certs.py tests/fixtures/tls
```

For Tracy, build with the profiler feature and request an unmeasured profiling
pass before each rsloop scenario:

```bash
uv run --with maturin maturin develop --release --features profiler
uv run --with uvloop python benches/workload_matrix.py \
  --loops rsloop \
  --profile-rsloop-dir benches/profiles \
  --allow-profiler-build
```

`--allow-profiler-build` is required because this invocation also reports
measurements from the Tracy-enabled binary. Treat those measurements as
profiling diagnostics, not as comparable release-build results. Rebuild without
`--features profiler` before collecting the normal comparison matrix.

On Windows, replace `--with uvloop` with `--with winloop` in the quick and
profiling commands.

Treat this matrix as a regression and trade-off tool, not a single leaderboard.
Compare throughput together with tail latency and memory, and tune concurrency,
payload sizes, application work, and connection counts to match the target
deployment.

## Regression gate and runtime microbenchmarks

Save comparable before/after runs, then enforce the default budgets of 3%
throughput, 5% p95/p99 latency, and 5% peak RSS:

```bash
uv run python benches/check_regression.py \
  benches/results/baseline.json benches/results/candidate.json
```

Add `--require-improvement 10` when validating an optimization that is expected
to improve at least one workload by 10%. The checker accepts JSON from either
benchmark runner and only compares matching `rsloop` measurements.

The embedded `src/vibeio` executor also has a release-mode microbenchmark for
spawn/join, single-task self-wakes, and batches of 256 self-waking tasks:

```bash
cargo bench --bench runtime
```

It prints CSV with seven measured samples after three warmups per workload.
Each sample includes runtime creation, spawning, polling, joining, and teardown;
`operations` counts task polls (including each task's final ready poll). The
benchmark compiles the embedded source directly, without making it public API.
Save stdout for before/after comparisons, use identical CPU affinity, and run
benchmarks without concurrent builds or tests. On Linux, for example:

```bash
taskset -c 2 cargo bench --bench runtime > /tmp/runtime.csv
```

These Rust scheduler measurements isolate dispatch costs; use the Python
benchmarks above to establish whether improvements carry through to applications.

For kernel-level profiling, pair the same release workloads with `perf stat` and
`perf record` on Linux, Instruments on macOS, or Windows Performance Recorder.
Track context switches, syscalls, scheduler wakeups, CPU cycles, and allocation
or peak-RSS changes alongside throughput and tail latency.
