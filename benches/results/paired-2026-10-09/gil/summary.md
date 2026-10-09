# Paired event-loop comparison (gil)

Fresh processes, one CPU; GC disabled. RTT is measured separately.

| Workload | Loop/revision | Median ms | Ops/s | CV % | Peak RSS MiB |
|---|---|---:|---:|---:|---:|
| callbacks | current | 173.096 | 5777148 | 1.02 | 108.11 |
| callbacks | historical | 173.491 | 5763972 | 0.97 | 115.91 |
| callbacks | pre | 170.183 | 5876021 | 1.19 | 108.25 |
| callbacks | uvloop | 252.096 | 3966744 | 1.30 | 237.84 |
| callbacks | zuvloop | 180.285 | 5546770 | 1.09 | 129.38 |
| tasks | current | 313.364 | 638235 | 2.95 | 38.54 |
| tasks | historical | 311.443 | 642173 | 2.30 | 38.58 |
| tasks | pre | 312.877 | 639229 | 1.10 | 38.64 |
| tasks | uvloop | 349.264 | 572633 | 3.50 | 38.00 |
| tasks | zuvloop | 295.338 | 677189 | 2.10 | 36.69 |
| tcp_streams | current | 323.488 | 61826 | 1.44 | 32.64 |
| tcp_streams | historical | 337.028 | 59342 | 1.71 | 32.66 |
| tcp_streams | pre | 330.145 | 60579 | 2.40 | 32.66 |
| tcp_streams | uvloop | 503.366 | 39733 | 1.38 | 30.92 |
| tcp_streams | zuvloop | 406.265 | 49229 | 0.86 | 30.29 |

| Revision | RTT median µs | RTT p95 µs | RTT p99 µs |
|---|---:|---:|---:|
| current | 16.68 | 18.84 | 28.73 |
| historical | 17.02 | 19.15 | 26.21 |
| pre | 16.90 | 18.72 | 24.63 |
| uvloop | 25.16 | 28.04 | 38.87 |
| zuvloop | 20.27 | 22.49 | 33.31 |

Current versus pre-migration (positive time/RSS means worse):

- callbacks peak_rss_bytes: -0.13%
- callbacks seconds: +1.71%
- tasks peak_rss_bytes: -0.27%
- tasks seconds: +0.16%
- tcp_streams peak_rss_bytes: -0.06%
- tcp_streams seconds: -2.02%
- tcp_latency peak_rss_bytes: -0.42%
- Paired callbacks seconds: median +1.01%, range +0.23…+6.00%
- Paired callbacks ops_per_sec: median -1.00%, range -5.66…-0.23%
- Paired callbacks peak_rss_bytes: median -0.12%, range -0.23…-0.02%
- Paired tasks seconds: median +0.54%, range -2.34…+9.33%
- Paired tasks ops_per_sec: median -0.53%, range -8.54…+2.40%
- Paired tasks peak_rss_bytes: median -0.31%, range -0.44…+0.08%
- Paired tcp_streams seconds: median -2.33%, range -7.67…+1.52%
- Paired tcp_streams ops_per_sec: median +2.38%, range -1.50…+8.31%
- Paired tcp_streams peak_rss_bytes: median -0.06%, range -0.38…+0.26%
- Paired tcp_latency peak_rss_bytes: median -0.45%, range -0.58…-0.21%
- Paired tcp_latency rtt_median: median -0.87%, range -3.94…+3.48%
- Paired tcp_latency rtt_p95: median +0.74%, range -2.15…+3.15%
- Paired tcp_latency rtt_p99: median +3.90%, range -22.76…+35.52%

Hosted runners are noisy: inspect min/max/CV and raw paired rounds;
repeat complete runs before attributing a small difference to smol.
