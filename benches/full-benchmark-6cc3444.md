# Full benchmark — commit 6cc3444

Measured September 7, 2026 on the release build of rsloop 0.1.49 at
`6cc3444e97aad1294813ea9b6de03bc190f95abb`. Linux 7.0.0-31-generic x86-64,
Intel Core i9-9900K (8 cores/16 threads), GIL-enabled CPython 3.14.7.
uvloop 0.22.1; zuvloop 0.0.14; websockets 17.0.1; aiohttp 3.14.3;
Starlette 1.6.0; uvicorn 0.52.3. Rsloop reports its io_uring reactor, rustls TLS
backend, and profiler disabled. No PGO. CPU affinity 0–15, frequency not fixed,
other host services running; no concurrent builds, tests, or profiling during
timed measurements.

## Microbenchmarks

Median milliseconds, lower is better. Seven measured fresh-process runs after
two warmups. TCP payload: 1,024 bytes; rsloop uses native fast streams and the
other loops use stdlib streams. Loop order: asyncio, uvloop, zuvloop, rsloop.

| Workload | asyncio | uvloop | zuvloop | rsloop |
|---|---:|---:|---:|---:|
| 200,000 callbacks | 112.49 | 51.57 | **36.23** | 48.31 |
| 50,000 tasks | 149.49 | 93.55 | **81.47** | 89.54 |
| 5,000 TCP roundtrips | 150.87 | 126.20 | 109.64 | **84.60** |

## Complete network matrix

Traffic-only operations/second, higher is better; bulk transfer uses MiB/s.
P95 latency is the median of run-level p95s, in milliseconds. Seven measured
runs after two warmups share one subprocess per loop/scenario; 16 connections,
500 operations per connection for request/response workloads. Bulk transfer
sends 2 MiB per connection in 64 KiB chunks. Loops run sequentially: rsloop,
uvloop, zuvloop. Setup and teardown are excluded from throughput.
TLS implementations differ:
rsloop uses rustls, while the other loops use Python's OpenSSL-backed SSL layer.

| Scenario | rsloop | uvloop | zuvloop | rsloop p95 | uvloop p95 | zuvloop p95 |
| --- | ---: | ---: | ---: | ---: | ---: | ---: |
| HTTP keep-alive | 54,103 | 51,649 | 57,764 | 0.336 ms | 0.346 ms | 0.292 ms |
| TLS HTTP | 72,496 | 26,226 | 23,408 | 0.241 ms | 0.646 ms | 0.717 ms |
| Raw WebSocket | 4,870 | 4,890 | 4,898 | 4.454 ms | 3.874 ms | 3.357 ms |
| Raw WebSocket over TLS | 4,933 | 4,455 | 4,397 | 4.355 ms | 4.194 ms | 3.800 ms |
| `websockets` | 24,667 | 25,800 | 27,409 | 0.736 ms | 0.631 ms | 0.594 ms |
| `websockets` over TLS | 26,781 | 14,872 | 14,555 | 0.644 ms | 1.135 ms | 1.167 ms |
| aiohttp WebSocket | 31,636 | 32,765 | 35,425 | 0.605 ms | 0.535 ms | 0.484 ms |
| aiohttp WebSocket over TLS | 34,165 | 19,110 | 18,400 | 0.504 ms | 0.882 ms | 0.909 ms |
| Starlette WebSocket | 18,785 | 20,407 | 21,743 | 0.981 ms | 0.829 ms | 0.787 ms |
| Starlette WebSocket over TLS | 18,509 | 13,104 | 13,038 | 0.948 ms | 1.278 ms | 1.291 ms |
| Mixed streams | 43,795 | 34,196 | 37,116 | 0.463 ms | 0.523 ms | 0.473 ms |
| Bulk transfer (MiB/s) | 2,009.9 | 1,264.9 | 1,313.3 | 15.058 ms | 25.219 ms | 24.317 ms |

## Idle activation latency

Milliseconds, lower is better. 200 established connections; 100 measured cycles
plus five warmup cycles per process; 0.2 seconds idle per cycle; nine paired
fresh-process blocks with rotating loop order. Each loop runs first three times.
Medians across runs of run-level median cycle milestones, not pooled connections.
Intervals use 10,000 paired process-block bootstrap resamples, seed 0, with a
5% practical threshold. Each loop contributes 900 measured cycles across nine
independent processes.

| Loop | First reply | 50% replied | 95% replied | All replied |
| --- | ---: | ---: | ---: | ---: |
| rsloop | 17.221 | 17.579 | 17.857 | 17.991 |
| uvloop | 18.193 | 18.722 | 19.174 | 19.221 |
| zuvloop | 16.441 | 16.869 | 17.248 | 17.288 |

Comparisons against uvloop use geometric mean paired process-run ratios,
not ratios of the table medians:

- rsloop: -23.5% p95 latency, approximate 95% interval [-48.6%, +5.9%]; **inconclusive** at the 5% threshold.
- zuvloop: -15.2% p95 latency, approximate 95% interval [-31.0%, +4.5%]; **inconclusive** at the 5% threshold.

Individual cycle-p95 latencies span 3.374–28.184 ms for rsloop,
3.736–24.228 ms for uvloop, and 3.454–26.163 ms for zuvloop.
Median ordering alone does not establish a latency win.

## Interpretation

Rsloop had the highest median throughput in seven of the 12 network scenarios:
all five TLS workloads, mixed streams, and bulk transfer. Zuvloop led the five
plaintext HTTP/WebSocket workloads.
Zuvloop led callback and task microbenchmarks; rsloop led TCP-stream round trips.
Throughput and tail latency can disagree, so both are reported. Idle results
require the paired uncertainty estimates above; median order alone is not a
latency-win claim. This compares current implementations on one host, and does
not isolate a before/after effect from the latest optimization. The
[reactor investigation](reactor-performance.md) contains that separate A/B evidence.

## Recorded results and commands

- [Microbenchmark JSON](results/full-6cc3444-micro.json)
- [Compact matrix JSON](results/full-6cc3444-matrix.json)
- [Compact idle activation JSON](results/full-6cc3444-idle.json)

The tracked JSON records the measured source and binary hashes, versions,
settings, per-run times, latency quantiles, RSS, and idle milestone medians.
It retains the statistics needed to reproduce these tables and idle intervals.
Individual request/cycle samples are omitted; full raw output remains locally
in `target/benchmark-6cc3444/`. Compact bundles are not direct input to
`check_regression.py`. The historical [9011c9f report](full-benchmark-9011c9f.md)
remains available; these new measurements replace the current Linux README tables.

```bash
.venv/bin/maturin develop --release --locked
.venv/bin/python -u benches/compare_event_loops.py \
  --loops asyncio,uvloop,zuvloop,rsloop --repeat 7 --warmups 2 \
  --json-output target/benchmark-6cc3444/micro.json
.venv/bin/python -u benches/workload_matrix.py \
  --loops rsloop,uvloop,zuvloop --sustained \
  --scenarios http_keepalive,tls_http,websocket_messages,websocket_tls,websockets_messages,websockets_tls,aiohttp_websocket_messages,aiohttp_websocket_tls,starlette_websocket_messages,starlette_websocket_tls,mixed_streams,bulk_transfer \
  --json-output target/benchmark-6cc3444/matrix.json
.venv/bin/python -u benches/workload_matrix.py \
  --loops rsloop,uvloop,zuvloop --scenarios idle_connections --repeat 9 \
  --idle-cycles 100 --idle-warmup-cycles 5 --idle-seconds 0.2 \
  --json-output target/benchmark-6cc3444/idle.json
```
