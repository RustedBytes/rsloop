# PyPI rsloop 0.1.54 benchmark

On October 4, 2026, the live [PyPI JSON API](https://pypi.org/pypi/rsloop/json)
reported [0.1.54](https://pypi.org/project/rsloop/0.1.54/) as the latest release.
The CPython 3.14 Linux wheel was uploaded at `2026-10-04T09:19:49.632695Z`.

Downloaded and benchmarked:

```text
rsloop-0.1.54-cp314-cp314-manylinux_2_39_x86_64.whl
SHA-256: aabab6f48b949810ae82716e342398484b111ac314172722751ac061575cc443
```

The checksum matches PyPI metadata. The wheel was installed with `uv pip install
--target` under `target/pypi-speed-check/pypi/package`, and its import path and
distribution version were verified. The development installation was preserved.
The wheel's `Handle` and `TimerHandle` object sizes are 80 and 56 bytes,
respectively, matching the optimized layouts on the local branch. Matching
layouts do not establish identical source or build settings.

## Standard loop comparison

Linux x86-64, Intel Core i9-9900K, GIL-enabled CPython 3.14.7, uvloop 0.22.1.
The repository's `benches/compare_event_loops.py` ran seven measured fresh
processes after two warmups per loop/workload. Times are medians; lower is better.

| Workload | asyncio | uvloop | PyPI rsloop 0.1.54 |
| --- | ---: | ---: | ---: |
| 200,000 callbacks | 106.30 ms | 51.06 ms | 43.87 ms |
| 50,000 tasks, batches of 5,000 | 141.76 ms | 87.40 ms | 85.42 ms |
| 5,000 TCP roundtrips, 1 KiB | 142.74 ms | 117.73 ms | 77.31 ms |

The default TCP comparison uses rsloop's native fast streams and stdlib streams
for the other loops. These are short, sequential microbenchmarks, not paired
application-level comparisons. In particular, the small task difference from
uvloop should not be generalized into an overall performance advantage.

A separate run disabled rsloop's native fast streams, using the stdlib streams
layer for all loops (same 5,000 roundtrips, 1 KiB, seven runs/two warmups):

| asyncio | uvloop | PyPI rsloop 0.1.54 |
| ---: | ---: | ---: |
| 147.30 ms | 117.68 ms | 125.93 ms |

Here uvloop had the lowest median; rsloop took 7.0% longer. The default native
stream result therefore should not be presented as a win with every streams
configuration. These separate short runs remain descriptive measurements.

These commands used the benchmark scripts at `a570b9f`. The stream-mode switches
were subsequently removed when native streams became mandatory on rsloop.
Use that revision or the archived harness to reproduce this historical check.

```bash
PYTHONPATH="$PWD/target/pypi-speed-check/pypi/package" \
  .venv/bin/python benches/compare_event_loops.py \
  --loops asyncio,uvloop,rsloop --workloads callbacks,tasks,tcp_streams \
  --warmups 2 --repeat 7 \
  --json-output target/pypi-speed-check/event-loops.json
PYTHONPATH="$PWD/target/pypi-speed-check/pypi/package" \
  .venv/bin/python benches/compare_event_loops.py \
  --loops asyncio,uvloop,rsloop --workloads tcp_streams \
  --no-rsloop-fast-streams --warmups 2 --repeat 7 \
  --json-output target/pypi-speed-check/stdlib-tcp.json
```

## Published wheel versus local branch

The local binary is a normal `maturin develop --release` build of `a570b9f`
on `perf/hotpath-rs-profiling` (project version 0.1.53). It was copied into a
second isolated package directory. Both ELF `.comment` sections report Rust
1.100.0-nightly (`f7575a9da`, September 24), GCC 13.3.0, and LLD 23.1.1. All four
packaged Python `.py` files have identical hashes. The complete published build
settings and source revision are unavailable, so this compares two distributable
binaries rather than isolating the effect of source edits.

The comparison reuses `scripts/hotpath_lab.py` workload configurations,
`run_sample`, balanced AB/BA orders, and paired bootstrap statistics. An archived
driver runs 12 shuffled paired blocks across eight workloads, with one full
warmup per fresh process. Every child verifies the selected native extension's
path and package inventory. Holdout workload sizes keep measured runs longer
than the short standard comparison above. GC is disabled during workloads.

The ordinary lab comparison intentionally requires matching compiler metadata
and flags. The published manifest records build provenance as unknown; embedded
ELF compiler labels are preserved separately. This distribution comparison uses
a separate driver, without applying the source-change promotion gate.
Both manifests and all benchmark sources are archived. No compilation or tests
run concurrently with timings; CPU frequency and unrelated host load are not
controlled.

All 12 blocks completed. The table shows median elapsed time and the paired
change from the published wheel to the local branch; negative means the branch
is faster. The paired change uses log-time ratios, not the ratio of medians.
Bootstrap intervals adjust across eight workloads and two metrics with a 95%
family confidence target. Every measured sample lasted at least 0.38 seconds.

| Workload | PyPI 0.1.54 | Branch `a570b9f` | Branch timing change (adjusted interval) |
| --- | ---: | ---: | ---: |
| 3,000,000 callbacks | 655.43 ms | 650.93 ms | -0.7% [-2.4%, +0.8%] |
| 350,000 tasks, batches of 777 | 584.30 ms | 583.41 ms | -1.0% [-2.8%, +0.3%] |
| 25,000 native TCP roundtrips, 8 KiB | 485.43 ms | 490.18 ms | +0.2% [-1.1%, +1.6%] |
| HTTP, 7 connections, 4,000 requests each | 495.83 ms | 498.93 ms | +1.1% [-1.4%, +4.0%] |
| 1,200,000 retained timers | 414.53 ms | 418.46 ms | +1.0% [-0.05%, +2.3%] |
| 1,200,000 discarded-handle timers | 388.67 ms | 386.12 ms | -1.2% [-3.2%, +0.05%] |
| 1,200,000 cancelled timers | 404.27 ms | 405.51 ms | +0.3% [-1.1%, +1.8%] |
| 4,000 generic TCP connections, 8 KiB | 693.48 ms | 691.55 ms | +0.1% [-2.0%, +2.0%] |

Timer batch size is 777. All timing intervals include zero: this experiment
does not establish a speed difference between the published wheel and the
local branch. This is not proof of exact equivalence or of identical source.

The published wheel's median peak RSS was about 0.6–0.9 MiB lower in most
workloads. For example, tasks used 33.42 versus 34.30 MiB and callbacks used
515.84 versus 516.50 MiB. Connection-churn peak RSS was essentially unchanged.
RSS includes the warmup and reflects the GC-disabled benchmark configuration;
these are whole-process measurements, not Python object-size measurements.

## Local artifacts

`target/pypi-speed-check/` contains the original wheel, full PyPI metadata,
selected-wheel metadata and digest, isolated packages, standard benchmark JSON
and log, comparison driver, plans, raw samples, and archived harness sources.
The driver is `target/pypi-speed-check/compare_distributions.py`; it refuses to
overwrite its `paired/` output directory.

Install the verified wheel into its isolated directory with:

```bash
uv pip install --python .venv/bin/python --no-deps \
  --target target/pypi-speed-check/pypi/package \
  target/pypi-speed-check/rsloop-0.1.54-cp314-cp314-manylinux_2_39_x86_64.whl
```
