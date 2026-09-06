# Embedded vibeio performance comparison — 2026-09-06

## Cleanup update: in-place timer waiter updates

Pending Sleep repolls now update a live timer's waiter in place instead of
canceling and reinserting its heap entry. Unchanged wakers avoid cloning as well.
The new `cargo bench --bench timer --locked` target measures this path directly.

| Workload | Before median (ms) | After median (ms) | Elapsed reduction | Speedup |
| --- | ---: | ---: | ---: | ---: |
| One timer, unchanged waker | 52.132 | 17.520 | 66.4% | 2.98× |
| One timer, changing waker | 51.081 | 27.080 | 47.0% | 1.89× |
| 1,024 timers, unchanged waker | 146.206 | 17.320 | 88.2% | 8.44× |
| 1,024 timers, changing waker | 144.017 | 27.334 | 81.0% | 5.27× |

Both binaries use the same current source except that the baseline disables
Sleep's in-place update branch, retaining the previous cancel/reinsert path.
The optimized branch was restored in the worktree after building the baseline.
Build command: `cargo bench --bench timer --locked --no-run`, Rust 1.98.1,
optimized default features. Saved binaries were run before/after three times
using `taskset -c 2`, with no concurrent builds or tests. Each run discards three
warmups and measures seven samples per workload; the table pools 21 samples per
version. Raw outputs: `target/vibeio-timer-{before,after}-{1,2,3}.csv`.

Single-timer samples perform 1,000,000 pending repolls; heap samples perform
1,000 rounds over 1,024 timers (1,024,000 repolls). All timers share a distant
deadline. Changed-waker cases alternate between two distinct wakers every round.
Elapsed time includes runtime/timer setup and teardown. The mock I/O driver is
used and no deadlines expire, so this isolates registration maintenance rather
than OS waiting, timer firing latency, or overall application throughput.

Per-pair elapsed reductions were 65.8–66.8%, 45.7–47.1%, 87.6–88.2%, and
80.9–81.1% in table order. CPU frequency and other host activity were not
controlled, and no confidence intervals were calculated. The shared-deadline
heap is a targeted stress workload, not an application workload distribution.
These results do not imply Python or uvloop speedups.

## Cleanup update: checked local ready queue

The local ready queue now uses a directly owned `RefCell<VecDeque<_>>` instead
of `Rc<UnsafeCell<VecDeque<_>>>`. This removes three raw-pointer access sites
and an unnecessary shared allocation. It adds runtime borrow checking; it is
a safety cleanup, not an established speed improvement.

The following compares only that queue change, with the earlier safe-waker
changes present in both binaries. Values are median elapsed milliseconds:

| Workload | Before 1 | After 1 | Before 2 | After 2 |
| --- | ---: | ---: | ---: | ---: |
| Spawn/join | 24.468 | 26.563 | 24.377 | 24.575 |
| Single-task yield | 43.162 | 44.053 | 42.472 | 42.776 |
| Batch yield | 34.142 | 33.843 | 33.195 | 33.123 |

Built with `cargo bench --bench runtime --locked`, Rust 1.98.1. Saved the original
binary, then alternated original/candidate/original/candidate on CPU 2 using
`taskset -c 2`, without concurrent builds or tests. Each binary discards three
warmups and reports seven samples per workload. Raw outputs are
`target/vibeio-queue-pinned-{before,after}-{1,2}.csv`. CPU frequency and other
host activity were not controlled; there are no confidence intervals.

Single-task yield medians increased by 2.1% and 0.7%; spawn/join varied more
(8.6% and 0.8% increases), while batch yields decreased slightly. These short
runs do not establish equivalence or a stable regression magnitude. An initial
unpinned before/after/baseline-recheck run was also mixed and is retained in
`target/vibeio-queue-{before,after,baseline-recheck}.csv`. No Python or uvloop
performance conclusion follows from these scheduler-only measurements.

## Cleanup update: local task ownership

After separating thread-safe wake proxies from local tasks, task ownership now
uses `Rc`; only wake proxies use `Arc`. This removes atomic reference counting
from local ready queues and join-handle task references. Cross-thread final waker
release cannot destroy local futures, and proxy identity rejects stale wakes.

The following compares the worktree immediately before/after **only this Rc
conversion**, with the wake-proxy safety fix present in both versions:

| Scheduler workload | Arc median (ms) | Rc median (ms) | Lower elapsed time |
| --- | ---: | ---: | ---: |
| Spawn/join | 26.603 | 24.680 | 7.2% |
| Single-task yield | 48.714 | 43.681 | 10.3% |
| Batch yield | 41.286 | 33.319 | 19.3% |

Command: `cargo bench --bench runtime --locked`, Rust 1.98.1, optimized default
features, same local Linux host. Each workload discards three warmups and reports
seven samples. Raw local outputs are `target/vibeio-local-arc-before.csv` and
`target/vibeio-local-rc-after.csv`. This is one sequential, unpinned comparison;
it does not establish statistical significance, the total impact of the earlier
wake-proxy redesign, Python performance, or a comparison with uvloop.

## Earlier borrowed-waker comparison (historical)

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
