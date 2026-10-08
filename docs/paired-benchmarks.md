# Measuring the smol migration

Run **Paired event-loop benchmarks** in Actions (`paired-benchmark.yml`). A PR
changing the harness or workflow also runs it. No historical numbers are copied
into the output: all five candidates are measured afresh.

Each Python job builds release wheels with the same Rust toolchain and locked
Cargo dependencies for:

- `historical`: `43f624a2d5a9781b15e5a976ea780d19da3325dc`, the October 6 benchmark revision.
- `pre`: `56d0ebd6769255adace1db76718d1c986ddf998e`, the immediate pre-migration master.
- `current`: the exact checked-out workflow commit, recorded in the artifact.

The same runner, CPython build, CPU affinity and harness are used for these
revisions and uvloop 0.22.1 / zuvloop 0.0.17. CPython 3.14.0 and 3.14.0t run in
separate jobs; compare loops **within** a job, not absolute times across jobs
(which may have different CPUs). Every child verifies the GIL state after native
imports. Incompatible modules fail the job rather than silently skipping or
forcing the GIL off. Context-aware warnings are disabled consistently,
PYTHONHASHSEED=0, GC is disabled during workloads. Affinity is set before native
imports; worker threads inherit the selected CPU. These are single-CPU tests,
including in the free-threaded build, not multicore scalability measurements.

The unchanged comparison functions measure 1M batched callbacks, 200K tiny tasks
in batches of 5K, and 20K sequential loopback TCP round trips of 1024 bytes.
Throughput excludes connection setup. A separate TCP run records individual RTTs
with perf_counter_ns, discarding 100 initial round trips; it does not instrument
the throughput run. Callbacks/tasks report batch duration, not individual
scheduling latency. TCP uses each loop's default asyncio stream integration,
including rsloop's native streams.

Two fresh-process warmups precede nine measured rounds per workload. Each round
randomizes all five candidates with a recorded seed to reduce order bias.
Children have a 180-second timeout. Absolute peak RSS is the child's OS high-water
mark after loop shutdown; it includes interpreter/import costs and is not an
allocation count. Baseline RSS is also saved. Latency-run RSS includes the RTT
sample array; use the uninstrumented TCP run for the memory comparison.

Artifacts include exact SHAs, harness hashes, interpreter/package versions,
runner CPU/kernel, Rust compiler, dependency freezes, raw trials (including
warmups), individual RTT samples, medians, min/max, coefficient of variation and
nearest-rank p95/p99. The Markdown summary reports median throughput, duration,
RSS and RTT, plus current/pre percentage changes. RTT percentiles in the summary
are medians of per-trial percentiles, not pooled samples.

A positive current/pre duration or RSS delta is a possible regression, not proof:
inspect variability and paired rounds and repeat the workflow on the same runner
class. Hosted Actions hardware is shared and not a dedicated performance lab.
The historical/current comparison includes other intervening changes; pre/current
better isolates #101, but still includes any later current commits. To isolate
exactly the migration, run this harness against its merge SHA
`eed0101b8674aae47ea4156a3bdc17ce4d50ca3b` in the current environment.

Callbacks/tasks exercise rsloop's scheduler, which the migration retains. TCP
covers some bridge paths but these workloads do not comprehensively measure TLS,
filesystem offload, or arbitrary Rust futures. No claim about those paths follows
from this comparison.

For local reproduction on Linux, prepare `historical`, `pre`, and `current`
virtual environments as in the workflow, with a `revisions.json` mapping labels
to exact SHAs, then run:

```bash
PYTHONHASHSEED=0 python benches/paired_comparison.py \
  --env-root /absolute/path/to/paired-envs \
  --output /absolute/path/to/results --mode gil --repeat 9 --warmups 2
```

Use `--mode free-threaded` with environments built with 3.14.0t. The parent
interpreter only orchestrates; all measurements use the environments' Python.
The October 8 Codex environment allowed CPython and wheel installation but
rejected io_uring_setup with EPERM. Its measurements are not published as
representative post-migration results; Actions execution is required.
