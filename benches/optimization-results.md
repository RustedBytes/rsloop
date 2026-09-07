# Five-path optimization experiment — 2026-09-07

The largest measured improvement is **4.84× faster raw-socket round trips**.
Buffered delivery, timer batches, and `writelines` also improve. Callback gains
are small. This does **not** make rsloop faster than uvloop and zuvloop on every
workload, and the application-level regression gate is not clean.

## Implemented changes

1. **Raw socket operations:** `sock_recv`, `sock_recv_into`, `sock_sendall`, and
   `sock_accept` attempt nonblocking I/O immediately and return a loop-native
   Future. Pending operations wait on the owning loop's vibeio reactor, using
   an owned duplicate for registration and cancellable Rust wake state.
   Python socket methods still run on the loop thread, including subclass
   overrides. Buffer exports are not retained across waits. Blocking sockets
   are rejected. Generic DNS/connect and datagram compatibility paths retain
   their existing implementations.
2. **BufferedProtocol delivery:** copy directly from the Rust read buffer into
   a writable contiguous Python buffer export; remove intermediate `bytes`,
   slice-assignment, and argument-tuple work. Release the export before
   `buffer_updated`, allowing the protocol to resize or replace its buffer.
   This removes an intermediate copy; it is **not direct kernel receive into
   the Python buffer**. Generic Protocol reads still use the existing pipeline.
3. **Write staging:** move the first owned buffer into staging, copy only the
   unsent suffix after partial borrowed writes, avoid `builtins.bytes` lookup
   for immutable `writelines` segments, and dispatch native stream-writer
   `writelines` directly in Rust. Subsequent staged segments can still be
   concatenated. Retained scatter/gather Python buffers are not introduced.
4. **Timers:** the active heap and deadline selection belong to the loop thread.
   Local scheduling avoids the coordination-thread channel. A pending heap
   handles scheduling outside a run; a guard preserves timers across stops.
   Callback destruction happens outside heap/mutex borrows, including cancelled
   timers and close-time cleanup.
5. **Callback ingress:** FASTCALL descriptors store zero/one arguments without
   an intermediate tuple, while preserving keywords and explicit contexts.
   Handles use a bounded 8,192-entry PyO3 freelist. The measurement does not
   establish a substantial general callback speedup.

These changes retain the existing ownership and backpressure model. The deeper
direct-receive, loop-local transport-state, and retained-vectored-write designs
remain possible follow-up work; this experiment does not claim to implement
those larger architectural rewrites.

## Measurement method

- Baseline: release wheel rebuilt from `493447e` (rsloop 0.1.49).
- Candidate: the working-tree optimization patch, also 0.1.49. The result JSON
  records a SHA-256 over sorted Rust sources, Cargo manifests, and build script.
- Host: Linux x86-64, Intel Core i9-9900K, 8 cores/16 threads, CPython 3.14.7.
  Process affinity includes CPUs 0–15; CPU frequency was not fixed.
- Competitors: uvloop 0.22.1 and zuvloop 0.0.14, same Python/environment.
- Focused probes: seven fresh-process measurements per variant; an initial
  block is discarded. Before/after/uvloop/zuvloop execution order rotates.
- Application A/B: seven alternating before/after process blocks per scenario.
  Each process performs two discarded warmups followed by one measurement,
  with 16 connections and 500 requests per connection. Bulk transfer is 2 MiB
  per connection. No builds or tests ran concurrently with these measurements.
- The complete sustained three-loop matrix was also run before and after.
  Because its seven warm measurements share a process and small regressions
  appeared, the independent alternating A/B experiment is the main evidence
  for application-level changes.

Raw focused samples: [optimization-probes-2026-09-07.json](results/optimization-probes-2026-09-07.json).
Application run records and uncertainty: [optimization-matrix-2026-09-07.json](results/optimization-matrix-2026-09-07.json).
The latter retains per-run timings, throughput inputs, p95/p99 and RSS, not every
individual request latency. Full intermediate logs remain in
`target/optimization-results/` locally.

## Focused results

Median elapsed milliseconds; lower is better. Speedup = before / after.
These are whole-probe measurements, not isolated syscall or allocation timings.

