# Reactor performance follow-up — 2026-09-07

This follow-up fixes the intermittent short transfer identified in the
[systems investigation](systems-performance.md), then reduces raw-socket wait
overhead. The final change cuts raw-socket elapsed time by about 34%. Buffered
reads and `writelines` improve in the focused probes. It does not establish a
general application throughput improvement or close the callback allocation gap.

[Recorded results](results/reactor-performance-2026-09-07.json) retain source
and binary hashes, process-run samples, bootstrap comparisons, and the rejected
unconditional-polling experiment. Local diagnostic files are under
`target/protocol-optimization/`.

**Fix data ownership before measuring throughput.** The original `4b0010f`
release build lost data in two of 30 repeated 32 MiB `writelines` probes. Both
failures received 33,542,728 instead of 33,554,432 bytes. Python flow-control
callbacks can synchronously re-enter the transport:

- `queue_write` invoked `pause_writing` before publishing the data to the
  writer queue. A callback calling `close` or `write_eof` could enqueue a control
  command ahead of those bytes.
- After a partial direct write, `flush_pending_direct_write` reduced its buffer
  count before transferring the unsent suffix to the writer. That reduction
  can invoke `resume_writing`. A closing callback could finish the transport
  while the remaining bytes were still owned by the current Rust stack frame.

The [write fix](../src/transport/stream/core_write.rs) publishes data before
either callback can run. It retains buffer accounting and transfers the unsent
suffix before reporting the drained prefix. Four
[regression cases](../tests/test_write_reentrancy.py) cover pause/resume crossed
with close/write_eof, force backpressure with a small socket send buffer, and
compare every received byte. All four fail on the original build and pass with
the fix. A further 100 fresh-process transfers alternating generic reads,
buffered reads, and `writelines` passed on the correctness-only build.

**Poll where local readiness can make progress.** When Python's ready queue
was empty, the loop released the GIL, tried its cross-thread wake spin, and
constructed a parking future before driving its own reactor. For loop-owned
socket waits, data could already be ready in the kernel while this overhead
was paid. The first experiment polled the reactor before every such wait. That
helped raw sockets, but increased native-fast-stream time by 16.6%, with a
paired 95% interval of +14.1% to +19.1%. Those exchanges depend on worker
wakeups; the extra reactor work delayed their successful spin path.

The final [loop change](../src/engine/loop_core.rs) uses the existing spin
cooldown as a signal. After a spin misses, it performs a bounded, nonblocking
reactor poll before detaching and parking. If that poll publishes ready work,
the next Python turn runs immediately. Successful worker-thread exchanges
retain the existing spin path. The loop still polls during hot Python callback
chains, checks signals and timers, and parks when no work is ready. The 50 µs
spin setting and eight-park cooldown remain unchanged.

Nine alternating fresh-process native-stream pairs measured 84.64 ms before
and 83.25 ms after for 5,000 round trips. The paired change is −1.13%, with an
interval of −2.58% to +0.66%: inconclusive, and substantially different from
the rejected unconditional-polling regression.

**Focused measurements.** All performance A/B comparisons use a baseline
containing the write correctness fix, with the final reactor change applied
only to the candidate. This isolates scheduling from the data-loss fix.
Linux x86-64, Intel Core i9-9900K, CPython 3.14.7, release builds, rsloop 0.1.49,
uvloop 0.22.1, zuvloop 0.0.14. CPU affinity is 0–15; frequency is not fixed and
host services remain active. No builds, tests, or profiling ran concurrently
with timed benchmark processes.

Seven measured fresh-process blocks, rotating before/after/competitor order,
after one discarded block. The raw-socket probe uses 2,000 one-byte Unix
socketpair round trips; protocol probes transfer 32 MiB; callbacks schedule
100,000 handles; timers schedule 10,000 handles at zero or 1 ms delay.
Median elapsed milliseconds:

| Probe | rsloop before | rsloop after | uvloop | zuvloop |
|---|---:|---:|---:|---:|
| Raw socket round trips | 71.750 | 47.522 | 51.799 | 50.594 |
| Generic protocol read | 15.294 | 14.797 | 8.806 | 7.216 |
| Buffered protocol read | 15.772 | 15.367 | 14.851 | 13.161 |
| Writelines | 18.433 | 17.187 | 11.085 | 22.873 |
| Zero-delay timers | 4.832 | 4.831 | 5.285 | 5.053 |
| Positive-delay timers | 4.625 | 4.693 | 11.226 | 5.746 |
| Zero-argument callbacks | 30.219 | 29.816 | 37.745 | 27.012 |

Paired process-block log-ratio bootstrap changes, 10,000 resamples, approximate
95% intervals; negative means less elapsed time. An interval must entirely
clear ±3% to classify a material improvement/regression. These exploratory
comparisons have no multiple-comparison adjustment.

