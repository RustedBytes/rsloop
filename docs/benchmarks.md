# Benchmarks

Measured November 23, 2026 at commit `70419b9` on an AMD Ryzen AI 7 350
running Linux 6.6.87.2 under WSL2 (x86_64). The comparison uses rsloop 0.1.69
(release build with io_uring), uvloop 0.23.0 and zuvloop 0.0.21. GIL-enabled
runs use CPython 3.14.7. Free-threaded runs use CPython 3.14.3t with
`PYTHON_GIL=0` for every loop.

Results are medians of seven runs after two warmups unless stated otherwise.
Lower times are better. Benchmarks ran sequentially after both release builds
finished. Small differences may reflect host noise.
The different Python patch versions prevent isolating the GIL's effect.

## Callbacks, tasks and TCP

Each sample uses a fresh process with cyclic GC disabled:

- Callbacks: schedule 1,000,000 callbacks in one batch.
- Tasks: run 200,000 tasks that await `asyncio.sleep(0)`, in batches of 5,000.
- TCP: exchange 20,000 1,024-byte messages over one connection. rsloop uses
  native fast streams; the other loops use asyncio streams.

Peak RSS includes the interpreter and libraries, measured with Linux `VmHWM`.
CPU affinity includes worker threads. CPUs 2 and 4 are separate physical
cores. Each process runs one event loop in both affinity configurations.

### One core, GIL enabled (CPU 2)

Time (ms):

| Workload | asyncio | uvloop | zuvloop | rsloop |
|---|---:|---:|---:|---:|
| 1,000,000 callbacks | 329.50 | 154.31 | 105.77 | 110.01 |
| 200,000 tasks | 411.15 | 228.04 | 215.31 | 210.26 |
| 20,000 TCP round trips | 224.60 | 168.09 | 153.95 | 169.34 |

Peak RSS (MiB):

| Workload | asyncio | uvloop | zuvloop | rsloop |
|---|---:|---:|---:|---:|
| 1,000,000 callbacks | 201.1 | 234.4 | 126.3 | 118.9 |
| 200,000 tasks | 31.4 | 34.4 | 33.6 | 34.2 |
| 20,000 TCP round trips | 24.2 | 27.6 | 26.9 | 28.2 |

### Two cores, GIL enabled (CPUs 2 and 4)

Time (ms):

| Workload | asyncio | uvloop | zuvloop | rsloop |
|---|---:|---:|---:|---:|
| 1,000,000 callbacks | 331.84 | 155.22 | 105.83 | 111.19 |
| 200,000 tasks | 406.50 | 224.10 | 192.67 | 227.60 |
| 20,000 TCP round trips | 226.63 | 169.32 | 155.41 | 170.50 |

Peak RSS (MiB):

| Workload | asyncio | uvloop | zuvloop | rsloop |
|---|---:|---:|---:|---:|
| 1,000,000 callbacks | 201.1 | 234.4 | 126.3 | 118.9 |
| 200,000 tasks | 31.4 | 34.3 | 33.7 | 34.2 |
| 20,000 TCP round trips | 24.2 | 27.6 | 27.0 | 28.1 |

### One core, free-threaded (CPU 2)

Time (ms):

| Workload | asyncio | uvloop | zuvloop | rsloop |
|---|---:|---:|---:|---:|
| 1,000,000 callbacks | 389.13 | 168.73 | 122.94 | 125.81 |
| 200,000 tasks | 543.15 | 321.42 | 246.93 | 259.87 |
| 20,000 TCP round trips | 241.05 | 171.14 | 154.84 | 179.15 |

Peak RSS (MiB):

| Workload | asyncio | uvloop | zuvloop | rsloop |
|---|---:|---:|---:|---:|
| 1,000,000 callbacks | 207.8 | 257.4 | 133.0 | 186.1 |
| 200,000 tasks | 38.2 | 42.0 | 42.5 | 41.9 |
| 20,000 TCP round trips | 30.2 | 34.0 | 33.6 | 36.1 |

## Network workloads

CPython 3.14.7, GIL enabled, CPU 2. HTTP and WebSocket workloads use
16 connections with 500 operations each (8,000 total). HTTP uses 4,096-byte
bodies and eight SHA-256 iterations per request; raw WebSocket frames cycle
through 32, 256 and 4,096 bytes. GC is enabled. Each loop/scenario uses one
process for all samples, creating a fresh loop and server per sample.

Short TCP connections use 8,000 fresh connections at concurrency 16, checking
a one-byte echo before closing. Each loop uses its own process, with a fresh
loop and server per sample and GC disabled.

Times include connection setup and teardown. Short TCP timing also includes
server shutdown and excludes listener creation.

