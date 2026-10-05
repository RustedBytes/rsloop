# Profiling rsloop with hotpath-rs

The opt-in `profile` feature instruments rsloop's Rust function bodies
across the Python bindings, callback/context/timer machinery, dispatcher,
transports, TLS, subprocesses, socket operations, and embedded vibeio runtime.
This includes the platform drivers and optional runtime modules. Normal builds
compile these hooks out and do not link hotpath.

Reports default to JSON with **unlimited rows** for functions, futures, and
threads. Function entries include source locations, call/sample counts, and
p50/p95/p99 durations. Async functions use `future = true`, adding poll counts
and poll durations. A separate `hotpath-alloc-profile` build counts Rust
allocations through hotpath's global allocator.

## Collect a coverage suite

Use the project interpreter (`.venv/bin/python` on Unix,
`.venv/Scripts/python.exe` on Windows). Build first, then profile; do not compile,
run tests, or run another benchmark while collecting profiles.

```bash
.venv/bin/python scripts/generate_test_tls_certs.py tests/fixtures/tls
.venv/bin/maturin develop --release --features profile
.venv/bin/python scripts/profile_with_hotpath.py all --output target/hotpath-rs/timing
```

`all` runs each of these workloads in its own fresh process:

| Area | Workloads |
| --- | --- |
| Scheduling | callbacks, tasks, task options, retained/discarded/cancelled timers, mixed future deadlines |
| TCP and application protocols | native echo, generic protocol connection churn, HTTP, TLS HTTP, WebSocket framing with/without TLS, mixed messages, bulk transfer, idle connections |
| Other runtime paths | thread-safe/context callbacks, executor/DNS, UDP sockets, raw TCP sockets, subprocess pipes, Unix streams, POSIX signals, cancellation |

The probes check their results. Unix/signal probes are explicitly skipped on
unsupported platforms. TLS needs the generated local certificates. A failed or
timed-out child makes the overall command fail, while preserving other reports
and failure logs. Each child has a timeout. Existing outputs are not overwritten.

Each output directory contains:

- `<workload>.json`: the full upstream hotpath report, without a top-50 cutoff.
- `<workload>.metadata.json`: the actual extension path/hash, build features,
  interpreter/platform, workload parameters, effective profiler environment,
  outcome and instrumented duration including report flushing.
- `<workload>.log`: child output/errors, including failed runs.
- `manifest.json`: commands, statuses, checkout revision, dirty state, and source
  hashes. These hashes describe the checkout, **not proof that the installed
  extension was built from it**. Rebuild after editing Rust; the binary hash
  identifies what actually ran.
- `coverage.json`: the source inventory with an `observed` flag for every
  definition, exclusion reasons, and any observed names the inventory cannot map.
- `summary.md`: per-workload counts and links to the reports.

Profiles include cold loop setup, execution, and teardown. There is no in-process
warmup: hotpath's future/thread registries are process-global, so warming up in
the same process can contaminate their statistics. The private native API allows
one start/stop session per process and rejects attempts to restart it.

The collector clears inherited `HOTPATH_*` settings before importing rsloop so
old filters, row limits, or output paths cannot silently change the result.
Effective settings are recorded. `--focus` explicitly filters function timing;
future/thread rows can still describe other work. `--time-sampling-rate` is in
`(0, 1]` and defaults to 1. A sampled report retains call counts and sampled-call
counts; estimates are not equivalent to timing every call.

## Focus on a path or run your application

```bash
.venv/bin/python scripts/profile_with_hotpath.py tcp_streams \
  --output target/hotpath-rs/tcp.json --tcp-roundtrips 5000
.venv/bin/python scripts/profile_with_hotpath.py tls_http \
  --output target/hotpath-rs/tls.json --focus tls --time-sampling-rate 0.1
.venv/bin/python scripts/profile_with_hotpath.py module \
  --output target/hotpath-rs/tls-tests.json --module pytest \
  --module-args -q tests/test_tls.py
.venv/bin/python scripts/profile_with_hotpath.py module \
  --output target/hotpath-rs/application.json --module your_application \
  --module-args --your-option value
```

