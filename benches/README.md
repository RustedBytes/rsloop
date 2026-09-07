# Event Loop Benchmark

The [systems performance investigation](systems-performance.md) explains the
remaining callback/protocol gap, measures numeric resolution and read-pool
notifications, and records a pre-existing intermittent transfer failure.

The [full comparison at commit 9011c9f](full-benchmark-9011c9f.md) records fresh
microbenchmarks, all 12 network workloads, and nine-block idle activation results
against uvloop and zuvloop, with compact run-level data.

See the [five-path optimization experiment](optimization-results.md) for
before/after socket, buffered-I/O, timer, write, and callback measurements,
including application-level regressions and reproducible alternating A/B runs.

For embedded timer repoll costs, run `cargo bench --bench timer --locked`.
It measures unchanged/changing wakers with one or 1,024 pending timers, without
OS waiting. See [vibeio performance results](vibeio-performance.md) for measured
before/after results and limitations; these are not Python event-loop comparisons.

This benchmark compares:

- stdlib `asyncio`
- `uvloop` (`winloop` on Windows)
- `zuvloop` on Python 3.14+
- the Rust prototype in `rsloop`

Run it from the repository root so Python resolves the editable Rust package cleanly:

```bash
uv run --with maturin maturin develop --release
uv run --with uvloop --with 'zuvloop; python_version >= "3.14"' python benches/compare_event_loops.py
```

Benchmark runner: [`benches/compare_event_loops.py`](./compare_event_loops.py)

The Rust prototype should be installed in release mode before benchmarking.
Using the default debug build will heavily skew the comparison against
`asyncio` and `uvloop`.

Useful quick run:

```bash
uv run --with maturin maturin develop --release
uv run --with uvloop --with 'zuvloop; python_version >= "3.14"' python benches/compare_event_loops.py \
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
uv run --with uvloop --with 'zuvloop; python_version >= "3.14"' python benches/compare_event_loops.py \
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
selected loops to go through the stdlib `asyncio` streams layer instead, pass:

```bash
uv run --with uvloop --with 'zuvloop; python_version >= "3.14"' python benches/compare_event_loops.py --no-rsloop-fast-streams
```

## Representative workload matrix

[`workload_matrix.py`](./workload_matrix.py) complements the microbenchmarks
with concurrent, production-shaped network traffic. Except for idle activation,
each loop/scenario pair gets one subprocess by default so warmups populate the
same process caches used by measured runs. These scenarios report total and
traffic-only throughput, p50/p95/p99 operation latency, and peak RSS. Idle v2
instead reports shared-origin latency across fresh-process paired blocks.

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
- `idle_connections`: repeated idle/wakeup cycles on established connections,
  measured as shared-origin activation latency (see below)

Build rsloop in release mode and run the standard matrix with:

```bash
uv run --with maturin maturin develop --release
uv run --with uvloop --with 'zuvloop; python_version >= "3.14"' python benches/workload_matrix.py  # Unix
uv run --with winloop --with 'zuvloop; python_version >= "3.14"' python benches/workload_matrix.py # Windows
```

The default comparison is `asyncio,uvloop,rsloop` on Unix. Because uvloop is
not available on Windows, the default there is `asyncio,winloop,rsloop`.
On Python 3.14+, zuvloop is added before rsloop in both defaults. Older Python
versions retain the three-loop defaults. Select a subset with `--loops`, for
example `--loops uvloop,zuvloop,rsloop` on Unix with Python 3.14+.
Unavailable optional loops are reported and skipped; workload failures are not
silently replaced by another loop. zuvloop is a benchmark-only dependency.

For a quick smoke run:

```bash
uv run --with uvloop --with 'zuvloop; python_version >= "3.14"' python benches/workload_matrix.py \
  --warmups 0 \
  --repeat 1 \
  --concurrency 4 \
  --requests-per-connection 5 \
  --bulk-bytes 262144 \
  --idle-connections 20 \
  --idle-cycles 3 \
  --idle-warmup-cycles 1 \
  --idle-seconds 0.01