| Workload | asyncio | uvloop | zuvloop | rsloop |
|---|---:|---:|---:|---:|
| HTTP keep-alive | 120.49 | 108.42 | 76.52 | 79.77 |
| TLS HTTP | 222.44 | 169.87 | 191.16 | 207.09 |
| Raw WebSocket | 1018.22 | 977.03 | 969.91 | 991.83 |
| Raw TLS WebSocket | 1125.71 | 1070.71 | 1113.07 | 1180.05 |
| 8,000 short TCP connections | 594.54 | 352.42 | 430.53 | 407.07 |

## Native reader lifecycle

rsloop only, CPython 3.14.7, GIL enabled, CPU 2, GC enabled. Each method has a
1,000-round warmup and seven samples of 100,000 rounds. Waiting reads include
Future completion callbacks. Values are median nanoseconds per round.

| Operation | Buffered (ns) | Waiting (ns) |
|---|---:|---:|
| `read` | 132.8 | 2069.8 |
| `readexactly` | 131.7 | 1340.3 |
| `readuntil` | 172.8 | 1346.9 |

## Reproduce

Run from the repository root. Build both environments before measuring:

```bash
uv venv --python 3.14.7 target/bench-gil
uv venv --python 3.14.3t target/bench-free-threaded
uv pip install --python target/bench-gil/bin/python maturin==1.16.0
for env in bench-gil bench-free-threaded; do
  uv pip install --python "target/$env/bin/python" uvloop==0.23.0 zuvloop==0.0.21
  target/bench-gil/bin/maturin build --release --locked --compatibility linux \
    --interpreter "target/$env/bin/python" --out "target/benchmarks/wheels/$env"
  uv pip install --python "target/$env/bin/python" "target/benchmarks/wheels/$env/"*.whl
done
uv run --no-project python scripts/generate_test_tls_certs.py tests/fixtures/tls
mkdir -p target/benchmarks
```

Run the general workloads with each interpreter and affinity:

```bash
compare() {
  taskset -c "$1" "$2" benches/compare_event_loops.py \
    --loops asyncio,uvloop,zuvloop,rsloop --repeat 7 --warmups 2 \
    --callbacks 1000000 --tasks 200000 --task-batch-size 5000 \
    --tcp-roundtrips 20000 --payload-size 1024 \
    --json-output "target/benchmarks/$3.json"
}
PYTHON_GIL=1 compare 2 target/bench-gil/bin/python gil-one-core
PYTHON_GIL=1 compare 2,4 target/bench-gil/bin/python gil-two-cores
PYTHON_GIL=0 compare 2 target/bench-free-threaded/bin/python free-threaded-one-core
```

Check CPU topology before choosing affinity on another host. Run the network
matrix and reader diagnostic with the GIL-enabled interpreter:

```bash
PYTHON_GIL=1 taskset -c 2 target/bench-gil/bin/python benches/workload_matrix.py \
  --loops asyncio,uvloop,zuvloop,rsloop \
  --scenarios http_keepalive,tls_http,websocket_messages,websocket_tls \
  --concurrency 16 --requests-per-connection 500 --warmups 2 --repeat 7 \
  --measurement-mode warm --json-output target/benchmarks/network.json
PYTHON_GIL=1 taskset -c 2 target/bench-gil/bin/python benches/stream_read_lifecycle.py \
  --rounds 100000 --repeat 7 --output target/benchmarks/reads.json
```

For short connections, run each loop in a separate process:

```bash
for loop in asyncio uvloop zuvloop rsloop; do
  PYTHON_GIL=1 taskset -c 2 target/bench-gil/bin/python - "$loop" <<'PYTHON'
import asyncio
import gc
import json
import statistics
import sys
from pathlib import Path

sys.path.insert(0, "benches")
from compare_event_loops import loop_factory_for
from connection_churn import measure

gc.collect()
gc.disable()
samples = [
    asyncio.run(asyncio.wait_for(measure(8000, 16), 120),
                loop_factory=loop_factory_for(sys.argv[1]))
    for _ in range(9)
][2:]
Path(f"target/benchmarks/churn-{sys.argv[1]}.json").write_text(json.dumps(samples))
print(sys.argv[1], statistics.median(samples) * 1000, "ms")
PYTHON
done
```

Raw samples and logs for this refresh are local build artifacts in
`target/benchmarks-2026-11-23/`. Sample files include `gil-one-core.json`,
`gil-two-cores.json`, `free-threaded-one-core.json`, `network.json`,
`churn-<loop>.json` and `reads.json`. The `environment-*.json` files record
Python and package versions, GIL state, build profile, reactor and native
binary hashes.