Put `--module-args` last. Applications must select rsloop themselves. Modules
run inside the same profiling session, and their failures propagate after the
report is flushed. Use `--format table` for a single human-readable report;
`all` requires JSON for coverage analysis. Parameters such as `--timers`,
`--connections`, `--concurrency`, `--bulk-bytes`, and `--probe-iterations` let you
extend short probes into representative experiments.

For allocations, use a separate build and fresh directory:

```bash
.venv/bin/maturin develop --release --features hotpath-alloc-profile
.venv/bin/python scripts/profile_with_hotpath.py all --output target/hotpath-rs/allocations
.venv/bin/maturin develop --release
```

Allocation mode changes the function report section from timing to allocation
statistics. With the collector's default environment, these are **exclusive
allocation bytes**, excluding nested instrumented calls; check the report's
`description` field. They are not inclusive like function timing. It does not
account for CPython's allocator, native libraries that
allocate outside Rust's global allocator, or explicit `System` allocations.
It measures allocation traffic, not retained heap size or peak RSS. Use the
normal benchmark harness for peak RSS. Always rebuild without profiling before
comparing performance.

Instrumentation can also introduce allocations: hotpath 0.28.3 creates an
`Arc<AsyncAllocBridge>` for each measured async invocation in allocation mode.
Those allocations can appear in an enclosing rsloop function. Check the locked
profiler implementation and repeat with focused instrumentation before treating
small per-await allocations as production costs.

## Keep coverage from drifting

```bash
cargo run --locked --manifest-path tools/hotpath-coverage/Cargo.toml
cargo run --locked --manifest-path tools/hotpath-coverage/Cargo.toml -- --json
# Add attributes for newly introduced functions, then review and format:
cargo run --locked --manifest-path tools/hotpath-coverage/Cargo.toml -- --instrument
cargo fmt --all
```

The audit parses Rust syntax, including inactive platform/feature branches,
and rejects missing function hooks or async functions without future tracking.
It excludes tests, verification/mock helpers, const functions, profiler control,
ABI callbacks, and known signal-safe/post-fork code. It also rejects hooks added
to these excluded function definitions. Signal-handler call chains require
manual safety review when changed: a syntax audit is not call-graph analysis.
The standalone vibeio harness accepts the profiling cfg but keeps hooks disabled.
`scripts/run_rust_tests.py` gives profiling-feature test threads a 32 MiB default
stack: unoptimized nested async wrappers can otherwise exceed libtest's 2 MiB
stack when moving owned I/O arrays. It preserves an explicit `RUST_MIN_STACK`.
For direct debug `cargo test` with profiling, set `RUST_MIN_STACK=33554432` too.
Use release builds for profile collection.

The inventory covers explicit functions/methods, not macro-generated code or
anonymous closures/async blocks as independent rows. Their executed work is
included in enclosing spans or instrumented callees; instrument a specific
boundary when finer attribution is needed. External dependencies, PyO3-generated
wrappers, Python frames, and kernel internals are not individually instrumented.

**Unobserved does not mean cheap or unused.** The inventory includes Windows,
macOS, Linux, optional Cargo features, error branches and APIs a particular
workload never calls. Run representative applications, targeted test modules,
and native-platform builds to fill those gaps. Do not describe a Linux smoke
suite as 100% execution coverage.

Function times are inclusive wall durations: nested calls overlap, blocking
socket waits are not CPU work, and parallel workers can sum beyond wall time.
Future poll durations help distinguish execution from suspension; thread CPU
samples are coarse and short-lived threads may not be sampled. Do not add nested
rows or call them self time. Broad instrumentation substantially perturbs tiny
functions. Use the profiles to choose a hypothesis, then validate it on normal
release builds with [paired holdouts](hotpath-lab.md).

