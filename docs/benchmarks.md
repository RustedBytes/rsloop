# Benchmarks

```bash
uv run --with maturin maturin develop --release
uv run --with uvloop --with 'zuvloop; python_version >= "3.14"' python benches/compare_event_loops.py \
  --loops asyncio,uvloop,zuvloop,rsloop --repeat 7 --warmups 2
```

## Four-loop comparison on Linux

Measured on September 14, 2026 at commit `ccbebb6` on an Intel Core i9-9900K,
Linux 7.0.0-31-generic (x86_64), and CPython 3.14.7, with rsloop 0.1.51 built
in release mode, uvloop 0.22.1, and zuvloop 0.0.16. Each entry is the median
of seven measured runs after two warmups, with each run in a fresh subprocess.
Times are milliseconds; lower is better.

| Workload | asyncio | uvloop | zuvloop | rsloop |
|---|---:|---:|---:|---:|
| 200,000 callbacks | 109.97 | 51.71 | **36.10** | 47.87 |
| 50,000 tasks | 146.98 | 89.96 | **78.60** | 87.18 |
| 5,000 TCP roundtrips | 150.57 | 125.78 | 104.51 | **82.01** |

The TCP workload uses 1,024-byte payloads and rsloop's native fast streams;
the other loops use stdlib asyncio streams. Use `--no-rsloop-fast-streams`
to compare all loops through the stdlib streams layer. Zuvloop led callbacks
and tasks in this run, while rsloop led TCP roundtrips. These are local
microbenchmarks, not isolated-lab measurements or general application performance
claims; do not compare them directly with the historical macOS results below.
The command above records the exact settings used for this updated microbenchmark.

## Historical macOS comparison

An earlier example output from the script on macOS (arm64) with CPython 3.14:

```
callbacks (200,000 ops)
loop           median_s       best_s      ops_per_s     peak_rss   vs_fastest    slower_by
rsloop         0.033083     0.032710      6,045,401     67.5 MiB        1.00x         0.0%
uvloop         0.040958     0.040721      4,883,026     72.8 MiB        1.24x        23.8%
asyncio        0.082233     0.082093      2,432,114     65.3 MiB        2.49x       148.6%

tasks (50,000 ops)
loop           median_s       best_s      ops_per_s     peak_rss   vs_fastest    slower_by
rsloop         0.063593     0.063286        786,247     37.6 MiB        1.00x         0.0%
uvloop         0.069614     0.069420        718,251     38.4 MiB        1.09x         9.5%
asyncio        0.108114     0.107502        462,473     36.1 MiB        1.70x        70.0%

tcp_streams (5,000 ops)
loop           median_s       best_s      ops_per_s     peak_rss   vs_fastest    slower_by
rsloop         0.090940     0.083355         54,981     32.2 MiB        1.00x         0.0%
uvloop         0.133182     0.127404         37,543     31.5 MiB        1.46x        46.5%
asyncio        0.302337     0.299813         16,538     29.6 MiB        3.32x       232.5%
```

## Sustained network workloads

The production-shaped workload matrix exercises HTTP, WebSocket libraries,
TLS, mixed message sizes, backpressure, and connection lifecycle behavior:

```bash
uv run --with uvloop --with zuvloop python benches/workload_matrix.py \
  --loops rsloop,uvloop,zuvloop \
  --sustained \
  --scenarios http_keepalive,tls_http,websocket_messages,websocket_tls,websockets_messages,websockets_tls,aiohttp_websocket_messages,aiohttp_websocket_tls,starlette_websocket_messages,starlette_websocket_tls,mixed_streams,bulk_transfer \
  --json-output target/matrix-zuvloop.json
```

