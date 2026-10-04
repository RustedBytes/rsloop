# hotpath-rs profiling experiment

This branch adds an opt-in `hotpath-profile` Cargo feature. Normal builds have no
profiler hooks. The feature instruments callback scheduling and invocation,
context operations, dispatcher work, and selected TCP stream paths. A pair of
private `_loop.hotpath_start(path)` / `_loop.hotpath_stop()` functions bounds the
profile around Python-driven workloads; `scripts/profile_with_hotpath.py` runs
the existing `compare_event_loops.py` workloads with a short warmup.

## Reproduce

```bash
.venv/bin/maturin develop --release --features hotpath-profile
.venv/bin/python scripts/profile_with_hotpath.py callbacks --output target/hotpath-rs/callbacks.txt
.venv/bin/python scripts/profile_with_hotpath.py tasks --output target/hotpath-rs/tasks.txt
.venv/bin/python scripts/profile_with_hotpath.py tcp_streams --output target/hotpath-rs/tcp_streams.txt
.venv/bin/maturin develop --release
.venv/bin/python benches/compare_event_loops.py --loops rsloop \
  --workloads callbacks,tasks,tcp_streams --repeat 5 --warmups 2 \
  --json-output target/hotpath-rs/uninstrumented.json
```

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