The implementation follows hotpath's [function/future instrumentation](https://hotpath.rs/functions)
and [report configuration](https://hotpath.rs/configuration). The locked hotpath
version and report format are recorded in the raw report.

## Coverage check on October 4, 2026

On Linux x86-64, CPython 3.14.7, Rust 1.100.0-nightly and hotpath 0.28.3,
all 23 default workloads completed in both timing and allocation builds.
The syntax audit found 1,995 instrumented definitions with 1,959 distinct names
across all platform/feature branches, plus 27 intentionally excluded definitions.
Timing workloads observed 789 names; allocation workloads observed 788. Every
observed rsloop function mapped back to the inventory, and reports were untruncated.

| Timing workload | Function rows | Future rows |
| --- | ---: | ---: |
| Callbacks | 170 | 0 |
| Native TCP streams | 494 | 9 |
| TLS HTTP | 538 | 3 |
| Subprocess pipes | 350 | 0 |

Function row counts include the session wrapper. Counts can vary with scheduling
and workload sizes. These are coverage observations, not evidence of a speedup.
The local ignored artifacts are under `target/hotpath-rs/full-timing/` and
`target/hotpath-rs/full-allocations/`, each with a summary, provenance, and raw
reports. They describe the working-tree implementation, not an immutable release.
The `Hotpath profiling coverage` workflow repeats a smaller suite in both modes
and uploads the reports; the main test workflow invokes it too.

## Performance investigation at `0e0c8d0`

On October 4, 2026, a fresh investigation used the clean committed source on
the same Linux/CPython/toolchain platform above. All **23 workloads passed in
both timing and allocation modes**, observing 800 distinct rsloop function
names in each mode, with no unmapped names or truncated reports. No production
code was changed in this investigation.

Raw reports, binary hashes, commands, and coverage are retained locally under
`target/hotpath-rs/performance-0e0c8d0/`. The `timing/` and `allocations/`
directories contain the full suites; `focused/` contains three independent
timing processes per selected path, and `allocations-focused/` contains a
profiler-overhead check. These ignored artifacts are not shipped with the repo.

For each profiling build, the full-suite command was:

```bash
.venv/bin/python scripts/profile_with_hotpath.py all \
  --output target/hotpath-rs/performance-0e0c8d0/timing \
  --callbacks 200000 --tasks 50000 --tcp-roundtrips 5000 \
  --timers 50000 --connections 500 --probe-iterations 200 \
  --concurrency 8 --requests-per-connection 200 --bulk-bytes 16777216 \
  --idle-connections 100 --idle-cycles 50
```

Use a fresh output directory and change `timing` to `allocations` after building
the allocation feature. Task/timer batches remained at 500 and TCP payloads
at 1 KiB. Bulk transfer received 16 MiB on each of eight connections.

### Focused timing findings

Each focused run used `--focus` with the function name below and the same
relevant workload parameters. Values are median function totals across three
fresh processes; shares use the corresponding profiled session duration.

| Workload | Focus | Median total (range) | Median session share |
| --- | --- | ---: | ---: |
| 200,000 callbacks | `schedule_callback_args` | 36.95 ms (36.87–37.42) | 33.60% |
| 50,000 retained timers | `LocalTimers::collect` | 12.60 ms (12.19–12.69) | 24.58% |
| 50,000 cancelled timers | `LocalTimers::collect` | 12.68 ms (12.60–12.73) | 27.07% |
| 128 MiB bulk receive | `ExactReadAccumulator::fill_from` | 38.46 ms (37.88–38.98) | 50.86% |
| 5,000 TCP roundtrips | `try_direct_tasked_write` | 46.22 ms (45.56–48.80) | 36.35% |

These remain instrumented wall times, not production self time or achievable
speedups. Filtering leaves instrumentation checks and future tracking in the
binary. In the broad timer profiles, collection took about 51 ms; filtering
comparison hooks reduced that to about 12.6 ms. The broad profiles counted
436,400 heap comparisons for retained timers and 440,100 for cancelled timers.
The normal *whole timer workload* took only about 17–18 ms, so interpreting
the broad profile literally would substantially overstate this cost.

### Recommended experiments, in order

1. **Size initial pooled writes to the request.** In 500 generic TCP
   connection/echo/close cycles, read pools allocated 31.2 MiB and write pools
   15.6 MiB, about 46.9 MiB combined, for 1 KiB messages. The write-pool row
   records 1,000 acquisitions averaging 16 KiB.
   `WriteBufferPoolState::acquire` in `src/transport/stream/buffers.rs` floors
   allocations at `WRITE_BUFFER_BLOCK_SIZE` (16 KiB). Test a smaller initial
   capacity that grows when needed while preserving the existing pool-slot
   limit and write-buffer accounting. Reducing those 1,000 initial allocations
   from 16 KiB to 1 KiB would remove about 14.6 MiB of allocation traffic in
   this probe; that is a capacity estimate, not a measured speed or RSS gain.
   Validate short connections, HTTP/TLS, mixed message sizes, bulk transfer,
   partial writes, backpressure, and close ordering. Change read-buffer sizing
   separately because smaller reads can increase syscall frequency.
2. **Reduce large exact-read copying if a safe design pays off.**
   `ExactReadAccumulator::fill_from` in `src/transport/stream/fast.rs` remains
   expensive with focused instrumentation. It already copies directly into a
   single final Python bytes allocation, so removing a temporary accumulator
   would not help. Explore receiving directly into the private result buffer
   for large pending exact reads, or an internal owned-buffer handoff that
   avoids the current copy. This is a substantial ownership/lifetime change:
   preserve cancellation, partial EOF, transport shutdown, Python attachment,
   and the rule that uninitialized bytes must never be exposed. Compare
   several payload sizes and concurrency levels; the measured span includes
   memory writes and possible page faults, not just instruction overhead.
3. **Reduce timer allocation and heap work.** `schedule_timer` accounted for
   about 3.5 MiB of exclusive Rust allocations per 50,000 timers. The source
   creates an `Arc<ReadyCallback>` per timer; cancellation still leaves heap
   entries to pop and release through the ready queue. Candidate designs
   include reusable timer storage with generation-checked handles, or
   cancellation-aware heap rebuilding. Keep equal-deadline order, finalizer
   reentrancy, retained handles, and `when()` semantics. These probes use
   zero-delay timers; add mixed future deadlines and cancellation densities
   before selecting a data structure.
4. **Investigate callback object/queue costs at finer boundaries.** Scheduling
   remains material, but `schedule_callback_args` includes context capture,
   handle creation, and queue insertion. The 200,000-callback burst records
   12.0 MiB of allocation traffic in `try_enqueue_local_ready`; p50/p95/p99
   are all zero, consistent with occasional `VecDeque` growth, not a Rust
   allocation per callback. The 50,000-task workload with bounded batches
   records only 29.9 KiB there. Separate large-burst capacity growth from
   steady-state scheduling, and add targeted spans around Python handle
   creation before choosing a layout change. Rust allocation reports do not
   measure CPython object allocations. The previous handle-freelist experiment
   already rejected that approach for large bursts.

The first experiment has the most concrete, limited change to test. The others
need more design work. Every candidate still requires an uninstrumented paired
comparison using [the holdout workflow](hotpath-lab.md).

### Costs that should not drive a change yet

- The TCP reader future reported **640,128 bytes in 20,004 allocations**.
  There were 10,002 calls each to `acquire_read_buffer_async` and
  `wait_until_async_readable`; each measured async call creates a 32-byte
  hotpath allocation bridge on this platform. Repeating with
  `--focus '/reader_task::run_tcp_socket_reader_task|::buffers::|::core_events::/'`
  reduced the reader-task allocation row and its poll allocation count to
  **zero**, while the read-pool row stayed at 96 KiB. Do not optimize away
  rsloop awaits to fix these profiler allocations.
- In bulk transfer, `ReadBufferPool::wait_async` spans totalled 429.07 ms
  across connections, but its future polls totalled only **0.324 ms**. The
  span primarily describes suspension/backpressure, not expensive CPU work.
  TLS socket polling similarly overlaps across worker threads.
- `try_direct_tasked_write` includes a mutex and the socket write. Its timing
  does not establish mutex contention. Existing write coalescing reduced
  4,800 application writes to 3,200 direct-write attempts in HTTP; additional
  batching must be justified against request latency.
- Runtime batch vectors still allocate 2 KiB per entry, but enabling the
  existing cache is not an established fix: the
  [earlier cache experiment](allocator-batch-results.md) confirmed a TCP
  regression. Revisit only with a different hypothesis and fresh comparisons.

### Normal release baseline

After restoring `.venv/bin/maturin develop --release`, nine workloads ran in
five fresh measured processes each, with one discarded process per workload
beforehand. Workload order was shuffled within each measured block with seed
41026; GC was disabled. The local `normal_baseline.py` calls the same existing
benchmark functions as the collector. `normal-baseline/` retains its plan,
source hashes, extension hash/features, raw samples, CPU time, and peak RSS.
The binary was verified to have both profiling features disabled and no
`hotpath_start` entry point. No compilation or competing benchmark ran during
collection; CPU affinity was unrestricted.

| Workload | Median workload time | Observed range |
| --- | ---: | ---: |
| Callbacks | 44.01 ms | 43.32–44.24 ms |
| Tasks | 85.70 ms | 84.21–86.26 ms |
| Retained timers | 17.81 ms | 17.57–18.87 ms |
| Cancelled timers | 17.11 ms | 16.91–17.17 ms |
| TCP streams | 79.31 ms | 78.31–79.92 ms |
| TCP connection churn | 83.23 ms | 82.39–84.23 ms |
| HTTP keep-alive | 31.60 ms | 31.53–31.85 ms |
| TLS HTTP | 55.29 ms | 53.27–55.49 ms |
| Bulk transfer | 61.65 ms | 60.65–62.54 ms |

These are diagnostic baselines, not candidate comparisons. They use each
benchmark's own timed region, whereas hotpath sessions also include cold
loop setup/teardown and other wrapper work. Five short samples are not the
paired holdout gate and do not establish any optimization speedup.

## Profile-guided implementation

The implementation on `perf/profile-guided-optimizations`, based on `0e0c8d0`,
keeps two production changes:

- Pooled write buffers start at the requested capacity with a 256-byte minimum
  for tiny headers, instead of a 16 KiB minimum. Growth, pool-slot limits,
  oversized-buffer rejection, and backpressure accounting are unchanged.
- Loop-thread zero-delay timers use an ordered FIFO alongside the timer heap.
  Future deadlines and out-of-order insertions use the heap. The next timer is
  selected by the same deadline/sequence ordering across both containers.
  Pending timers outside the running loop retain their original heap, and
  stopping a loop transfers outstanding entries back to it. Callback release
  still happens outside timer borrows/locks. Per-timer `Arc` allocation and
  Python handle/cancellation semantics are unchanged.

The new `timers_mixed` workload schedules scattered future deadlines and cancels
them before expiry. It is available in both profiling and holdout tools, and
brings the profiling collector's default suite to 24 workloads. This probe
exposed extra work in an initial design that tried FIFO insertion for all
deadlines; the retained fast path is specifically for zero-delay timers.

No bulk-read implementation change is retained. One trial fed fragments directly
into the private result bytes and returned their buffers to the pool earlier;
the combined candidate's bulk-transfer time increased 2.36% (interval
+0.26% to +3.96%). Another preserved pool recycling and deferred Future-reference
cloning until completion, but did not establish a bulk gain; its combined
candidate also regressed TCP time. These were combined experiments, not isolated
attribution of each slowdown to the read code. Both trials were reverted.
Regression tests for fragmented exact reads, overflow, partial EOF, exceptions,
cancellation, and a real large transfer with trailing bytes remain.