Measured on September 7, 2026 with an Intel Core i9-9900K, Linux
7.0.0-31-generic (x86_64), CPython 3.14.7, rsloop 0.1.49 (release build,
commit `6cc3444`), uvloop 0.22.1, and zuvloop 0.0.14. Each row reports the
median of seven measured runs after two warmups, using 16 concurrent connections.
Request/response workloads send 500 requests per connection; bulk transfer
sends 2 MiB per connection in 64 KiB chunks. Throughput is traffic-only
operations per second, except for
`bulk_transfer`, which reports traffic MiB/s. The p95 columns are the medians
of each run's p95 latency. Higher throughput and lower latency are better.
Each loop/scenario pair runs in its own subprocess, with warmups and measured
runs sharing that process. Loops run sequentially in the order shown.

WebSocket library versions were websockets 17.0.1, aiohttp 3.14.3, Starlette
1.6.0, and uvicorn 0.52.3. The run used unrestricted CPU affinity, with other
host services running but no concurrent builds or tests. These measurements
are from a different host than the macOS microbenchmark example above.

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

The former single-burst idle-activation row has been retired: its traffic
phase lasted only a few milliseconds and produced unstable throughput rankings.
Idle activation now has a separate, versioned latency benchmark described below.
In this run, zuvloop had the highest plaintext HTTP and WebSocket throughput,
while rsloop led TLS throughput, mixed streams, and bulk transfer. Throughput
and tail latency do not always agree: rsloop's raw WebSocket p95 was higher
than both alternatives, including over TLS. These results are not an
across-the-board performance win or a before/after regression measurement. See
the [historical benchmark documentation](https://github.com/RustedBytes/rsloop/blob/b7dac64/benches/README.md)
for workload definitions and reproduction commands.

The ordinary matrix defaults are intentionally short enough for local smoke
and CI runs. Even with `--sustained`, compare repeated runs before drawing
performance conclusions for a deployment — competing desktop load matters more
than it looks, because rsloop trades helper-thread CPU for loop-thread work and
so has more to lose when cores are contended.

## Idle activation latency

```bash
uv run --with uvloop --with zuvloop python benches/workload_matrix.py \
  --loops rsloop,uvloop,zuvloop --scenarios idle_connections --repeat 9 \
  --idle-cycles 100 --idle-warmup-cycles 5 --idle-seconds 0.2 \
  --json-output target/idle-v2-paired.json
```

Idle v2 reuses 200 established connections across repeated idle/wakeup cycles.
It measures all replies from one shared activation timestamp, including task
scheduling delay, and reports first/50%/95%/all-reply latency. Nine fresh-process
blocks rotate loop order so each loop runs first three times; confidence
intervals resample whole paired runs, not individual connections. Results are classified as improved, regressed, or
inconclusive using a 5% practical threshold and an approximate 95% confidence
interval. The command takes about ten minutes; use `--idle-cycles 3
--idle-warmup-cycles 1 --idle-seconds 0.01 --repeat 1` for a smoke test only.

The new measurements cannot be compared with the retired ops/s row. See
[historical benchmark methodology and regression handling](https://github.com/RustedBytes/rsloop/blob/b7dac64/benches/README.md#idle-activation-v2)
for timing definitions, host controls, raw distributions, and sample requirements.

Measured on September 7, 2026 at commit `6cc3444` on the Linux/i9-9900K
host above with CPython 3.14.7, rsloop 0.1.49 (release), uvloop 0.22.1,
and zuvloop 0.0.14. The run collected 900 measured cycles per loop in 27
distinct processes, with unrestricted affinity and no concurrent builds or
test runs. These are medians across runs of each run's median cycle milestone,
in milliseconds (lower is better):

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

The [historical full report](https://github.com/RustedBytes/rsloop/blob/b7dac64/benches/full-benchmark-6cc3444.md)
links the recorded measurements and documents the settings used for all three
benchmark suites.

See the [benchmark scripts](https://github.com/RustedBytes/rsloop/tree/main/benches)
for current workload implementations and extra flags, and the
[examples](https://github.com/RustedBytes/rsloop/tree/main/examples) for the
FastAPI loop comparison example.
