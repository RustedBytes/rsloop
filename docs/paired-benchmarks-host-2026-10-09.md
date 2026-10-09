# Paired comparison on the Linux host, October 9, 2026

Both full benchmark suites completed on `ml-lab2`, an Intel Core i9-9900K
Linux host. This supplies the measurements that PR #102 could not obtain in
the ChatGPT VM. The host successfully executed `io_uring_setup`; the probe
result is preserved with the artifacts.

The paired median duration changes against the immediate pre-migration revision
were small, with mixed directions:

| Workload or metric | CPython 3.14.0 | CPython 3.14.0t |
|---|---:|---:|
| Callback duration | +1.01% | +0.94% |
| Task duration | +0.54% | +2.39% |
| TCP throughput workload duration | -2.33% | +1.08% |
| TCP RTT median | -0.87% | +1.84% |
| TCP RTT p95 | +0.74% | +2.19% |
| TCP RTT p99 | +3.90% | +1.41% |
| Callback peak RSS | -0.12% | -0.03% |
| Task peak RSS | -0.31% | -0.58% |
| TCP throughput workload peak RSS | -0.06% | -0.12% |

Positive duration and RTT changes mean slower. These are medians of nine
within-round current/pre percentage changes, rather than ratios of aggregate
medians. The full summaries show both calculations and the paired ranges.

The GIL TCP duration pairs ranged from -7.67% to +1.52%, while free-threaded
task duration pairs ranged from -0.98% to +4.46%. These observations do not
establish a general smol improvement or regression. Free-threaded median RTT
was higher in all nine pairs, by +0.38% to +4.16%; another complete run is needed
before treating that small difference as a stable effect. Peak memory was
effectively unchanged for these workloads.

Current rsloop TCP throughput was 61,826 round trips/s with the GIL and 58,690
round trips/s without it, versus uvloop's 39,733 and 38,424, and zuvloop's 49,229
and 48,179. Current rsloop's median RTT was 16.68 and 17.26 microseconds.
Zuvloop had the highest tiny-task throughput in both modes. These comparisons
apply to the harness's default asyncio stream integrations and this one CPU.

All candidates ran sequentially on CPU 0 with the same harness, disabled GC,
`PYTHONHASHSEED=0`, and no forced `PYTHON_GIL` override. Each mode used two
fresh-process warmups and nine measured rounds per candidate and workload:
1,000,000 callbacks, 200,000 tasks in batches of 5,000, 20,000 TCP round trips
with 1,024-byte payloads, and a separate instrumented RTT workload. In total,
440 worker processes completed, including 360 measured trials. Every worker
preserved its expected GIL state after native imports and after its workload.

The exact wheel revisions were:

| Label | Revision | Package version |
|---|---|---|
| historical | `43f624a2d5a9781b15e5a976ea780d19da3325dc` | 0.1.57 |
| pre | `56d0ebd6769255adace1db76718d1c986ddf998e` | 0.1.59 |
| current | `b02ab6a6e876ad7e23b2de7f2dfc18bd5ba18e65` | 0.1.60 |

The current wheel is PR #102's original head, not a later master revision.
Its pre/current difference includes #101, its hotpath/optional-import fix,
the version bump, and the benchmark additions. Rust source changes made after
that revision are outside this comparison. All six wheels used
`nightly-2026-09-25`, locked dependencies, and the default release profile.
Uvloop 0.22.1 and zuvloop 0.0.17 were installed in every isolated environment.

Peak RSS uses `/proc/self/status`'s `VmHWM`. During the host continuation we
confirmed that `getrusage().ru_maxrss` can retain the orchestrator's memory peak
across `exec`, inflating a small worker's result. The harness was corrected,
and the preliminary run using the inherited counter was discarded. Both full
suites reported here use the corrected harness; its SHA-256 is in each
`environment.json`. A regression check passes on both Python builds.

The host had CPU frequency scaling and SMT enabled. CPU 0 affinity does not
exclude activity on its sibling or background host load. Both modes ran on
this host in sequence, but their absolute times still do not measure
multicore scalability. The workloads do not establish performance for TLS,
filesystem offload, or arbitrary Rust futures.

The complete evidence is checked in under
[`benches/results/paired-2026-10-09`](../benches/results/paired-2026-10-09/):

- [GIL summary](../benches/results/paired-2026-10-09/gil/summary.md) and
  [free-threaded summary](../benches/results/paired-2026-10-09/free-threaded/summary.md).
- Compressed `raw.jsonl.gz` files retain every trial, warmup, and individual RTT
  sample. Each mode contains 220 trials and 1,100,000 RTT samples.
- Each mode also contains exact revision SHAs, summary distributions, paired
  deltas, harness hashes, worker metadata, dependency freezes, CPU/kernel/compiler
  details, and the successful io_uring probe.
- `build.log` records all six locked release builds; `SHA256SUMS` covers the
  evidence files. Run `sha256sum -c SHA256SUMS` inside the evidence directory.

See [the benchmark protocol](paired-benchmarks.md) for reproduction instructions
and attribution limits. The existing historical `benchmarks.md` is preserved.
