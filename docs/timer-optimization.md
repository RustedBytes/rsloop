# Timer scheduling and PyO3 borrow overhead

Baseline: `1449641`, after the callback/queue memory changes. The candidate has
two production changes:

- Derive `TimerHandle.when()` from the heap's actual `Instant` deadline. The
  previous path read the monotonic clock once for the Python timestamp and again
  for the heap deadline. One read now serves both, and the reported timestamp
  agrees with the deadline used for dispatch.
- Make `PyTimerHandle` a frozen PyO3 class. Its only mutable state is already
  atomic, and all Python methods take shared references. PyO3 can consequently
  omit its dynamic borrow counter. Cancellation still uses `AtomicBool` and the
  existing weak callback reference.

PyO3 0.29.3's ordinary shared-borrow path uses an atomic compare/exchange to
increment its counter and an atomic decrement on release. Frozen classes use
an empty borrow-checker slot. This affects methods such as `cancel`, `cancelled`,
and `when`. Source:
[PyO3 borrow checker](https://github.com/PyO3/pyo3/blob/v0.29.3/src/pycell/impl_.rs).

The compiler reports that the complete PyO3 timer-handle object shrinks from
64 to 56 bytes on this build. Allocator size classes and the bounded freelist
limit what that implies for process memory; the measured RSS results below do
not show a repeatable memory reduction.

The timer freelist stays enabled. Unlike an ordinary callback handle, a timer
handle is not owned by the scheduled callback. If Python immediately discards
it, its allocation can be reused while its callback is still pending. The
earlier ordinary-handle freelist result therefore does not justify removing the
timer freelist.

## Experiment

Uninstrumented releases on Linux x86-64, Intel Core i9-9900K, CPython 3.14.7,
Rust 1.100.0-nightly (`f7575a9da`), PyO3 0.29.3. Packages are copied into isolated
directories after `maturin develop --release`; manifests preserve file hashes,
compiler identity, baseline commit, and the candidate diff. These are comparison
packages, not full lab build artifacts containing compiler IR and source trees.

The existing lab runner verifies the imported binary and runs balanced shuffled
AB/BA blocks in fresh processes, with a full warmup per process. GC is disabled
inside workloads. No build or test runs concurrently with timing. CPU frequency
and unrelated host load are not controlled.

The primary workload is `timers_retained`; guards are `timers_discarded`,
`timers_cancelled`, `callbacks`, `tasks`, and `tcp_connect_churn`. Each experiment
has 12 blocks. The new workloads and their training/holdout shapes are described
in [hotpath-lab.md](hotpath-lab.md). Timer probes include scheduling and dispatch
or cancellation plus handle release; these are not isolated clock-call timings.

Initial comparison, 12 paired blocks:

| Workload | Baseline median | Candidate median | Paired change (adjusted interval) |
| --- | ---: | ---: | ---: |
| Retained timer handles | 387.34 ms | 367.02 ms | -2.6% [-5.3%, +5.0%] |
| Discarded timer handles | 368.88 ms | 350.52 ms | -5.2% [-7.8%, -2.7%] |
| Cancelled timers | 374.68 ms | 350.74 ms | -10.6% [-21.5%, -5.4%] |
| Callback burst | 441.74 ms | 440.66 ms | -0.2% [-1.2%, +0.9%] |
| Tasks | 519.19 ms | 519.13 ms | -0.1% [-1.6%, +1.8%] |
| Generic TCP connection churn | 491.94 ms | 490.30 ms | +0.1% [-2.2%, +3.3%] |

Changes use paired log-time ratios, not the ratio of medians. Intervals bootstrap
process pairs and adjust for all twelve timing/RSS comparisons with a 95%
family confidence target. The discarded and cancelled cases show improvement,
but the predeclared primary retained-handle result is inconclusive. The lab's
overall speed gate did not pass. Timer peak-RSS changes were approximately -1%;
the control workloads were near zero.

Fresh confirmation, 12 paired blocks, seed 2718. Timer volume increases to
1,200,000 while batch size falls from 10,000 to 777, below the 1,024-entry
freelist capacity. Callback/task volumes and task batch size also change; the
TCP probe increases to 4,000 connections and 8 KiB payloads.

| Workload | Baseline median | Candidate median | Paired change (adjusted interval) |
| --- | ---: | ---: | ---: |
| Retained timer handles | 438.02 ms | 417.09 ms | -4.6% [-5.9%, -3.0%] |
| Discarded timer handles | 403.49 ms | 387.11 ms | -3.8% [-5.2%, -2.2%] |
| Cancelled timers | 433.33 ms | 402.21 ms | -6.7% [-7.5%, -6.1%] |
| Callback burst | 653.14 ms | 657.50 ms | +1.3% [-0.3%, +3.0%] |
| Tasks | 578.39 ms | 589.09 ms | +1.1% [-1.1%, +2.4%] |
| Generic TCP connection churn | 685.02 ms | 683.50 ms | -0.2% [-1.8%, +1.1%] |

The confirmation passes the lab's balanced gate: the primary interval is below
-1%, with every timing and RSS upper bound below +3%. The callback bound is
close to the limit (unrounded +2.997%), so this is not evidence of zero cost to
other workloads. Peak-RSS changes are small: timer point estimates range from
+0.16% to +0.59%, and all memory interval upper bounds are below +1.3%.

Keep the candidate for its measured timer improvement, including the two timer
cases that improved in both runs. The first experiment's primary result remains
inconclusive; it should not be silently replaced by the confirmation. Neither
run demonstrates faster callback bursts, tasks, or network connections.

The experiment measures both production edits together. These are zero-delay
timer lifecycle workloads, not timer precision or application latency tests;
they do not establish a general networking speedup.

## Validation

The candidate passes 309 Rust library tests, 216 Python core tests (2 skipped),
58 benchmark/tooling tests, and the all-features Cargo check. New tests cover
timer timestamp agreement with the heap, cross-thread cancellation, weakrefs
after freelist reuse, partial final benchmark batches, and generic TCP connection
startup/echo/close. Both asyncio and rsloop run the new benchmark smoke tests.
These results use GIL-enabled CPython on Linux; they do not establish behavior or
performance on every supported interpreter and platform.

```bash
cargo test --lib --locked
.venv/bin/python -m pytest -q tests/test_*.py
.venv/bin/python -m pytest -q tests/tooling/test_benchmark_loops.py \
  tests/tooling/test_hotpath_lab.py -m tooling
cargo check --all-features --locked
.venv/bin/python scripts/hotpath_lab.py compare \
  --baseline target/timer-optimization/baseline \
  --candidate target/timer-optimization/candidate \
  --workloads timers_retained,timers_discarded,timers_cancelled,callbacks,tasks,tcp_connect_churn \
  --primary timers_retained --suite training --blocks 12 \
  --out target/timer-optimization/training
.venv/bin/python scripts/hotpath_lab.py compare \
  --baseline target/timer-optimization/baseline \
  --candidate target/timer-optimization/candidate \
  --workloads timers_retained,timers_discarded,timers_cancelled,callbacks,tasks,tcp_connect_churn \
  --primary timers_retained --suite holdout --blocks 12 --seed 2718 \
  --out target/timer-optimization/holdout
```

Local package copies, manifests, test logs, plans, raw process samples, and
archived benchmark sources are under `target/timer-optimization/`. Choose a new
output directory when repeating a comparison. For committed revisions, use the
standard isolated build workflow in [hotpath-lab.md](hotpath-lab.md).
