# Full benchmark — commit 9011c9f

Measured September 7, 2026. Commit pushed to origin/master. Release build of rsloop 0.1.49 on Linux x86-64, Intel Core i9-9900K (8 cores/16 threads), CPython 3.14.7. uvloop 0.22.1; zuvloop 0.0.14; websockets 17.0.1; aiohttp 3.14.3; Starlette 1.6.0; uvicorn 0.52.3. Unrestricted CPU affinity; other host services running, but no concurrent builds or tests during measurement.

## Microbenchmarks

Median milliseconds, lower is better. Five measured fresh-process runs after one warmup. TCP payload: 1,024 bytes; rsloop uses native fast streams.

| Workload | asyncio | uvloop | zuvloop | rsloop |
|---|---:|---:|---:|---:|
| 200,000 callbacks | 111.78 | 51.24 | 36.59 | 48.05 |
| 50,000 tasks | 147.60 | 95.29 | 78.09 | 85.43 |
| 5,000 TCP round trips | 149.11 | 124.42 | 105.60 | 91.51 |

## Complete network matrix

Traffic-only operations/second, higher is better. Bulk transfer uses MiB/s instead. All p95 values are milliseconds, lower is better. Medians of seven runs after two warmups in one subprocess per loop/scenario; 16 connections and 500 requests per connection. Loops run sequentially: rsloop, uvloop, zuvloop. Setup and teardown are excluded from throughput.

| Workload | rsloop | uvloop | zuvloop | rsloop p95 | uvloop p95 | zuvloop p95 |
|---|---:|---:|---:|---:|---:|---:|
| HTTP keep-alive | 53,191 | 49,994 | 57,592 | 0.333 | 0.356 | 0.318 |
| TLS HTTP | 71,150 | 24,790 | 23,419 | 0.246 | 0.687 | 0.728 |
| Raw WebSocket | 4,791 | 4,825 | 4,907 | 5.661 | 3.723 | 3.383 |
| Raw WebSocket + TLS | 4,915 | 4,445 | 4,413 | 4.291 | 3.979 | 3.787 |
| websockets | 23,929 | 25,664 | 27,539 | 0.787 | 0.661 | 0.624 |
| websockets + TLS | 26,960 | 15,006 | 14,163 | 0.647 | 1.111 | 1.205 |
| aiohttp WebSocket | 29,771 | 33,197 | 35,135 | 0.655 | 0.521 | 0.498 |
| aiohttp WebSocket + TLS | 34,482 | 19,320 | 17,256 | 0.500 | 0.871 | 0.974 |
| Starlette WebSocket | 19,138 | 20,119 | 20,950 | 0.968 | 0.841 | 0.868 |
| Starlette WebSocket + TLS | 18,538 | 13,624 | 13,187 | 0.949 | 1.243 | 1.266 |
| Mixed streams | 41,900 | 36,567 | 35,284 | 0.495 | 0.498 | 0.499 |
| Bulk transfer (MiB/s) | 2,015.9 | 1,236.8 | 1,065.2 | 14.767 | 25.801 | 29.989 |

## Idle activation latency

Milliseconds, lower is better. 200 established connections; 100 cycles plus five warmups per process; 0.2 seconds idle per cycle; nine paired fresh-process blocks with rotating loop order. Medians across runs of run-level median cycle milestones, not pooled connections.

| Loop | First reply | 50% replied | 95% replied | All replied |
|---|---:|---:|---:|---:|
| rsloop | 17.779 | 18.130 | 18.391 | 18.517 |
| uvloop | 18.056 | 18.568 | 18.996 | 19.042 |
| zuvloop | 16.601 | 17.016 | 17.382 | 17.422 |

Comparisons against uvloop use geometric-mean paired run ratios, not ratios of table medians:

- rsloop: +13.3% p95 latency, approximate 95% CI [-10.2%, +42.4%]; inconclusive at the 5% threshold.
- zuvloop: +2.3% p95 latency, approximate 95% CI [-15.5%, +27.0%]; inconclusive at the 5% threshold.

## Interpretation

Rsloop led throughput in seven of 12 network scenarios (all five TLS workloads, mixed streams, and bulk transfer); zuvloop led the five plaintext HTTP/WebSocket workloads. Zuvloop led callbacks/tasks, while rsloop led the TCP-stream microbenchmark. Raw WebSocket p95 remained worse for rsloop, including over TLS. Both idle comparisons are inconclusive: median ordering alone is not a reliable latency-win claim. This is a comparison of the current versions, not a new before/after regression measurement.

## Recorded results and commands

- [Microbenchmark JSON](results/full-9011c9f-micro.json)
- [Compact matrix JSON](results/full-9011c9f-matrix.json)
- [Compact idle activation JSON](results/full-9011c9f-idle.json)

The tracked JSON retains per-run measurements and the statistics needed to reproduce
these tables and idle confidence intervals. Individual request/cycle samples are
omitted; full raw output remains locally in `target/benchmark-9011c9f/`.
The compact format is not direct input to `check_regression.py`.

```bash
.venv/bin/maturin develop --release --locked
.venv/bin/python benches/compare_event_loops.py --loops asyncio,uvloop,zuvloop,rsloop --repeat 5 --warmups 1 --json-output target/benchmark-9011c9f/micro.json
.venv/bin/python benches/workload_matrix.py --loops rsloop,uvloop,zuvloop --sustained --scenarios http_keepalive,tls_http,websocket_messages,websocket_tls,websockets_messages,websockets_tls,aiohttp_websocket_messages,aiohttp_websocket_tls,starlette_websocket_messages,starlette_websocket_tls,mixed_streams,bulk_transfer --json-output target/benchmark-9011c9f/matrix.json
.venv/bin/python benches/workload_matrix.py --loops rsloop,uvloop,zuvloop --scenarios idle_connections --repeat 9 --idle-cycles 100 --idle-warmup-cycles 5 --idle-seconds 0.2 --json-output target/benchmark-9011c9f/idle.json
```