```

Use `--json-output benches/results/matrix.json` to retain raw measurements.
Pass `--measurement-mode cold` to put every warmup and measured run in a fresh
process when startup cost is the subject of the comparison.
For performance conclusions, add `--sustained`. It raises short loopback
workloads to at least two warmups, seven measured runs, and 500 operations per
connection; the ordinary defaults remain intentionally quick for smoke and CI
runs.
Idle activation is an exception: it always uses fresh-process paired blocks
and its own warmup cycles, regardless of `--measurement-mode` or `--warmups`.
Use the explicit small cycle count above for smoke tests.
TLS uses the test certificates under `tests/fixtures/tls`; regenerate them
cross-platform when needed with:

```bash
uv run --no-project python scripts/generate_test_tls_certs.py tests/fixtures/tls
```

For Tracy, build with the profiler feature and request an unmeasured profiling
pass before each rsloop scenario:

```bash
uv run --with maturin maturin develop --release --features profiler
uv run --with uvloop --with 'zuvloop; python_version >= "3.14"' python benches/workload_matrix.py \
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

### Idle activation v2

```bash
.venv/bin/python benches/workload_matrix.py \
  --loops rsloop,uvloop --scenarios idle_connections \
  --repeat 8 --idle-connections 200 \
  --idle-cycles 100 --idle-warmup-cycles 5 --idle-seconds 0.2 \
  --json-output target/idle-v2-paired.json
```

Allow about six minutes for this two-loop comparison. Each process establishes
200 connections once, discards five idle/activation warmup cycles, then measures
100 cycles on the same connections. Each cycle idles for 200 ms, schedules one
ping per connection, and waits for every reply before beginning the next idle
period. It does not become a continuous ping-pong throughput test.

All replies are timed from a shared timestamp immediately before scheduling the
activation tasks. This includes per-client scheduling delay, rather than
starting a separate stopwatch only after each coroutine begins. Each cycle
records time to the first reply, 50%, 95%, and all replies. Setup, teardown,
warmup, and actual idle durations are recorded separately and excluded from
activation latency. `--idle-timeout` bounds setup and each burst (default 30 s).

Each measured run uses a fresh process. Two loops run in AB/BA order across
blocks; larger loop sets rotate their starting position. Eight blocks give
balanced order for two loops. `--warmups` is not used for idle v2: configure
`--idle-warmup-cycles` instead. `--sustained` still ensures at least seven runs,
but does not override explicit cycle counts.

The console table reports the median across process runs of each run's median
cycle milestone. It also shows min/p10/p50/p90/max cycle-p95 latency to expose
fast/slow clusters. JSON preserves every cycle and every connection's reply
latency; these correlated samples are **not** treated as independent trials.

For comparison, each run contributes one value: its median cycle-p95 latency.
The reported effect is the geometric mean paired candidate/reference ratio,
expressed as a percentage latency change (negative is better). A deterministic
10,000-resample percentile bootstrap resamples whole process-run pairs, giving
an approximate 95% confidence interval. The reference is uvloop when present,
otherwise the first selected loop. Only this preselected metric determines the
classification; the other milestones are descriptive, not extra significance
tests.

- **Improved:** the entire interval is below -5%.
- **Regressed:** the entire interval is above +5%.
- **Inconclusive:** otherwise, or fewer than seven process runs. No confidence
  interval is reported for an insufficient sample. Inconclusive does not prove
  equivalence or stability; collect more independent runs if needed.

Avoid concurrent builds and CPU-heavy jobs. JSON records OS/Python information,
CPU count, effective affinity, process ID, and load averages before and after
each run where supported. `--cpu-affinity` accepts a comma-separated CPU set on
platforms with `sched_setaffinity`; it applies to the parent, children, and
their helper threads. Choose the same suitable multi-core allocation for both
loops. The harness does not change governors, disable host services, or claim
that affinity alone eliminates interference. Confidence intervals are not
proof against systematic host drift; repeat paired invocations.

Idle v2 is explicitly versioned and **not comparable** to the old single-burst
ops/s row. `check_regression.py` rejects mixed versions or different idle
settings. For two matching v2 files it independently bootstraps process runs
(separate invocations are not paired), checks the latency threshold, and does
not apply the legacy throughput/RSS gate to idle. Its exit codes are 0 for a
passed gate, 1 for a regression/incompatible data, and 2 for an inconclusive
idle result with no other failures. Old non-idle regression checks are unchanged.

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
