# Paired event-loop comparison (free-threaded)

Fresh processes, one CPU; GC disabled. RTT is measured separately.

| Workload | Loop/revision | Median ms | Ops/s | CV % | Peak RSS MiB |
|---|---|---:|---:|---:|---:|
| callbacks | current | 204.004 | 4901855 | 1.13 | 129.30 |
| callbacks | historical | 209.524 | 4772732 | 1.09 | 136.96 |
| callbacks | pre | 201.239 | 4969215 | 1.67 | 129.40 |
| callbacks | uvloop | 296.073 | 3377544 | 1.57 | 260.68 |
| callbacks | zuvloop | 200.978 | 4975679 | 1.11 | 136.18 |
| tasks | current | 369.299 | 541566 | 1.72 | 46.67 |
| tasks | historical | 374.807 | 533608 | 0.97 | 47.07 |
| tasks | pre | 363.140 | 550751 | 0.83 | 46.96 |
| tasks | uvloop | 408.689 | 489369 | 1.57 | 45.22 |
| tasks | zuvloop | 353.022 | 566536 | 1.13 | 45.74 |
| tcp_streams | current | 340.772 | 58690 | 1.28 | 40.25 |
| tcp_streams | historical | 344.482 | 58058 | 1.16 | 40.55 |
| tcp_streams | pre | 337.521 | 59256 | 1.89 | 40.31 |
| tcp_streams | uvloop | 520.515 | 38424 | 1.60 | 37.66 |
| tcp_streams | zuvloop | 415.116 | 48179 | 0.96 | 37.25 |

| Revision | RTT median µs | RTT p95 µs | RTT p99 µs |
|---|---:|---:|---:|
| current | 17.26 | 19.10 | 21.71 |
| historical | 17.33 | 19.09 | 21.65 |
| pre | 16.93 | 18.88 | 21.28 |
| uvloop | 26.23 | 28.36 | 31.02 |
| zuvloop | 20.80 | 22.57 | 25.12 |

Current versus pre-migration (positive time/RSS means worse):

- callbacks peak_rss_bytes: -0.08%
- callbacks seconds: +1.37%
- tasks peak_rss_bytes: -0.61%
- tasks seconds: +1.70%
- tcp_streams peak_rss_bytes: -0.14%
- tcp_streams seconds: +0.96%
- tcp_latency peak_rss_bytes: -0.36%
- Paired callbacks seconds: median +0.94%, range -1.53…+5.02%
- Paired callbacks ops_per_sec: median -0.93%, range -4.78…+1.55%
- Paired callbacks peak_rss_bytes: median -0.03%, range -0.22…+0.14%
- Paired tasks seconds: median +2.39%, range -0.98…+4.46%
- Paired tasks ops_per_sec: median -2.34%, range -4.27…+0.99%
- Paired tasks peak_rss_bytes: median -0.58%, range -0.72…-0.17%
- Paired tcp_streams seconds: median +1.08%, range -2.80…+5.96%
- Paired tcp_streams ops_per_sec: median -1.06%, range -5.63…+2.88%
- Paired tcp_streams peak_rss_bytes: median -0.12%, range -0.93…+0.43%
- Paired tcp_latency peak_rss_bytes: median -0.31%, range -0.98…+1.06%
- Paired tcp_latency rtt_median: median +1.84%, range +0.38…+4.16%
- Paired tcp_latency rtt_p95: median +2.19%, range -0.23…+5.11%
- Paired tcp_latency rtt_p99: median +1.41%, range -10.77…+3.79%

Hosted runners are noisy: inspect min/max/CV and raw paired rounds;
repeat complete runs before attributing a small difference to smol.