Correctness validation passed 411 Rust tests with all features, 230 Python tests
(three skipped on the normal build), and 94 tooling tests. New Rust tests compare
random insertion, removal, and stop/start heap transfers against `BinaryHeap`,
and check that small pooled writes grow without losing data and reuse storage.

Local benchmark artifacts are under `target/profile-guided-optimizations/`.
The `baseline/` and `final/` directories preserve the exact normal-release
packages and binary hashes; `final/source.patch` records the Rust change from
the base commit. Each comparison archives its runner/workloads, predeclared
parameters/order/seed, raw fresh-process samples, and paired bootstrap results.
`paired-12/` and `confirmation-12/` preserve the rejected intermediate trials;
`final-12/` measures the retained implementation. No profiling, builds, or tests
ran concurrently with these comparisons. CPU affinity was unrestricted.

### Retained implementation: paired measurements

The final comparison used 12 balanced, randomized baseline/candidate process
pairs per workload (seed 41029), with one full warmup and one measured workload
per process, GC disabled, and uninstrumented release binaries. Every measured
workload exceeded 0.25 seconds. Parameters were 1.5 million callbacks, 250,000
tasks, 1.2 million timers, task/timer batches of 777, 25,000 TCP roundtrips with
8 KiB payloads, 2,500 connection cycles, eight concurrent connections with
4,000 requests each, and 128 MiB per bulk connection.

