# Profiling rsloop with hotpath-rs

The opt-in `hotpath-profile` feature instruments rsloop's Rust function bodies
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
.venv/bin/maturin develop --release --features hotpath-profile
.venv/bin/python scripts/profile_with_hotpath.py all --output target/hotpath-rs/timing
```

`all` runs each of these workloads in its own fresh process:

| Area | Workloads |
| --- | --- |
| Scheduling | callbacks, tasks, task options, retained/discarded/cancelled timers |
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
statistics. It does not account for CPython's allocator, native libraries that
allocate outside Rust's global allocator, or explicit `System` allocations.
It measures allocation traffic, not retained heap size or peak RSS. Use the
normal benchmark harness for peak RSS. Always rebuild without profiling before
comparing performance.

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

## Next experiments

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
