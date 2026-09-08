# Granian on Python 3.15: benchmark and sampling profile

Measured on 2026-09-08 using Python 3.15.0rc2, Granian 2.8.1, uvloop 0.22.1,
and rsloop 0.1.50 built in release mode with the `io_uring` reactor. The host
was Linux 7.0 on an Intel Core i9-9900K (8 cores, 16 hardware threads).

This is a short local diagnostic, not a release-performance claim. Each result
is the median of three five-second loopback runs at concurrency 128 with one
Granian worker, one runtime thread, the asyncio task implementation, HTTP/1,
and a fixed 10 KiB response. Longer randomized or alternating runs are needed
before accepting a small optimization.

## Benchmark result

```bash
uv run --python 3.15 --with granian --with uvloop \
  python benches/compare_granian.py \
  --loops asyncio,uvloop,rsloop \
  --warmup-duration 2 --duration 5 --repeat 3 \
  --json-output target/granian-py315.json
```

| Loop | Requests/s | p50 | p95 | p99 |
| --- | ---: | ---: | ---: | ---: |
| asyncio | 80,658 | 1.514 ms | 2.094 ms | 2.546 ms |
| uvloop | 95,663 | 1.366 ms | 1.605 ms | 1.861 ms |
| rsloop | 99,851 | 1.229 ms | 1.528 ms | 1.629 ms |

In this run rsloop delivered 4.4% more requests/s than uvloop and 23.8% more
than asyncio. Its median p50, p95, and p99 were respectively 10.1%, 4.8%, and
12.4% lower than uvloop's. One uvloop run was substantially slower than its
other two, so the exact gaps should be treated as directional.

## Python 3.15 sampling profiler

Python 3.15 adds the PEP 799 `profiling.sampling` package. It samples a process
externally and supports CPU, wall, GIL, and exception modes, all-thread
sampling, async-aware stacks, native-boundary markers, and several output
formats. The default rate is 1 kHz. See the official
[Python 3.15 overview](https://docs.python.org/3.15/whatsnew/3.15.html#whatsnew315-sampling-profiler)
and [sampling-profiler reference](https://docs.python.org/3.15/library/profiling.sampling.html).

Granian runs the application in a worker process. Start the example, identify
that worker PID, generate load from another terminal, and attach to the worker:

```bash
uv run --python 3.15 --with granian --with uvloop \
  python examples/granian_service.py --event-loop rsloop --no-log

# In another terminal, with WORKER_PID set to Granian's application worker:
python3.15 -m profiling.sampling attach \
  --mode gil --blocking --sampling-rate 1khz --duration 5 --native \
  --pstats --output target/rsloop-granian-gil.pstats "$WORKER_PID"

oha --no-tui -c 128 -z 8s --output-format json \
  http://127.0.0.1:8000/benchmark >/dev/null
```

The profiler and target must use the same Python version. On Linux, attaching
can also be denied by the system's ptrace policy. Follow the profiler's error
message or run the target as a profiler child; do not weaken a shared host's
ptrace policy merely to collect this profile. `--blocking` freezes the target
while reading a stack and avoids torn async stacks, but adds overhead. The
documentation recommends sampling no faster than 1 kHz in this mode.

GIL-mode profiles were collected for rsloop and uvloop at 1 kHz for five
seconds while `oha` was active. The profiler reported 32% and 27% sampling
errors respectively, largely from samples where the worker did not hold the
GIL, so the values below are approximate shares of retained samples rather
than end-to-end CPU percentages.

| Python-visible site | rsloop | uvloop | Interpretation |
| --- | ---: | ---: | --- |
| ASGI response sends in `granian_service.py` | 43.9% | 44.3% | Shared application/protocol path |
| Granian `loop.run_until_complete` boundary | 16.0% | 15.8% | Native event-loop work is below this frame |
| Granian `_run` / `loop.create_task` | 9.6% | 9.7% | One task is created for each scheduled request |
| Granian `_schedule` / `loop.call_soon_threadsafe` | 7.1% | 6.0% | Cross-thread handoff into the selected loop |
| Granian ASGI callback wrapper | 5.5% | 4.5% | Shared wrapper overhead |

The close distributions do not reveal a Python-level rsloop regression. The
sampling profiler can show the native boundary with `--native`, but it cannot
attribute time among rsloop's Rust functions. CPU-mode sampling without
`--blocking` had a 74% error rate here; CPU mode with `--blocking` produced no
retained samples with this release-candidate build. Those profiles were not
used for optimization conclusions.

## Performance opportunities

1. **Measure the cross-thread ready-queue handoff in Rust.** Granian calls
   `loop.call_soon_threadsafe()` once per request, followed by
   `loop.create_task()`. rsloop already uses FASTCALL/vectorcall for both, but a
   cross-thread callback still allocates a Python `Handle`, captures context,
   locks `active_ready_dispatch`, locks its pending `VecDeque`, and signals the
   loop. Existing Tracy spans should be extended around those individual
   stages, then the Granian load should be repeated with a profiler-enabled
   build. A stable active-dispatch reference or a lower-contention FIFO is
   worth testing only if those spans confirm contention; FIFO ordering and
   run/stop lifecycle safety must be preserved.

2. **Do not prioritize a custom task path yet.** Switching Granian from
   `--task-impl asyncio` to `--task-impl rust` changed rsloop's median
   throughput by only +0.4% and worsened median p99 by 13.9% in this short run.
   That is not evidence of a win, and the Python profile shows nearly identical
   task-creation shares for rsloop and uvloop.

3. **Tune Granian runtime threads for the deployment.** Two runtime threads
   produced 103,706 requests/s here, roughly 4--6% above the one-thread runs,
   with slightly worse latency. Four threads fell to 96,088 requests/s. This is
   a Granian configuration result, not an rsloop core optimization, and should
   be retested with the target workload and CPU affinity before changing a
   default.

4. **Keep the current wake-spin default pending stronger evidence.** Disabling
   `RSLOOP_WAKE_SPIN_US` or raising it from 50 to 100 microseconds did not
   improve this workload consistently. The existing 50-microsecond default
   remains the best-supported setting from these runs.

5. **Improve native observability before changing the reactor.** Python 3.15
   is built with frame pointers, but its documentation warns that third-party
   native build systems must preserve equivalent flags for complete native
   unwinding. A profiling-only rsloop build with Rust frame pointers
   (`-C force-frame-pointers=yes`) would make system-profiler stacks more useful.
   The host used for this test had `perf_event_paranoid=4`, so native `perf`
   attribution was unavailable; no reactor or allocator change is justified by
   the Python-only samples.

The highest-value next experiment is therefore a Tracy-instrumented Granian
run focused on `schedule_callback_args`, the active ready-queue locks, wake
signalling, and ready-handle execution. The application and Granian wrapper
consume most Python-visible GIL samples, while rsloop is already the fastest
loop in the end-to-end comparison.