Changes below are geometric means of **paired** time ratios, not ratios of the
displayed medians. The intervals use 20,000 bootstrap resamples and a 99.79%
confidence target, adjusting across 12 timing and 12 RSS metrics.

| Workload | Baseline median (ms) | Candidate median (ms) | Paired time change (interval) |
| --- | ---: | ---: | ---: |
| Callbacks | 323.79 | 328.44 | +0.21% [-3.98%, +2.39%] |
| Tasks | 417.06 | 420.27 | +0.51% [-0.69%, +1.64%] |
| Retained zero-delay timers | 424.57 | 394.28 | -7.30% [-7.87%, -6.62%] |
| Discarded zero-delay timers | 389.26 | 353.88 | -8.37% [-9.98%, -6.86%] |
| Cancelled zero-delay timers | 412.40 | 377.56 | -8.74% [-9.98%, -7.68%] |
| Mixed future deadlines | 513.34 | 516.53 | +1.05% [+0.17%, +2.12%] |
| TCP streams | 484.69 | 483.39 | -0.10% [-1.88%, +1.83%] |
| TCP connection churn | 428.56 | 434.17 | -0.06% [-2.97%, +2.23%] |
| HTTP keep-alive | 578.02 | 594.49 | +2.55% [+1.54%, +3.89%] |
| TLS HTTP | 331.98 | 337.70 | +0.83% [-2.33%, +4.21%] |
| Mixed streams | 684.33 | 678.48 | -0.34% [-3.25%, +2.46%] |
| Bulk transfer | 444.10 | 445.81 | +0.30% [-1.62%, +2.06%] |