| Probe | Change | 95% interval | Classification |
|---|---:|---:|---|
| Raw socket round trips | -34.04% | -35.87% to -32.05% | Improved |
| Generic protocol read | -4.23% | -6.61% to -1.57% | Inconclusive |
| Buffered protocol read | -5.34% | -7.38% to -3.45% | Improved |
| Writelines | -5.65% | -7.85% to -3.36% | Improved |
| Zero-delay timers | +0.29% | -0.12% to +0.72% | Inconclusive |
| Positive-delay timers | -0.49% | -2.82% to +1.82% | Inconclusive |
| Zero-argument callbacks | -3.43% | -8.71% to -0.05% | Inconclusive |

Geometric mean changes differ from ratios of medians; individual slow process
runs affect the estimates and intervals. Generic protocol reads still trail
both competitors. Buffer copies, transport queues and locks, and Python
handle/context allocation remain future targets.

**Application and idle checks.** Seven alternating A/B process blocks per
application, each with two discarded warmups and one measured run, using
16 connections and 500 requests per connection. Traffic-only elapsed changes:

| Workload | Change | 95% interval |
|---|---:|---:|
| http_keepalive | +0.82% | -0.40% to +2.45% |
| tls_http | +0.63% | -3.03% to +4.08% |
| websockets_messages | -1.78% | -2.31% to -1.31% |
| websockets_tls | -0.17% | -5.37% to +4.40% |
| aiohttp_websocket_messages | -1.55% | -2.33% to -0.83% |
| aiohttp_websocket_tls | -2.11% | -4.79% to +1.13% |
| mixed_streams | -1.65% | -8.68% to +3.59% |
| bulk_transfer | +0.70% | -1.48% to +3.07% |

All application intervals are inconclusive at the 3% threshold. This does not
establish equivalence or rule out smaller regressions. Process-run p95/p99 and
peak RSS are retained in the results; no request-level independence is assumed.

Idle v2 used eight alternating fresh-process pairs, 200 established connections,
five discarded cycles and 20 measured cycles, with 10 ms idle between cycles.
The median process-level cycle-p95 was 6.49 ms before and 3.29 ms after.
Its paired change was -20.10% with an interval of
-39.12% to +2.80%; inconclusive. The wide interval
precludes an idle-latency improvement claim. A separate seven-pair CPU sanity
check around `rsloop.run(asyncio.sleep(0.5))` used 10.64/11.12 ms
median process CPU before/after. This includes loop startup/shutdown and is not
a precise steady-state power measurement; it found no continuous busy loop.

**Standard competitor comparison.** Seven fresh-process measurements after
two warmups, default standard-runner counts, native fast streams enabled.
These descriptive competitor medians are not paired before/after evidence.
Elapsed milliseconds:

| Workload | rsloop | uvloop | zuvloop |
|---|---:|---:|---:|
| 200,000 callbacks | 47.65 | 52.08 | 37.82 |
| 50,000 tasks | 85.39 | 90.12 | 78.92 |
| 5,000 TCP stream round trips | 84.91 | 125.04 | 104.90 |

**Validation and reproduction.** Final checks passed: 306 Rust tests;
238 Python tests, two skipped; Clippy with all targets and warnings denied;
Rust formatting and Ruff checks. The final candidate also completed 100 fresh
process transfers alternating generic reads, buffered reads, and `writelines`
without byte-count failures. The new busy-socket test verifies progress for
timers and thread-safe callbacks during continuous traffic. Execution was
validated on GIL-enabled Linux CPython 3.14; other platforms/interpreters were
not executed.

To reproduce the A/B baseline, check out `4b0010f` separately, apply only the
`src/transport/stream/core_write.rs` diff from this change, build in release
mode, and copy its installed `rsloop` package **and dist-info** into an isolated
import directory. Then build the full candidate. The recorded source and binary
hashes distinguish the original, correctness-only baseline, and candidate.

```bash
.venv/bin/maturin develop --release --locked
.venv/bin/python benches/optimization_probes.py \
  --baseline-pythonpath target/protocol-optimization/correctness-baseline \
  --repeat 7 --label reactor-during-spin-cooldown \
  --scenarios raw_socket,protocol_read,buffered_read,writelines,timers_zero,timers_positive,callbacks_zero
.venv/bin/python benches/optimization_matrix_ab.py \
  --baseline-pythonpath target/protocol-optimization/correctness-baseline --repeat 7
.venv/bin/python benches/compare_event_loops.py \
  --loops rsloop,uvloop,zuvloop --repeat 7 --warmups 2
.venv/bin/python -m pytest -q tests/test_write_reentrancy.py tests/test_local_sockets.py
```

For the native TCP A/B, alternate baseline/candidate import paths over nine
pairs of `benches/compare_event_loops.py --child --loop rsloop --workload tcp_streams`.
For idle, alternate them over eight pairs of
`benches/workload_matrix.py --child --loop rsloop --scenario idle_connections
--idle-connections 200 --idle-cycles 20 --idle-warmup-cycles 5 --idle-seconds 0.01`.
Set `RSLOOP_USE_FAST_STREAMS=1` for these comparisons. Resample whole paired
process blocks with `benches/idle_statistics.py`, using a 3% threshold.
