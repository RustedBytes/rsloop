# Embedded vibeio performance comparison — 2026-09-06

The task-polling optimization reduces median elapsed time by 18% for repeated
self-wakes and 25% for batches of ready tasks on this host. A second run of the
unchanged baseline supports improvements of 18% and 22%, respectively. Network
throughput is essentially unchanged. These are local measurements, not a claim
of equivalent application-wide speedups.

## Analysis and change

`vibeio` is embedded at `src/vibeio`, rather than built from
`vendor/vibeio/Cargo.toml`. The latter path in the benchmark instructions was
stale. The new `cargo bench --bench runtime` target compiles the embedded source
directly and restores a runnable scheduler benchmark.

The executor already batches up to 256 tasks, reuses its batch within `block_on`,
coalesces queued wakes, and provides a single-task wake slot. Its timer uses an
indexed four-ary heap. Changing those policies would affect fairness or timer
behavior; this change targets an independent cost inside every spawned-task
poll instead.

Previously, `Task::waker()` cloned the task's `Arc` to build an owned waker, which
was dropped after polling. `Task::waker_ref()` now borrows the existing task
reference through `futures_util::task::WakerRef`, removing that atomic increment
and decrement. The return lifetime is tied to the task reference; the wrapper
suppresses destruction of the borrowed waker. Futures that clone the waker still
acquire an owned reference through the existing vtable. The cancellation path
continues to use an owned waker.

Python ready callbacks run through `LoopCore`'s own dispatch path. This explains
why scheduler microbenchmark gains need not translate to callback throughput.
Further profiling candidates are repeated `block_on` setup (batch allocation
and root notification ownership), task allocation during spawning, and the
Python/native I/O handoff. They were not changed or established as bottlenecks
by these measurements.

## Method

- Baseline: commit `fc4e456`, with only the new benchmark target added.
- Candidate: the same source with the borrowed task-waker change.
- Host: Intel Core i9-9900K, Linux `7.0.0-31-generic`, Rust `1.97.1`, CPython
  `3.14.0`; release builds, fat LTO, one codegen unit, profiler disabled.
- Rust benchmark: CPU 2, three warmups and seven measured samples per workload.
  Each sample includes runtime creation and teardown. The automatic driver and
  timer-enabled rsloop scheduler profile are used; these workloads perform no
  socket I/O. Operations count spawned-task polls, including completion polls.
- Python benchmarks: CPUs 2–3, two warmups and seven measured samples; native
  fast streams enabled. The workload matrix uses 16 concurrent connections and
  500 requests per connection.
- Benchmarks ran without concurrent builds or tests. Other host services were
  running; CPU frequency and host load were not controlled. No confidence
  intervals were calculated. A saved baseline Rust binary was rerun immediately
  after the candidate to check temporal drift.

## Results

Rust elapsed time: lower is better. Percentages compare the candidate with the
initial baseline; the last column shows the subsequent unchanged-baseline run.

| Workload | Before median | After median | Elapsed change | Baseline recheck |
| --- | ---: | ---: | ---: | ---: |
| Spawn/join 100,000 tasks | 24.381 ms | 22.333 ms | -8.40% | 22.711 ms |
| One task, 1,000,000 self-wakes | 50.719 ms | 41.639 ms | -17.90% | 50.476 ms |
| 256 tasks, 4,000 self-wakes each | 43.386 ms | 32.651 ms | -24.74% | 42.057 ms |

The spawn/join improvement shrinks to 1.66% against the recheck, so it is not
strong evidence of a repeatable improvement. Self-wake and batch improvements
remain 17.51% and 22.37% against that recheck.

Python throughput: higher is better. These are observed changes, not all
attributable to the Rust optimization.

| Workload | Before ops/s | After ops/s | Throughput change |
| --- | ---: | ---: | ---: |
| 200,000 callbacks | 4,191,082 | 4,364,309 | +4.13% |
| 50,000 tiny Python tasks | 534,241 | 579,877 | +8.54% |
| 10,000 TCP echo round trips, 1 KiB | 58,167 | 58,223 | +0.10% |
| Concurrent HTTP keepalive | 53,758 | 53,538 | -0.41% |
| Concurrent mixed streams | 41,627 | 42,003 | +0.90% |

HTTP p95/p99 latency changed by +0.85%/+4.81%; mixed-stream p95/p99 changed by
-4.30%/-7.67%. Peak RSS changed by at most +0.57%. Both existing regression
gates passed (3% throughput, 5% latency, 5% RSS budgets). Callback and Python
task gains should be treated as inconclusive because this change targets
spawned Rust tasks and the measurements were sequential on a shared host.

## Reproduction and retained measurements

Run the following on both versions, changing the output suffix:

```bash
cargo bench --bench runtime --no-run
taskset -c 2 cargo bench --bench runtime > /tmp/runtime-before.csv
.venv/bin/maturin develop --release
taskset -c 2,3 .venv/bin/python benches/compare_event_loops.py \
  --loops rsloop --warmups 2 --repeat 7 --callbacks 200000 \
  --tasks 50000 --tcp-roundtrips 10000 \
  --json-output /tmp/python-before.json
taskset -c 2,3 .venv/bin/python benches/workload_matrix.py \
  --loops rsloop --scenarios http_keepalive,mixed_streams \
  --warmups 2 --repeat 7 --requests-per-connection 500 \
  --json-output /tmp/matrix-before.json
```

The session's raw CSV/JSON measurements, saved baseline/candidate Rust binaries,
baseline Python extension, and test logs are retained locally under
`target/vibeio-bench/` (ignored by Git). The Rust CSV files are
`runtime-before.csv`, `runtime-after.csv`, and `runtime-before-recheck.csv`;
Python results are `python-{before,after}.json` and
`matrix-{before,after}.json`. Compare Python files using
`benches/check_regression.py`.

Validation: 157 Rust tests passed. The Python compatibility, stream-reader,
run, and public-API suites ran 79 tests: 78 passed and one skipped. New Rust tests
cover borrowed/owned reference counts, cloned-waker lifetime, remote waking,
panic unwinding, and actual spawned-task resumption from another thread.
`cargo fmt --all -- --check` and `git diff --check` passed. Windows and macOS
were not tested in this session.