| Probe | rsloop before | rsloop after | Speedup | uvloop | zuvloop |
|---|---:|---:|---:|---:|---:|
| Raw sockets: 2,000 one-byte round trips | 340.124 | 70.228 | 4.84× | 51.674 | 49.908 |
| 100,000 callbacks, zero arguments | 28.841 | 29.266 | 0.99× | 37.443 | 26.668 |
| 100,000 callbacks, one argument | 31.614 | 30.538 | 1.04× | 57.511 | 29.253 |
| 40 batches of 5,000 recycled callbacks | 44.867 | 44.161 | 1.02× | 56.272 | 41.524 |
| 10,000 zero-delay timers | 5.767 | 4.932 | 1.17× | 5.230 | 4.831 |
| 10,000 1-ms timers | 5.472 | 4.604 | 1.19× | 11.096 | 5.724 |
| 32 MiB generic Protocol delivery | 15.497 | 15.872 | 0.98× | 8.783 | 7.536 |
| 32 MiB BufferedProtocol delivery | 20.909 | 16.127 | 1.30× | 15.227 | 13.187 |
| 32 MiB through two-segment `writelines` | 21.006 | 18.762 | 1.12× | 11.197 | 23.073 |

Raw sockets reduce median elapsed time by 79.4%; buffered delivery by 22.9%;
positive timers by 15.9%; and `writelines` by 10.7%. These percentages differ
from the throughput-style speedup factors above.

Paired, run-level log-ratio bootstrap intervals (10,000 resamples, seed 0)
support more than 3% lower elapsed time for raw sockets, both timer probes,
buffered delivery, and `writelines`. Callback and generic-Protocol changes are
inconclusive against that 3% threshold. Zero-argument callbacks are slightly
slower at the median; the one-argument callback interval spans approximately
−4.8% to −0.1% elapsed time, insufficient to establish a >3% improvement.
Intervals use geometric-mean ratios, whereas the table uses medians. Seven
blocks on one host are evidence, not a universal performance guarantee.

## Application-level A/B results

Median total operations/second, including setup/teardown. These are the
independent alternating A/B results, not the original sustained matrix.

| Workload | Before | After | Throughput change |
|---|---:|---:|---:|
| HTTP keepalive | 51,842 | 51,599 | −0.47% |
| TLS HTTP | 66,920 | 66,711 | −0.31% |
| websockets plaintext | 23,453 | 23,555 | +0.43% |
| websockets TLS | 26,361 | 26,164 | −0.75% |
| aiohttp WebSocket plaintext | 30,226 | 30,070 | −0.52% |
| aiohttp WebSocket TLS | 32,482 | 32,317 | −0.51% |
| Mixed streams | 42,907 | 41,515 | −3.25% |
| Bulk transfer (completed connections/s) | 894 | 906 | +1.27% |

Most end-to-end workloads do not exercise the changed paths enough to produce
a clear gain. **Mixed streams remains a concern:** its elapsed-time interval
is approximately +0.4% to +11.3%, and its median throughput is 3.25% lower.
The interval crosses the 3% regression threshold, so the classifier says
“inconclusive”; this must not be interpreted as proof of no regression.

The original sustained-matrix gate failed for plaintext aiohttp WebSocket
throughput/p99 and mixed-stream throughput. Alternating A/B reduced the apparent
aiohttp regression, but did not clear mixed streams. The patch is therefore an
implemented and measured optimization experiment, **not a blanket production
performance-gate pass**. Profiling mixed streams and isolating callback/staging
effects is the next performance decision before a release claim.

## Correctness checks

- 303 Rust tests pass; native library Clippy passes with `-D warnings`.
- 135 Python tests pass on CPython 3.14.7 (two optional/environment skips).
- 24 socket/timer/callback/buffer/concurrency tests pass on free-threaded
  CPython 3.14.0 without enabling the GIL.
- 19 focused regression tests pass on CPython 3.10.18 using a separately built
  release wheel.
- Regressions cover cancellation without consuming replacement data, partial
  sends, typed buffers, immediate buffer resizing after cancellation/delivery,
  EOF, pre-run waits, callback context/keyword/weakref behavior, subtype method
  descriptors, timer stop/restart and reentrant finalizers.
- The standalone vibeio harness cross-checks for Windows GNU and macOS ARM64.
  The complete extension cross-builds are blocked by missing MinGW/macOS C
  toolchains for AWS-LC; no Windows/macOS runtime or performance claim is made.

## Reproduce

Build the original revision in a separate worktree and install its wheel into
an isolated directory; keep the candidate installed in the benchmark venv.
Use the same interpreter and release settings for both. For example, after
building the baseline wheel:

```bash
uv pip install --python .venv/bin/python --target target/baseline-package path/to/baseline.whl
.venv/bin/maturin develop --release --locked
.venv/bin/python benches/optimization_probes.py \
  --baseline-pythonpath target/baseline-package --repeat 7 > target/probes-ab.json
.venv/bin/python benches/optimization_matrix_ab.py \
  --baseline-pythonpath target/baseline-package --repeat 7 > target/matrix-ab.json
```

Install uvloop and zuvloop into the venv for the focused comparison. Both new
runners print progress to stderr and JSON to stdout. Existing
`workload_matrix.py --sustained` remains the three-loop application comparison;
the new A/B runner complements it rather than replacing its regression gate.