Connection-churn peak RSS decreased **9.48%**, with an interval of
[-9.55%, -9.41%]. Timer RSS was unchanged at the process peak measurement's
resolution. The mixed-deadline probe showed a small timing increase within the
3% budget; this change is a zero-delay optimization, not an improvement to every
timer distribution.

The broad balanced gate is **inconclusive**: HTTP/TLS timing and some network
RSS upper bounds exceed the 3% regression budget. A separately predeclared
32-pair follow-up for HTTP, TLS, and bulk transfer (seed 41030, same sizes,
99.17% intervals across its six metrics) produced:

| Workload | Time change (interval) | Peak RSS change (interval) |
| --- | ---: | ---: |
| HTTP keep-alive | +1.17% [-1.07%, +4.23%] | +0.31% [-0.97%, +1.62%] |
| TLS HTTP | +0.35% [-1.70%, +2.34%] | +0.13% [-0.95%, +1.30%] |
| Bulk transfer | -0.61% [-1.36%, +0.03%] | +0.92% [-1.49%, +3.53%] |

The follow-up resolves the TLS bounds but leaves HTTP timing and bulk RSS
uncertain against that budget. It does not turn the original gate into a pass.
The supported gains are faster zero-delay timers and lower connection-churn
memory use; a general network speedup or absence of all regressions is not
established. Follow-up artifacts are in `network-32/`. The final baseline and
candidate extension hashes are respectively
`be2ada747687c4a05bcee520e03a5640b74225d1ab64e7596f4d178c6ef78c29`
and `05a273296b2a2bd959ad84f53ec949aa4a70fa488fd29fcd347af484e959b7bc`.

### Hotpath verification of the retained changes

Fresh profiles of the final source are retained in `timing/` and `allocations/`
under the implementation artifact directory. The syntax audit covers 2,029
definitions (2,002 instrumented and 27 excluded), with no missing hooks. The
profiling lifecycle Python test also passed on the allocation build.

For 50,000 timers in batches of 500, the measured `TimerEntry::partial_cmp`
call counts changed as follows relative to the earlier `0e0c8d0` profiles:

