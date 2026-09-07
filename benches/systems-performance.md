# Systems performance investigation — 2026-09-07

Follow-up: the [reactor investigation](reactor-performance.md) fixes the
short-transfer issue recorded below and measures a further raw-socket
optimization. This report preserves the earlier measurements and findings.

Rsloop's remaining disadvantage is concentrated in callback scheduling and
generic plaintext protocol traffic. It is not uniformly slower than uvloop or
zuvloop. This investigation implements two changes: avoid redundant read-pool
wakeups, and resolve a narrow class of numeric addresses without executor work.
The numeric lookup improvement is large and repeatable; application throughput
improvements from the pool change are small and uncertain.

Baseline: clean commit `63f058e`, rebuilt with `maturin develop --release --locked`
and copied into an isolated import directory before editing. Candidate: the
working-tree changes described here. Both report rsloop 0.1.49. Linux x86-64,
Intel Core i9-9900K, CPython 3.14.7, uvloop 0.22.1, zuvloop 0.0.14. The kernel
permits io_uring; tracing confirms rsloop uses it on this host. CPU affinity is
0–15; frequency is not fixed and host services remain active. No builds, tests,
or profiling ran concurrently with timed samples.

[Recorded results](results/systems-performance-2026-09-07.json) contain binary
and source hashes, process-run samples, environment information, and bootstrap
comparisons. Full local diagnostics are in `target/systems-performance/`.

**What is actually slower?** The final release build gives these medians in the
existing standard microbenchmark runner (seven fresh-process measurements after
two warmups, default counts, rsloop native fast streams):

| Workload; elapsed milliseconds | rsloop | uvloop | zuvloop |
|---|---:|---:|---:|
| 200,000 callbacks | 47.94 | 51.45 | 36.72 |
| 50,000 tasks | 85.99 | 90.11 | 80.20 |
| 5,000 TCP stream round trips | 83.18 | 127.63 | 107.17 |

Rsloop remains 31% slower than zuvloop for this callback batch and 7% slower
for tasks, while beating uvloop in all three rows. The separate focused probe
still shows raw `sock_recv`/`sock_sendall` exchanges trailing both competitors:
71.48 ms for rsloop versus 51.41/50.29 ms for uvloop/zuvloop. Different callback
counts and garbage-collection settings mean the focused and standard runners'
absolute callback times should not be compared to each other.

The final sustained plaintext matrix also retains a clear workload split.
Traffic-only operations/second, 16 connections, 500 requests per connection;
seven measurements after two warmups in each loop/scenario process:

| Workload | rsloop | uvloop | zuvloop |
|---|---:|---:|---:|
| HTTP keep-alive | 54,596 | 51,261 | 57,739 |
| websockets | 23,545 | 25,563 | 27,642 |
| aiohttp WebSocket | 31,020 | 33,162 | 35,308 |
| Mixed streams | 43,694 | 35,782 | 35,652 |

These sequential competitor comparisons describe this build and host; they are
not evidence of a before/after effect. See the [earlier full comparison](full-benchmark-9011c9f.md)
for the broader TLS, bulk, and idle matrix. Idle activation was not remeasured.

**Why the gap exists.** Source inspection and syscall measurements support
different explanations for different workloads:

