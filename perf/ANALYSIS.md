# rsloop behind Granian: cross-thread wakes, turn shape and name resolution

This branch is based on upstream v0.1.65 (`37a3d3f`). It changes four
things in the loop and fixes one lost-wake race:

| commit | change |
|---|---|
| `fix: fence the block_on park against a wake that races it` | a SeqCst fence between the park's store of `waiting` and its load of the ready flag, and on both notifier sides |
| `perf: enqueue off-thread call_soon_threadsafe directly into the active run` | one wake per off-thread handle instead of a hop through the coordination thread |
| `perf: park instead of spinning for cross-thread wakes by default` | `RSLOOP_WAKE_SPIN_US` defaults to 0 (was 50) |
| `perf: run one callback generation per turn and poll I/O without the GIL` | asyncio's `_run_once` turn shape, with `call_soon` / `call_soon_threadsafe` kept in scheduling order |
| `perf: resolve host names natively, without the GIL` | `loop.getaddrinfo()` calls `getaddrinfo(3)` on the blocking pool without the GIL, at most `min(32, CPUs + 4)` at once |

The investigation started on v0.1.64, where a Litestar web service behind
Granian, with an async SQLAlchemy/asyncpg pool, ran up to 44% slower on rsloop than on uvloop. Sections 2–4 keep
that investigation because it explains each change; every code path it
names is still present in v0.1.65.
[Section 1](#1-results-against-v0165-and-uvloop) compares this branch with
v0.1.65 and uvloop.

## Summary

- **v0.1.65 still has the defect.** In the pool repro (a small SQLAlchemy pool,
  8 Granian-style driver threads posting through `call_soon_threadsafe`)
  v0.1.65 opens 60–74 new Postgres connections per 1k requests and runs at
  439 req/s, against uvloop's 0.0–0.1 and 1,747 req/s. This branch opens
  0.8–1.8 per 1k and runs at 1,427 req/s, at 652 µs of CPU per request vs
  v0.1.65's 2,152 and uvloop's 552.
- **Root cause:** the loop thread spun for up to 50 µs, with the GIL
  released, waiting for a cross-thread wake. Behind Granian those wakes come
  from Python threads that still hold the GIL, so the spinning loop thread
  queued for the GIL behind them, and request starts and completions reached
  the loop in bursts. A small SQLAlchemy pool then kept discarding returned
  connections into a full idle queue and opening new ones (a SCRAM login
  each).
- **Second cause:** each turn ran to quiescence before polling I/O, so under
  saturation database responses completed in bursts larger than the pool's
  idle queue.
- **Third cost:** every `getaddrinfo` took the GIL twice on a blocking
  worker; a burst of 1,000 lookups grows v0.1.65's pool to its 500-thread
  limit.
- **Microbenchmarks against v0.1.65** (all three arms interleaved, under
  load): cross-thread ping-pong 15.3 → 9.9 µs CPU per op (uvloop 10.6),
  Granian-shaped requests 62.4 → 51.8 µs (uvloop 64.1), single-connection
  TCP 17.7 → 10.8 µs (uvloop 11.7), `getaddrinfo` 99.5 → 33.7 µs (uvloop
  47.8). Costs: a self-rescheduling `call_soon` chain 0.20 → 0.51 µs and a
  cancelled timer 1.52 → 2.21 µs, both still below uvloop (0.79, 2.61).
  Upstream's `benches/compare_event_loops.py` (callbacks, tasks, TCP) is
  level.
- **A lost-wake race** in the runtime park, present in v0.1.64 and v0.1.65,
  is fixed; its new test stalls on v0.1.65 in 2 of 3 runs.
- **Full-service comparison against v0.1.65** (uvloop, v0.1.65, this branch;
  five endpoints, c16 and c64, 3 rotated rounds): at c16 the branch is ahead
  of v0.1.65 on four of five rows (+17.5% to +64.9%) with 3–8 new Postgres
  sessions per 1k requests against v0.1.65's 27–53, and within noise of
  uvloop on all five. The machine was not quiet, so only the "ahead" cells
  are differences; see
  [the run](#full-service-run-against-v0165).

## Setup

- **The service.** A Litestar web service behind Granian (2 workers),
  backed by Postgres (SQLAlchemy async + asyncpg), Redis and NATS. Its
  endpoints are authenticated JSON reads, named here by workload (two session
  reads, an aggregate read, a count read, a list read, and a health probe
  that opens a NATS connection); the routes themselves stay with the
  application.
- **The measured stack.** Litestar 2.23.0, Granian 2.7.5, SQLAlchemy 2.0.50
  (async engine, `QueuePool`) over asyncpg 0.31.0, redis-py 5.3.1 and
  nats-py 2.15.0; uvloop 0.22.1 is the reference loop and oha the load
  generator. What each showed:
  - Granian hands every request to the loop with `call_soon_threadsafe` from
    its own threads, which still hold the GIL when the wake flag flips: the
    wake pattern behind the root cause.
  - SQLAlchemy's `QueuePool` turned the resulting bursts into connection
    churn, and every new asyncpg connection is a SCRAM login
    (`_generate_salted_password`, `libcrypto`): that is where the extra CPU
    went.
  - nats-py and asyncpg connect by host name, so each connect goes through
    `loop.getaddrinfo` (the health probe opens a NATS connection per
    request).
  - No effect of Litestar's or redis-py's own was isolated; every arm ran
    the same application code.
- **The machine.** One desktop machine with an i7-13650HX (6 hyper-threaded
  P-cores, CPUs 0–11, and 8 E-cores, CPUs 12–19) and 31 GiB, with other work
  running on it.
- **Harness** (`perf/bench/harness.py`).
  - Granian 2.7.5 with 2 workers, CPython 3.14.5, the API container capped
    at `--cpus 2`, oha in its own `--cpus 2` container on the same Docker
    network.
  - 15 s warm-up, then 60 s at c16 and at c64 per endpoint.
  - Arms are interleaved and rotated per round; all arms use the same image
    and application code, and only the event loop differs. A rsloop build is
    selected by putting its unpacked wheel first on `PYTHONPATH`; every
    start row records which modules the arm imported and the sha256 of the
    native module.
  - In Docker the loop runs on **epoll** (the default seccomp profile
    refuses `io_uring_setup`).
  - The harness also reports CPU per 1k requests (the API container's
    cgroup) and new Postgres sessions per 1k requests
    (`pg_stat_database.sessions`), the counter that exposed the root cause.
- **Quiet gate** (`perf/bench/quiet.py`). Before every row the harness
  waits until CPU used outside the benchmark's containers stays at or below
  2.0 cores over a 30 s probe, and measures that foreign CPU over the row
  itself; a row above 2.5 cores is discarded and measured again.
- **Builds.** Release wheels (`maturin build --release --locked`) of
  `37a3d3f` and of this branch, built from `git archive` in a
  `python:3.14.5-slim` builder image with the repository's toolchain.

## 1. Results against v0.1.65 and uvloop

### Pool repro (`perf/bench/pool_churn.py`)

A small-pool database shape on one loop: SQLAlchemy async engine with
`pool_size=4, max_overflow=2` against Postgres, 8 driver threads each
posting a request through `loop.call_soon_threadsafe` and blocking on its
reply, 2 queries per request, 10 s per run. Docker epoll, `--cpus 1`, 3
runs per arm. Measured with other builds running on the machine (load1
13–25); the connect count is a mechanism count and does not depend on that
load.

| arm | req/s median (min–max) | CPU µs/req | new connections /1k | post → task start µs | reply → driver resume µs |
|---|---|---|---|---|---|
| uvloop 0.22.1 | 1,747 (1,627–2,260) | 552 | 0.0 (0.0–0.1) | 254 | 286 |
| rsloop 0.1.65 | 439 (272–479) | 2,152 | **65.9** (59.9–74.2) | 2,359 | 1,769 |
| this branch | 1,427 (1,412–2,135) | 652 | 1.4 (0.8–1.8) | 395 | 309 |

### Microbenchmarks (`perf/bench/micro.py`)

Docker epoll, `--cpus 2`, 3 rounds with the arm order rotated, medians; CPU
µs per op, wall µs in parentheses. Measured under load (load1 10.8–20.6), so
read differences of a few percent as noise.

| bench | uvloop 0.22.1 | rsloop 0.1.65 | this branch |
|---|---|---|---|
| `xthread_pingpong` (thread posts `call_soon_threadsafe`, waits for the callback) | 10.55 (9.25) | 15.32 (9.12) | **9.89** (7.95) |
| `xthread_task_pingpong` (thread starts a task, the task answers through a queue) | 11.26 (11.27) | 13.09 (8.27) | **9.27** (9.01) |
| `granian_like` (8 requests in flight: thread post → task with 2 DB round trips → thread-safe reply) | 64.05 (87.04) | 62.39 (84.48) | **51.82** (81.09) |
| `soon_chain` (self-rescheduling `call_soon`) | 0.79 | **0.20** | 0.51 |
| `timers_cancel` (`asyncio.timeout` around a resolved await) | 2.61 | **1.52** | 2.21 |
| `tcp_pingpong` (one connection) | 11.70 (17.17) | 17.68 (22.70) | **10.84** (16.21) |
| `tcp_pingpong_c8` (8 connections) | 8.48 (10.38) | **7.53** (9.31) | 8.00 (9.69) |
| `resolve_name` (`getaddrinfo("localhost")`, one at a time) | 47.76 (47.66) | 99.47 (79.89) | **33.67** (33.03) |
| `resolve_name_c16` (16 at once) | **36.53** (14.74) | 108.60 (51.50) | 39.73 (16.34) |
| `executor_noop` (`run_in_executor(None, int)`) | **22.84** (20.31) | 37.51 (20.00) | 23.34 (22.04) |
| `connect_by_name` (`open_connection("localhost", ...)`, one round trip, close) | **269.9** (373.9) | 385.7 (482.0) | 284.6 (397.6) |

- `soon_chain` and `timers_cancel` are the cost of the turn shape: a
  callback that reschedules itself now waits for the next turn and its
  non-blocking reactor poll, as on asyncio and uvloop.
- `executor_noop` gains although `run_in_executor` itself is unchanged: on
  v0.1.65 the parked-loop wake path costs more CPU (the spin).

Upstream's own `benches/compare_event_loops.py` (pinned to one CPU, 5
measured runs after 1 warm-up, 1,000,000 chained callbacks, 200,000 tasks in
batches of 5,000, 20,000 TCP round trips of 1,024 bytes; median seconds, two
passes per build, interleaved):

| workload | rsloop 0.1.65 | this branch |
|---|---|---|
| callbacks | 0.171 / 0.168 | 0.177 / 0.163 |
| tasks | 0.267 / 0.289 | 0.275 / 0.262 |
| tcp_streams | 0.202 / 0.206 | 0.208 / 0.209 |

All three are level within their pass-to-pass spread.

### Name-resolution bursts

1,000 concurrent `loop.getaddrinfo("localhost", port)` on the host (20
CPUs, cap 24), peak threads of the blocking pool, 2 runs each: v0.1.65 500
threads (smol's limit) in 114–123 ms; this branch 24 threads in 29–30 ms
(25 in 1 of 20 further runs: the pool can start a thread while one that
just released its slot is not yet idle). Pinned to 2 CPUs (cap 6), the
branch peaks at 6.

### Full-service run against v0.1.65

Three arms (uvloop, v0.1.65 `37a3d3f`, this branch), five endpoints, c16 and
c64, 15 s warm-up and 60 s per row, 3 rounds with the arm order rotated, the
API container pinned to two physical P-cores and oha and Postgres/Redis/NATS to
others. Median rps with the min–max over the 3 rounds; "ahead" means the
branch's range lies entirely above the other arm's, "overlap" that the two
ranges overlap:

| endpoint | c | uvloop | v0.1.65 | branch | branch vs v0.1.65 | branch vs uvloop | new PG sessions /1k uvloop / v0.1.65 / branch |
|---|---|---|---|---|---|---|---|
| session read 1 | 16 | 1,499 (1,420–1,703) | 1,200 (997–1,374) | 1,424 (1,316–1,650) | +18.7% (overlap) | −5.0% (overlap) | 3.4 / 27.7 / 3.3 |
| session read 1 | 64 | 1,443 (1,194–1,476) | 1,101 (1,033–1,120) | 1,628 (1,469–1,654) | +47.8% (ahead) | +12.8% (overlap) | 3.0 / 27.9 / 4.8 |
| aggregate read | 16 | 949 (821–1,019) | 792 (789–822) | 930 (917–968) | +17.5% (ahead) | −1.9% (overlap) | 5.4 / 28.0 / 7.5 |
| aggregate read | 64 | 814 (786–971) | 913 (796–923) | 939 (909–966) | +2.8% (overlap) | +15.4% (overlap) | 4.6 / 11.0 / 5.7 |
| session read 2 | 16 | 1,650 (1,595–1,723) | 1,078 (1,025–1,083) | 1,779 (1,700–1,970) | +64.9% (ahead) | +7.8% (overlap) | 5.1 / 53.4 / 2.8 |
| session read 2 | 64 | 1,765 (1,717–1,823) | 1,818 (1,787–1,952) | 1,925 (1,726–1,927) | +5.9% (overlap) | +9.1% (overlap) | 2.5 / 2.3 / 2.3 |
| count read | 16 | 1,335 (1,319–1,605) | 997 (977–1,044) | 1,401 (1,400–1,507) | +40.4% (ahead) | +5.0% (overlap) | 10.7 / 33.4 / 3.7 |
| count read | 64 | 1,443 (1,224–1,598) | 1,503 (1,464–1,525) | 1,450 (1,291–1,509) | −3.5% (overlap) | +0.5% (overlap) | 3.0 / 2.4 / 2.5 |
| list read | 16 | 1,263 (1,162–1,280) | 922 (915–1,058) | 1,194 (1,091–1,228) | +29.5% (ahead) | −5.5% (overlap) | 8.7 / 26.9 / 4.0 |
| list read | 64 | 1,287 (1,152–1,304) | 1,210 (1,079–1,246) | 1,175 (1,137–1,257) | −2.9% (overlap) | −8.7% (overlap) | 3.4 / 3.6 / 3.8 |

(Session reads 1 and 2 are light GETs on different routes, aggregate read a
GET with a DB aggregate, count read a GET with a small DB count, list read a
GET with a small DB list.)

This run was not quiet: other builds were running on the 20-CPU host (load1
3.1–9.7, 1.5–5.9 cores of CPU outside the benchmark), and 77 of the 90 rows
were above the 2.5-core quiet threshold, so only the "ahead" cells are
differences. At c16 the branch uses 14–39% less CPU per request than
v0.1.65; at c64 four of the five rows are within noise of v0.1.65.

### Full-service run on the v0.1.64 base

The last quiet-gated run of all five endpoints measured the same
changes on the v0.1.64 base (before later fixes to name resolution
and before v0.1.65's rework of `blocking::run`, which the branch now uses): uvloop,
rsloop 0.1.64 and the fork, 3 rotated rounds, 60 s per row, every row with
≤ 2.0 foreign cores (192 clean rows, none discarded). Median rps:

| endpoint | c | uvloop | rsloop 0.1.64 | fork | fork vs uvloop | new PG sessions /1k uvloop / 0.1.64 / fork |
|---|---|---|---|---|---|---|
| session read 1 | 16 | 1,787 | 1,636 | 1,799 | +0.7% | 2.9 / 17.9 / 2.9 |
| session read 1 | 64 | 1,668 | 1,525 | 1,827 | +9.5% | 2.7 / 17.0 / 5.1 |
| aggregate read | 16 | 1,069 | 940 | 1,070 | +0.1% | 6.5 / 33.5 / 8.1 |
| aggregate read | 64 | 1,029 | 944 | 1,052 | +2.3% | 4.3 / 27.5 / 4.2 |
| session read 2 | 16 | 2,208 | 1,172 | 2,222 | +0.6% | 2.9 / 55.7 / 1.4 |
| session read 2 | 64 | 2,190 | 2,348 | 2,172 | −0.8% | 1.4 / 2.7 / 2.0 |
| count read | 16 | 1,771 | 1,138 | 1,689 | −4.6% (ranges overlap) | 3.2 / 46.3 / 2.8 |
| count read | 64 | 1,745 | 1,821 | 1,744 | −0.1% | 2.2 / 4.9 / 2.5 |
| list read | 16 | 1,349 | 1,057 | 1,445 | +7.2% | 9.2 / 40.1 / 4.0 |
| list read | 64 | 1,405 | 1,510 | 1,416 | +0.8% | 2.8 / 3.2 / 3.1 |

On the v0.1.64 base the c16 rows rsloop lost by 22–47% were level with
uvloop, at uvloop's CPU and connection churn per request; the session read 1
and aggregate read c64 rows were ahead. v0.1.64 beat both on the session
read 2, count read and list read c64 rows, where its spin pays off
in a saturated container (see [Remaining differences](#6-remaining-differences)).
To repeat the run against v0.1.65:
`harness.py api <api> 1` with
`BENCH_ARMS=uvloop,fork:<v0.1.65 site>,fork:<branch site>`, three rounds
(`BENCH_ROUND_BASE=0..2`), then `compare.py`.

## 2. How the gap was found (v0.1.64)

Baseline on v0.1.64 (3 rounds, median rps; load1 2.2–6.2):

| endpoint | c | uvloop | rsloop 0.1.64 | Δ | CPU-ms/1k uvloop / rsloop |
|---|---|---|---|---|---|
| session read 2 | 16 | 2,057 | 1,150 | −44.1% | 972 / 1,740 |
| session read 2 | 64 | 1,965 | 2,150 | +9.4% | 1,018 / 931 |
| count read | 16 | 1,543 | 1,093 | −29.1% | 1,295 / 1,830 |
| count read | 64 | 1,568 | 1,626 | +3.7% | 1,276 / 1,231 |
| list read | 16 | 1,281 | 1,022 | −20.2% | 1,559 / 1,957 |
| list read | 64 | 1,289 | 1,369 | +6.2% | 1,551 / 1,462 |
| session read 1 | 16 | 1,689 | 1,888 | +11.7% | 1,182 / 1,051 |
| aggregate read | 16 | 1,003 | 881 | −12.2% | 1,984 / 2,256 |

At c16, rps × CPU-ms per request ≈ 2,000 ms/s for both loops: the container
is pinned at its 2-CPU quota, so the throughput gap is a CPU-per-request
gap (about 535 µs per request on the count read).

**Where the CPU went** (count read c16; per-thread
CPU from `/proc`, `perf record -g`, `strace -c`):

- The extra CPU was on the Python loop threads themselves. The coordination
  thread, rsloop's helper threads and Granian's tokio threads together
  accounted for less than 10 µs per request of the difference. rsloop
  context-switched less than uvloop (1.9 vs 4.1 per request): the busy-wait
  replaced sleeping.
- `libcrypto` held 8.0% of rsloop's cycles and 0.02% of uvloop's, and
  asyncpg's protocol module 6.4% vs 1.9%; the call graphs put SCRAM's
  `_generate_salted_password` at 8.6–9.6% of rsloop's samples. Postgres
  authenticates only when a connection is opened.
- The new-session counter confirmed it: 33–40 new Postgres sessions per 1k
  requests on rsloop, 4 on uvloop. `strace` showed the footprint of the
  reconnects (1,378 `close`, 1,652 `epoll_ctl`, 275 new threads in 10 s,
  against 6, 12 and 0 on uvloop).

**The mechanism, isolated** (`perf/bench/pool_churn.py`): the same small-pool
database shape on one loop without Granian or the application. A SQLAlchemy async
engine with `pool_size=4, max_overflow=2` against Postgres, 8 driver threads
each posting a request through `loop.call_soon_threadsafe` (as Granian
does) and blocking on its reply, 2 queries per request. Docker epoll,
`--cpus 1`, medians of 3:

| arm | req/s | CPU µs/req | connects /1k | post→task start µs | reply→driver resume µs |
|---|---|---|---|---|---|
| uvloop | 2,166 | 434 | 0.1 | 222 | 225 |
| rsloop 0.1.64 | 598 | 1,661 | 61.3 | 1,688 | 1,448 |
| + direct off-thread enqueue | 641 | 1,534 | 55.0 | 1,724 | 1,495 |
| + park instead of spin | **2,593** | **377** | **0.2** | **39** | **52** |

The pool trace showed connections coming back while the idle queue already
held 3–4 with no waiter (at least 343 check-ins discarded into a full queue
in 6 s, 265 connections created into an empty one), while the remaining
requests waited 1.4–1.7 ms between their driver thread and the loop.

Hypotheses tested on that repro:

| hypothesis | test | verdict |
|---|---|---|
| Every off-thread `call_soon_threadsafe` takes two wakeups (channel → coordination thread → run queue → loop thread). | cross-thread ping-pong microbenchmark; per-thread CPU | Confirmed, minor: 11.4 → 7.8 µs CPU per cross-thread callback; does not touch the churn (61 → 55 /1k). |
| The 50 µs loop-thread spin before parking hurts when wakes come from Python threads that hold the GIL. | `RSLOOP_WAKE_SPIN_US=0` on the same build | Confirmed, root cause: churn 55 → 0.2 /1k, post→start 1.7 ms → 39 µs, 4× req/s. |
| The spin starves the reactor (up to 64 consecutive caught spins without a poll). | variant polling the reactor after every caught spin | Refuted: still 55–82 connects /1k. |
| The spin burns the CFS quota and the cgroup is throttled. | same repro at `--cpus 2` and `--cpus 3` | Refuted: identical churn without a binding quota. |
| GIL convoy: the spinner sees the wake while its producer still holds the GIL and queues behind the driver threads (`take_gil` waits up to the 5 ms switch interval). A parked loop wakes after the producer let go and drains everything queued. | latency split above; no-spin variant | Consistent with every measurement. |
| Timer heap, ready-queue or transport costs. | timer, `call_soon` and TCP microbenchmarks | Refuted: rsloop was faster than uvloop on timers and `call_soon`, level with 8 connections. |
| Each turn runs to quiescence before polling the reactor, so I/O completions arrive in bursts larger than a pool's idle queue. | pool trace inside the service (below) | Confirmed, second cause. |

**The second mechanism: completions in bursts.** Once the spin was
gone, some c64 rows (session read 1 among them) still opened 14–20 new
sessions per 1k requests (uvloop 0.1–3.7). An opt-in trace of the pool inside the service
(`perf/bench/sitecustomize.py`, `RSPERF_TRACE_POOL=1`) counted, in 20 s of
session read 1 at c64, about 690 check-ins into a full idle queue with 5–20
getters waiting, against at most 3 on uvloop. A `run_forever` turn took in
every callback it scheduled (refill every 64 items, up to 1024) and polled
the reactor only when nothing was left, so a whole chain of response →
request step → check-in → next response ran inside one turn. asyncio's
`_run_once` runs only what was ready when the iteration started and polls
I/O again before anything newer.

**The third cost: name resolution.** `loop.getaddrinfo()` of a host name
goes through `run_in_executor`: a worker takes the GIL to call
`socket.getaddrinfo` and again after the lookup, each time behind a busy
loop thread. Every nats-py connect and every new Postgres session resolves a
name. On v0.1.64 that also started a thread per lookup (v0.1.65 reuses
`smol`'s blocking threads); it cost the health probe ~250 µs of CPU
per request.

## 3. The changes

### Fence the park against a racing wake

- **Symptom.** In Docker (epoll), the #115 greenlet repro hung in about 1 of
  300–450 runs on every build, v0.1.64 included (2 of 440).
- **What was stuck.** A core dump of a hung run showed the loop thread
  cycling through its bounded parks and the coordination thread in
  `epoll_wait` with no timeout. The first step of a
  `run_until_complete` task, queued before `run_forever`, never ran: work
  queued before a run reaches it only through the coordination thread.
- **Root cause.** The waker stores the root's ready flag (or pushes onto the
  remote task queue), then loads `waiting`; the park stores `waiting`, then
  loads the ready flag. With release/acquire only, store-load reordering lets
  both loads read the old value: the waker skips the interrupt and the park
  sleeps through the wake. v0.1.65 restructured `BlockOnNotify` around
  `RemoteWakeContext` but kept the same handshake.
- **Fix.** A SeqCst fence between store and load on the park side and on
  both notifier sides; same-thread wakes skip it.
- **Tests.** `block_on_never_misses_a_wake_racing_with_the_park` and
  `spawned_task_never_misses_a_wake_racing_with_the_park` ping-pong 100,000
  wakes against the park on the mio driver and fail when progress stops for
  5 s. On v0.1.65 without the fences the first stalls in 2 of 3 runs (at
  rounds 20,765 and 79,563); with them both pass. On v0.1.64 the #115 repro
  then ran 2,200 times in Docker without a hang (5–7 expected at the old
  rate).

### Off-thread `call_soon_threadsafe` joins the active run directly

`LoopCore::schedule_ready_handle`: while a run is active and no earlier
handle is still on the command channel, an off-thread handle goes straight
into the run's pending queue and wakes the loop once. A counter of handles
in flight on the channel (released by the dispatcher when it forwards them)
keeps each producer's FIFO order across run start and end, and `finish_run`
puts the run's queue (older work) ahead of commands received since the last
dispatch. Test:
`tests/test_fast_callbacks.py::test_threadsafe_producer_order_survives_run_restarts`.

On v0.1.64 (Docker epoll, `--cpus 2`): cross-thread ping-pong 11.4 → 7.8 µs
CPU per op (uvloop 5.9), task ping-pong 10.4 → 6.9 (uvloop 6.9).

### Park instead of spinning by default

`wake_spin_window` defaults to 0; `RSLOOP_WAKE_SPIN_US` stays as an opt-in,
documented in `docs/configuration.md`. A spin only pays off when the waking
threads never hold the GIL.

On v0.1.64 (Docker epoll, `--cpus 2`, medians of 3): Granian-shaped
requests 54.6 → 41.1 µs CPU (uvloop 50.0), cross-thread ping-pong 14.1 →
6.5 µs (uvloop 7.6), single-connection TCP ping-pong 15.2 → 10.5 µs (uvloop
8.0), and the pool repro 598 → 2,593 req/s at 61.3 → 0.2 connects per 1k
(uvloop 2,166, 0.1).

### One callback generation per turn; poll I/O without the GIL

- `run_forever`: a turn runs the callbacks queued when it started; newer
  ones wait for the next turn, after a non-blocking reactor poll. That poll
  runs under `py.detach`, as uvloop polls without the GIL (with the GIL held,
  back-to-back turns kept it for the whole switch interval while
  `call_soon_threadsafe` producers waited, and the pool repro fell to 851
  req/s at 61 connects per 1k).
- A queued `stop()` keeps the turn going until its Stop item runs, so stop
  still takes effect in the turn that requested it.
- **Scheduling order.** The loop thread queues its own `call_soon` handles in
  a lock-free local queue and other threads queue into the run's pending
  queue. Every off-thread item records how many items the loop thread had
  queued for itself when it was queued (`LoopCore::local_enqueues`, written
  only by the loop thread), and each generation is merged from both queues
  on that mark (`merge_ready_generation`): an off-thread item goes behind
  exactly the local items queued before it. Marks are stored once per change,
  so a burst of off-thread items shares one; work queued before a run
  carries the mark read before the run could queue anything. v0.1.64 already
  failed the tightest case (50 alternating `call_soon` / thread-posted
  `call_soon_threadsafe` calls from one callback).
- **Tests.** `tests/test_local_sockets.py::test_tcp_data_is_delivered_between_call_soon_generations`
  (a self-rescheduling `call_soon` chain delayed a readable protocol by 513
  callbacks before, 1 after); `tests/test_fast_callbacks.py`:
  `test_earlier_call_soon_runs_before_later_threadsafe_call`,
  `test_call_soon_and_threadsafe_calls_interleave_in_scheduling_order` and
  `test_threadsafe_call_runs_before_later_call_soon`, each against asyncio
  and rsloop; Rust `generation_order_tests` for the merge.
- **Cost.** A self-rescheduling `call_soon` chain is now one turn per step,
  as in asyncio and uvloop. See [section 1](#1-results-against-v0165-and-uvloop)
  for what that does to the chained-callback benchmark.

On v0.1.64 with the two changes above, session read 1 at c64 went from
1,618 to 1,936 rps and from 20 to 1.3–3.0 new sessions per 1k (uvloop
1,746).

### Resolve host names natively, without the GIL

- `src/bindings/loop_api/resolver.rs`: on Unix, an exact loop resolves
  natively when it has no custom default executor, `socket.getaddrinfo` is
  still the standard library's function, and host and port are plain (an
  ASCII name the IDNA codec passes unchanged, or `bytes`; an `int` port in
  range, `str` or `bytes`). `getaddrinfo(3)` runs on the blocking pool
  without the GIL. The loop thread raises the `socket.getaddrinfo` audit
  event through CPython's C API (`PySys_Audit`, as `_socket` does, so a
  replaced `sys.audit` skips no hook) and builds the same rows (enum members, glibc's `inet_ntop` text) or
  the same `gaierror` / `OSError` (`EAI_SYSTEM` as `PyErr_SetFromErrno`
  raises it, errno 0 included). Every other shape keeps the executor path.
- A replaced or wrapped `socket.getaddrinfo` still receives every lookup.
  The standard function is recognised by what a `functools.wraps` wrapper
  cannot copy: its globals are the `socket` module's namespace, its code is
  named `getaddrinfo`, and it has no `__wrapped__`. The `_socket.getaddrinfo`
  it delegates to must be `_socket`'s builtin, so a replaced delegate also
  keeps the executor path. Both are checked on every lookup.
- `src/blocking.rs`: at most `min(32, CPUs + 4)` lookups run at once, the
  size of asyncio's default executor (CPUs from the affinity mask, as
  `os.process_cpu_count()` counts them); further lookups wait in order and
  `run_in_executor` jobs never wait for a lookup slot. Without the cap a
  burst of 1,000 lookups grew the pool to hundreds of threads.
- `src/engine/commands.rs`: a `LoopWork` ready item carries the loop-thread
  half of such work, so the worker never takes the GIL.
- **Tests.** `tests/test_native_addrinfo.py`: 25 argument shapes compared
  with `socket.getaddrinfo` (rows, enum types and errors, including the
  shapes left to CPython), 64 concurrent lookups, cancelled lookups, a lookup
  created before `run_until_complete`, the audit event (raised on the loop
  thread; a refusing hook fails the lookup, also with `sys.audit` replaced),
  a `functools.wraps` wrapper installed before the first lookup, a replaced
  `_socket.getaddrinfo` before and after native lookups, and the thread
  count under a burst.
  Rust: the bounded slots, glibc's IPv6 text, the IDNA fast path, the cap
  formula and the `EAI_SYSTEM` mapping.

## 4. What v0.1.65 already did, and what was dropped

- **Blocking workers.** The fork replaced v0.1.64's thread per
  `blocking::run` call with its own cached pool. v0.1.65 moved
  `blocking::run` onto `smol::unblock`, which reuses threads too, so the
  fork's pool was dropped; only the bounded lookup slots remain, on top of
  `smol::unblock`.
- **Fork safety, investigated and left alone.** `smol`'s blocking pool keeps
  its idle count in a static, so a job submitted in a child forked within
  500 ms of the parent's last blocking job waits for a thread that does not
  exist (a Rust test on `blocking::run` reproduces it). In Python, though,
  `run_in_executor` and `rsloop.run` already hang in a child forked after
  the parent ran any rsloop loop, on v0.1.64, v0.1.65 and this branch alike,
  so a fork-aware pool alone fixes nothing visible; it is not part of this
  branch.
- **Wake overhead.** v0.1.65's root-waker reuse, `block_on_until` and the
  turn counter are kept as they are; the changes above sit on top of them.

## 5. Correctness

All on this branch's head (release builds), on the host (io_uring) unless
noted:

| check | result |
|---|---|
| `cargo fmt --check`; `cargo clippy --all-targets --all-features --locked -- -D warnings` | clean (also on every commit of the series) |
| `scripts/run_rust_tests.py`, default and `--all-features` | 383 and 474 passed |
| Python core suite (`scripts/run_python_tests.py tests/test_*.py`, CPython 3.14.7) | 689 passed, 12 skipped; `tests/test_run.py::TestRun::test_run_fallback_handles_sigint` passes when run in a foreground process (it fails under a runner that ignores SIGINT) |
| free-threaded CPython 3.14.7t: `tests/test_native_addrinfo.py`, `tests/test_free_threading.py` | 37 passed |
| ruff, ruff format, pyright on the changed tests | clean; pyright reports one error in `tests/test_local_sockets.py:298`, which is upstream's own test and reports the same on master |
| #115 greenlet lost wakeup / SIGSEGV repro, every depth | 0 stuck, 0 SIGSEGV, 0 nonzero exits (55 host runs, 110 Docker epoll runs) |
| #116 protocol leak repro; stdlib `StreamReaderProtocol` churn; asyncpg connect/close churn | 0 protocols alive, +0–1 B per connection (host and Docker) |
| #118 cancelled `TimerHandle` retention (`repro.py`, `asyncpg_repro.py`) | 0/20000 alive in every case; 0 statement caches, states or protocols alive |
| wake-race stress tests on v0.1.65 without the fences | `block_on` stalls in 2 of 3 runs (rounds 20,765 and 79,563); with the fences both pass |

## 6. Remaining differences

- **Chained callbacks.** A self-rescheduling `call_soon` chain costs one turn
  per step now, as on asyncio and uvloop
  ([section 1](#1-results-against-v0165-and-uvloop)).
- **Spin under saturation.** On v0.1.64 the 50 µs spin let rsloop beat
  uvloop by 4–7% on the session read 2, count read and list read c64 rows (a saturated container, frequent
  wakes) while it cost 22–47% at c16. `RSLOOP_WAKE_SPIN_US=50` restores it
  for workloads whose wakes come from GIL-free threads.
- **Single serial TCP connection.** On a strict one-connection ping-pong the
  branch costs about 0.7–1.4 µs more CPU per round trip than uvloop on
  v0.1.64's base; with 8 connections it is level. The difference sits in the
  loop-thread reader task and turn bookkeeping.
- **Latency under load.** With the loop busy, an I/O event reaches the task
  it wakes one turn later than on uvloop, as on asyncio (a loaded asyncpg
  connect took 126 ms vs uvloop's 86 ms on v0.1.64's base). Running the
  reactor's events before taking each generation brought it to 94 ms but
  changed nothing in the full-service runs and changes asyncio's callback order, so
  it was not adopted.
- **Health probe at c64** sat 0.5–1.4% below uvloop on v0.1.64's base,
  at asyncio's level, after name resolution brought its CPU per request to
  uvloop's. Each request holds one pooled connection while it waits out a
  0.25 s timeout for a second one, so the rate depends on which waiter wins
  released connections; timer punctuality, timer order and I/O turn depth
  were each measured and ruled out. It is not among the rows in section 1.

## Reproducing

The tools in `perf/bench/` are the ones these measurements used. They
expect a benchmark workspace outside this repository, `$RSPERF_ROOT` (see
the `perf/bench/harness.py` docstring for its variables): the application's
images and compose file, a compose override that renames every container,
network and volume to the `rsperf` project and removes host ports, `sites/`
with unpacked wheels (one directory per build) and the outputs under
`run/bench/`.

| tool | what |
|---|---|
| `harness.py` | the service harness: `harness.py api <api> [rounds]`. Arms `uvloop`, `asyncio`, `rsloop` (the image's pin), `fork:<site>` (a build under `sites/`), `<arm>@<variant>` (extra environment, e.g. `@nospin`). The workload (service, endpoints, test-account login) comes from `RSPERF_<API>_*`; quiet-gated (`QUIET_*`); `BENCH_ENDPOINTS`, `BENCH_CONCURRENCY`, `BENCH_ROUND_BASE` select rows and rotation |
| `quiet.py` | the quiet-machine probe: CPU used outside the benchmark's containers over a window |
| `compare.py` | the tables: `compare.py <suffix> <api>... --arm <label>=<arm> ...` |
| `micro.py` | microbenchmarks: `python micro.py <asyncio\|uvloop\|rsloop> <bench>` (cross-thread ping-pong, Granian-shaped requests, `call_soon` chains, timers, TCP ping-pong, name resolution, `run_in_executor`) |
| `pool_churn.py` | the SQLAlchemy pool repro: `python pool_churn.py <loop> <dsn> [threads] [seconds] [queries]` |
| `sitecustomize.py` | opt-in (`RSPERF_TRACE_POOL=1`) SQLAlchemy pool trace inside the service workers |

For the microbenchmarks, the pool repro and upstream's `benches/`
comparison, each arm is a release wheel unpacked into its own directory and
put first on `PYTHONPATH` (`pip install --no-deps --target <dir> <wheel>`),
so every arm runs on the same interpreter.