| Workload | Before | After | Reduction |
| --- | ---: | ---: | ---: |
| Retained zero-delay timers | 436,400 | 150,000 | 65.6% |
| Cancelled zero-delay timers | 440,100 | 150,300 | 65.8% |

The 500-connection, 1 KiB echo probe still made 1,000 pooled write acquisitions,
but their allocation traffic fell from 16,384,000 to 1,024,000 bytes: **93.75%
less**, or about 14.65 MiB saved. Read-pool traffic remained about 31.2 MiB.
These are Rust allocation requests, not resident-memory measurements; the RSS
result above comes from separate uninstrumented processes with 8 KiB messages.
Timing profiles for retained, cancelled, and mixed-deadline timers, and
allocation profiles for connection churn, TCP streams, and mixed timers all
completed successfully. The normal release binary was restored afterward.

## Historical selective experiment

The figures below predate the broad instrumentation and current collector.
They are retained as historical observations, not measurements of this build.

The observations below are from commit `aa40fec` plus the profiling changes, on
an Intel Core i9-9900K, Linux x86-64, CPython 3.14.7, Rust
1.100.0-nightly, and hotpath 0.28.3. Each workload was run in a fresh process.
The benchmark measured 200,000 callbacks, 50,000 tasks, or 5,000 TCP echo
roundtrips with 1 KiB payloads. Figures are from one exploratory run, rounded
to one decimal place.

| Workload | Measured function | Calls | Inclusive time | Share of profiled wall time |
| --- | --- | ---: | ---: | ---: |
| Callbacks | `call_callback_noargs` | 200,001 | 69.9 ms | 49.7% |
| Callbacks | `schedule_callback_args` | 200,009 | 40.8 ms | 29.0% |
| Callbacks | `capture_context` | 200,009 | 7.8 ms | 5.5% |
| Tasks | `call_callback_onearg` | 50,015 | 58.7 ms | 34.1% |
| Tasks | `call_callback_noargs` | 100,001 | 52.4 ms | 30.5% |
| Tasks | `schedule_callback_args` | 150,018 | 22.8 ms | 13.3% |
| TCP | `try_direct_tasked_write` | 10,000 | 43.9 ms | 47.1% |
| TCP | `call_callback_onearg` | 10,019 | 38.2 ms | 41.0% |
| TCP | `drain_pending_read_events_with_py` | 10,003 | 10.5 ms | 11.2% |

These are **inclusive** measurements: a Python callback can call the Rust
write path, so the TCP callback and direct-write rows overlap. Nested rows
must not be added. Timing instrumentation also runs once per call, so it
perturbs these sub-microsecond paths substantially. The results identify
places to investigate; they do not establish an uninstrumented speedup.

The same workloads on the normal release build had medians of 49.1 ms for
callbacks, 88.3 ms for tasks, and 81.3 ms for TCP (five fresh measured
processes each, after two warmups). The instrumented single runs took 139.4,
170.7, and 91.9 ms respectively. These are sequential, unpaired observations,
but show that the profiler especially distorts the tiny callback paths.

### Experiments proposed from the historical profile

1. For callback-heavy workloads, break down `schedule_callback_args` further,
   particularly `Py::new` handle creation and reference ownership. That path
   consumed about 29% of the instrumented callback run. Keep the returned
   `asyncio.Handle` and cancellation behavior intact, then compare a candidate
   without instrumentation.
2. For TCP, test whether the 10,000 direct socket writes can be made cheaper
   without hurting one-message roundtrip latency or backpressure behavior. The
   function includes a mutex and the actual socket write, so its 43.9 ms does
   not identify which part dominates. The existing server-side write staging
   already coalesces writes within a loop turn; a batching change needs a
   representative throughput and latency comparison.
3. Do not prioritize replacing context handling or the dispatcher queue from
   these data. In the TCP run, context capture plus enter and exit were below
   1% of profiled wall time; dispatcher drains were below 0.1% in all three
   workloads. `PyContext` typing would not change the CPython calls.

Before adopting a performance change, use the uninstrumented benchmark suite
and the paired holdout workflow in `docs/hotpath-lab.md`. This profiling run is
only a guide to where a candidate is worth testing.
