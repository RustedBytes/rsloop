# Event Loop Benchmark

This benchmark compares:

- stdlib `asyncio`
- `uvloop`
- the Rust prototype in `rsloop`

Run it from the repository root so Python resolves the editable Rust package cleanly:

```bash
uv run --with maturin maturin develop --release
uv run --with uvloop python benchmarks/compare_event_loops.py
```

Benchmark runner: [`benchmarks/compare_event_loops.py`](./compare_event_loops.py)

The Rust prototype should be installed in release mode before benchmarking.
Using the default debug build will heavily skew the comparison against
`asyncio` and `uvloop`.

Useful quick run:

```bash
uv run --with maturin maturin develop --release
uv run --with uvloop python benchmarks/compare_event_loops.py \
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
uv run --with uvloop python benchmarks/compare_event_loops.py \
  --loops rsloop \
  --workloads callbacks,tasks,tcp_streams \
  --profile-rsloop-dir benchmarks/profiles
```

No files are written by Tracy. The directory argument is only used to derive a
human-readable label for the unmeasured profiling pass before the warmup and
measured runs. Open the Tracy desktop profiler and connect while that pass is
running.

The runner executes each loop/workload in a fresh subprocess and reports:

- median time
- best time
- operations per second
- relative slowdown versus the fastest loop for that workload

Current workloads:

- `callbacks`: chained `call_soon()` dispatch
- `tasks`: many tiny `asyncio.sleep(0)` tasks
- `tcp_streams`: local `asyncio.start_server()` / `asyncio.open_connection()` echo round trips

By default, `tcp_streams` uses `rsloop`'s native fast streams. If you want all
three loops to go through the stdlib `asyncio` streams layer instead, pass:

```bash
uv run --with uvloop python benchmarks/compare_event_loops.py --no-rsloop-fast-streams
```