- Callback allocation: rsloop's [context capture](../src/context.rs) calls
  `PyContext_CopyCurrent` for every implicit context. This is a context-object
  capture, not a deep copy of all context variables. Zuvloop 0.0.14
  [defers empty-context allocation and recycles eligible empty contexts](https://raw.githubusercontent.com/Kludex/zuvloop/v0.0.14/zig/context.zig).
  Its [variable-size handles](https://raw.githubusercontent.com/Kludex/zuvloop/v0.0.14/zig/handle.zig)
  also store all positional arguments inline; rsloop avoids the tuple for zero
  or one argument, but still allocates it for multiple arguments. This is a
  plausible explanation for part of the scheduling gap, not a measured
  percentage attribution. The new patches do not optimize callback allocation.
- Protocol delivery: a [Rust reader task](../src/transport/stream/reader_task.rs)
  reads into a pooled `Vec`, queues a transport event, schedules a Python-loop
  ready item, and [drains it](../src/transport/stream/core_events.rs) into the
  protocol. That involves pool/transport/queue mutexes and ownership transfers.
  Generic protocols then receive a new Python `bytes`; BufferedProtocol still
  copies from the Rust buffer into its exported Python buffer.
  [Uvloop's buffered allocator](https://raw.githubusercontent.com/MagicStack/uvloop/v0.22.1/uvloop/handles/stream.pyx)
  gives the exported Python buffer directly to libuv. Its write contexts also
  retain multiple buffers for vectored writes. Rsloop's native fast streams
  bypass some protocol costs, explaining why their result differs.
- Redundant notifications: the original read pool called `Condvar::notify_all`
  on every buffer return, even with spare slots. For 8,000 WebSocket exchanges,
  `strace -f -c` counted 16,407 futex calls in rsloop and 22 in zuvloop. The
  pool-only patch reduced rsloop to 349. These are whole-process syscall counts,
  including setup/teardown, not CPU-time percentages. Blocked helper threads
  dominate strace's cumulative time, so those percentages would be misleading.
- Extra I/O attempts: that same trace counted 32,637 reads, including 16,080
  errors, in rsloop, versus 16,590 reads in zuvloop. Rsloop's reader retries
  until `WouldBlock` before awaiting readiness. Investigating readiness-aware
  read batching is justified, but simply dropping retries could strand data
  with edge-triggered readiness; this patch does not change that behavior.
- Raw socket waits: [socket_operation.rs](../src/bindings/loop_api/socket_operation.rs)
  invokes Python socket methods and sets up an owned descriptor duplicate,
  Rust readiness task, cancellation state, and Python completion callback for
  a pending operation. That work remains after the earlier socket optimization.
- Numeric resolution: public `getaddrinfo` previously dispatched through
  `run_in_executor` even when no name lookup was needed. The new fast path
  eliminates that scheduling and synchronization entirely for eligible inputs.

Hardware/software perf sampling was denied by the host's perf policy. A native
py-spy attempt fell behind and had poor symbol resolution; no CPU-percentage
claims are based on that capture. The strongest causal evidence here is the
isolated pool A/B plus its eliminated syscalls, and the numeric resolver A/B.

**Implemented changes and their limits.** The
[read-pool release](../src/transport/stream/buffers.rs) now checks whether the
pool was exhausted under its existing mutex. It notifies only when a release
makes capacity available. Both recycling a buffer and discarding an oversized
buffer free capacity. Close still unconditionally wakes waiters. Blocking
waiters check the condition under that mutex; async waiters retain their
register-then-recheck protocol. Tests cover both waiter types and release before
async registration.

The [numeric resolver](../src/bindings/loop_api/executor.rs) constructs the
single TCP/UDP result for an exact Python string containing a normal IP literal,
an exact integer port in 0–65535, matching/unspecified family and protocol, and
zero resolver flags. It returns a completed asyncio Future. Hostnames, bytes,
string/service ports, scoped IPv6, unspecified socket types, flags and invalid
combinations continue through the existing resolver. Custom default executors,
executor shutdown and loop subclass overrides also retain their existing path.
Eligible numeric requests no longer invoke a monkeypatched `socket.getaddrinfo`.
This uses no private CPython layouts and keeps the public loop API unchanged.

Nine fresh-process blocks with rotating before/after/uvloop/zuvloop order,
after a discarded first block, measured 10,000 IPv4/TCP lookups:

| Probe; elapsed milliseconds | rsloop before | rsloop after | uvloop | zuvloop |
|---|---:|---:|---:|---:|
| Numeric `getaddrinfo` | 620.07 | 6.96 | 11.38 | 10.50 |

The ratio of medians is **89.1× faster**, or 98.9% less elapsed time. A paired
process-run log-ratio bootstrap gives a 95% interval of −98.899% to −98.857%.
These immediate completions remove executor round trips; this is not a claim
of a comparable speedup for DNS names or persistent HTTP traffic. Focused
callback and raw-socket before/after comparisons are inconclusive at 3%.

The isolated pool experiment used nine alternating A/B process blocks, each
with two discarded warmups and one measurement. Changes below are geometric
mean ratios of **traffic time**, with paired process-run bootstrap intervals;
negative is better:

| Workload | Change | Approximate 95% interval |
|---|---:|---:|
| HTTP keep-alive | −3.6% | −5.7% to −1.3% |
| TLS HTTP | −7.1% | −12.3% to −2.5% |
| websockets | −0.4% | −2.1% to +1.2% |
| websockets TLS | −0.3% | −3.0% to +3.5% |
| aiohttp WebSocket | −1.6% | −3.9% to +0.1% |
| aiohttp WebSocket TLS | +0.1% | −2.4% to +3.1% |
| Starlette WebSocket | +0.4% | −1.6% to +2.5% |
| Mixed streams | −1.4% | −3.9% to +1.4% |
| Bulk transfer | +2.8% | −1.5% to +9.3% |

No entire interval clears the preselected 3% improvement/regression threshold.
HTTP suggests a benefit; the measurements do not establish a general network
speedup or rule out regressions, particularly for the short bulk workload.
Run-level p95/p99 and RSS are retained for review, not pooled into independent
request-level samples.

A separate seven-block experiment disabled `RSLOOP_WAKE_SPIN_US` on the
baseline. WebSocket traffic time fell about 2–3%, but mixed-stream and bulk
uncertainty remained. The 50 μs default and cooldown are **unchanged**.

**Correctness finding.** The 32 MiB protocol/writelines probe intermittently
fails its exact-byte-count check on both the original baseline and candidate.
One baseline reproduction received 33,546,418 rather than 33,554,432 bytes at
`connection_lost`. The failure reproduced on iteration 4 of a repeated baseline
probe. Its underlying cause remains unresolved. No successful-only averages
from these failed probe series are included in the results. The standard test
suite passes, which does not invalidate this additional failure.

The next transport change should first reproduce and fix that short-transfer
issue. Then measure a loop-owned protocol receive path, direct
BufferedProtocol reads, and retained vectored writes separately. For scheduler
work, measure allocation and context capture separately from invocation; retain
explicit-context isolation, weakref behavior and free-threaded correctness.
Replacing io_uring or changing language would not by itself remove these costs.

**Validation and reproduction.** Final checks: 306 Rust tests passed; 233 Python
tests passed and two skipped; `cargo clippy --all-targets --locked -- -D warnings`,
Rust formatting and Ruff checks passed. Execution was tested on GIL-enabled
Linux CPython 3.14; other interpreter/platform combinations were not executed.

```bash
# Save the release baseline package AND dist-info in a separate directory first.
.venv/bin/maturin develop --release --locked
.venv/bin/python benches/optimization_probes.py \
  --baseline-pythonpath target/systems-performance/baseline --repeat 9 \
  --scenarios numeric_dns,callbacks_zero,callbacks_one,raw_socket
.venv/bin/python benches/optimization_matrix_ab.py \
  --baseline-pythonpath target/systems-performance/baseline --repeat 9
.venv/bin/python benches/compare_event_loops.py \
  --loops rsloop,uvloop,zuvloop --repeat 7 --warmups 2
.venv/bin/python benches/workload_matrix.py --loops rsloop,uvloop,zuvloop \
  --scenarios http_keepalive,websockets_messages,aiohttp_websocket_messages,mixed_streams \
  --sustained
strace -f -c .venv/bin/python benches/workload_matrix.py --child \
  --loop rsloop --scenario websockets_messages --requests-per-connection 500
# Repeat this to reproduce the pre-existing transfer failure:
.venv/bin/python benches/optimization_probes.py --child rsloop_before writelines \
  --baseline-pythonpath target/systems-performance/baseline
```

The pool-only A/B preceded the numeric patch. Its exact scenario list adds
`starlette_websocket_messages` to the eight defaults of `optimization_matrix_ab.py`.
The final competitor matrix includes both patches. Benchmark subprocess errors
now print their stderr before failing, so failed probe output is visible.
