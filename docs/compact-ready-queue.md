# Compact ready queue: memory measurements

Measured 7 October 2026 against master `80c1a8b`, release builds of rsloop
0.1.58. This change reduces `ReadyItem` from 24 to 16 bytes on Linux x86_64
by boxing the multiword Future completion, accepted-connection and connect
completion payloads. A compile-time bound prevents any supported target from
silently growing the queue slot beyond two pointer words.

Callbacks, timers, stream-read/write notifications and stop ordering use the
same routing and Python reference ownership as before. The ordinary callback
queue entry does not allocate a Box. Context capture and PyHandle representation
are unaffected by this change.

## Callback results

1,000,000 callbacks per fresh process; controlled implicit empty/nonempty
contexts using the diagnostic benchmark from PR #98. Each variant has 14
measured samples: two rounds of 2 warmups + 7 samples, with before/after order
reversed in round 2. Table entries are pooled medians.

| Build/context | Peak RSS MiB, before → after | Total ms, before → after | Time range ms, before / after |
|---|---:|---:|---:|
| gil, empty | 110.75 → 103.11 | 188.45 → 195.18 | 178.24–231.48 / 178.45–236.00 |
| gil, nonempty | 171.98 → 164.40 | 216.77 → 202.81 | 202.76–260.42 / 196.40–232.82 |
| ft, empty | 131.99 → 124.35 | 217.47 → 214.85 | 205.97–242.19 / 204.04–277.83 |
| ft, nonempty | 193.24 → 185.68 | 228.70 → 225.76 | 221.67–248.83 / 218.36–249.88 |

The stable reduction is approximately 7.6 MiB per million live queued callbacks,
consistent with eight fewer bytes per queue slot. This saves real production
queue storage with either context workload. It does not remove the separate
Context allocations identified in #98. PyHandle already contains three Python
references, the full callback identifier, flags/cancellation state and weakref
support; no measured, semantics-preserving handle compaction is included here.

## Other workloads and allocation tradeoff

Each variant again has 14 measured fresh-process samples in two reversed-order
rounds. Tasks: 200,000 sleep(0) tasks in batches of 5,000. TCP streams: 20,000
round trips with 1,024-byte payloads using native fast streams. Connection churn:
1,000 separate stdlib-protocol TCP connections per sample, each exchanging a
1,024-byte payload. Churn specifically exercises the new accepted/connect boxes.

| Build/workload | Median ms, before → after | Range ms, before / after |
|---|---:|---:|
| gil, tasks | 342.27 → 328.21 | 325.34–367.25 / 314.01–365.22 |
| gil, tcp_streams | 163.51 → 161.66 | 159.35–186.35 / 156.81–196.03 |
| gil, tcp_connect_churn | 116.23 → 116.99 | 114.00–133.54 / 114.47–131.19 |
| ft, tasks | 384.55 → 397.28 | 372.19–437.89 / 370.94–465.39 |
| ft, tcp_streams | 160.85 → 161.26 | 157.05–170.16 / 155.00–170.85 |
| ft, tcp_connect_churn | 143.23 → 138.68 | 138.66–180.07 / 134.29–156.64 |

There is no uniform timing improvement. GIL empty-context callback median is
about 3.6% higher; FT task median about 3.3% higher. Ranges overlap on this shared
host, so these measurements do not establish either a universal speedup or an
absence of small regressions. The benefit demonstrated here is queue memory.
Connection-churn results do not show a consistent slowdown, but payload-heavy
queues now pay for one Box per multiword ready event. A failed local/active enqueue
can unpack the payload into a dispatcher command and box it again on delivery.
Those allocations are the explicit tradeoff; future-heavy workloads should be
checked on representative deployment hardware before merging.

## Validation

- Release builds completed for conventional and free-threaded CPython 3.14.0.
- `cargo check --locked` and `cargo clippy --locked --lib -- -D warnings` passed.
  Compile-time queue-size bound passed.
- FT GIL-off: 251 tests passed, 2 skipped; one pathname Unix-socket test fails
  with PermissionError in this environment, also reproduced on the stock baseline.
- Conventional build: 246 tests passed, 2 skipped, that blocked test deselected.
- Coverage includes callback cancellation and cross-thread scheduling, parallel
  loops, repeated loop lifecycles, ContextVars/task compatibility, native/stdlib
  streams, accept/connect, numeric/executor resolution, read/write ownership and
  reentrancy, timers and buffered delivery.
- One initial broad test invocation stalled; it was interrupted. The suspected
  test passed in isolation and the diagnostic rerun completed in 2.57 seconds.
  The intermittent stall was not isolated to a specific code change.

## Reproduction and raw data

All samples are in [compact-ready-queue.csv](benchmarks/compact-ready-queue.csv).
The host used Linux 6.18.44/glibc 2.39, x86_64, CPU affinity 0, rustc 1.99.0,
and uv CPython 3.14.0 distributions. FT runs used PYTHON_GIL=0. GIL/FT builds
have different headers/allocators; before and after use the same interpreter
within each comparison. GC is disabled for callback and core comparison probes;
connection churn uses the existing scheduler probe's normal GC behavior.

Build both commits with the same locked dependencies and release profile. Keep
separate package copies of their native extensions and use PYTHONPATH to select
one per fresh-process sample. Retrieve the diagnostic callback probe from PR #98:

```bash
git show origin/perf/free-threaded-callback-rss:benches/callback_rss.py > target/callback_rss.py
PYTHONPATH="$PWD/target/before/python" taskset -c 0 target/bench-gil/bin/python target/callback_rss.py --context empty --repeat 7 --warmups 2 --output target/before-empty.json
PYTHON_GIL=0 PYTHONPATH="$PWD/target/after/python" taskset -c 0 target/bench-ft/bin/python target/callback_rss.py --context nonempty --repeat 7 --warmups 2 --output target/after-nonempty.json
PYTHONPATH="$PWD/target/after/python" taskset -c 0 target/bench-gil/bin/python benches/compare_event_loops.py --loops rsloop --workloads tasks,tcp_streams --tasks 200000 --tcp-roundtrips 20000 --repeat 7 --warmups 2 --json-output target/after-core.json
```

Choose an allowed CPU on your host. For churn, run
`scheduler_workloads.bench_tcp_connect_churn("rsloop", 1000, 1024)` through
`compare_event_loops.run_with_loop` in each fresh subprocess, using the same
before/after package selection and warmup/repeat counts. The downloadable
investigation archive also contains the exact orchestration scripts and full
per-run JSON metadata.
