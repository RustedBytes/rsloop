"""Profile one existing rsloop benchmark in an instrumented extension.

Build first with ``maturin develop --release --features hotpath-profile``.
Run each workload in its own process so each report has a clear scope.
"""

from __future__ import annotations

import argparse
import gc
import importlib.util
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
BENCHMARK = ROOT / "benches" / "compare_event_loops.py"


def load_benchmark():
    spec = importlib.util.spec_from_file_location("compare_event_loops", BENCHMARK)
    assert spec is not None and spec.loader is not None
    module = importlib.util.module_from_spec(spec)
    sys.modules[spec.name] = module
    spec.loader.exec_module(module)
    return module


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("workload", choices=("callbacks", "tasks", "tcp_streams"))
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--callbacks", type=int, default=200_000)
    parser.add_argument("--tasks", type=int, default=50_000)
    parser.add_argument("--task-batch-size", type=int, default=5_000)
    parser.add_argument("--tcp-roundtrips", type=int, default=5_000)
    parser.add_argument("--payload-size", type=int, default=1024)
    args = parser.parse_args()

    from rsloop import _loop

    if not hasattr(_loop, "hotpath_start"):
        raise SystemExit("build rsloop with --features hotpath-profile first")

    bench = load_benchmark()

    def run(count: int):
        if args.workload == "callbacks":
            coro = bench.bench_callbacks("rsloop", count)
        elif args.workload == "tasks":
            coro = bench.bench_tasks("rsloop", count, args.task_batch_size)
        else:
            coro = bench.bench_tcp_streams("rsloop", count, args.payload_size)
        return bench.run_with_loop("rsloop", coro)

    warmup = {"callbacks": 1_000, "tasks": 500, "tcp_streams": 50}
    count = {
        "callbacks": args.callbacks,
        "tasks": args.tasks,
        "tcp_streams": args.tcp_roundtrips,
    }[args.workload]
    run(warmup[args.workload])

    args.output.parent.mkdir(parents=True, exist_ok=True)
    gc.collect()
    gc.disable()
    try:
        _loop.hotpath_start(str(args.output))
        try:
            result = run(count)
        finally:
            _loop.hotpath_stop()
    finally:
        gc.enable()

    print(
        f"{result.workload}: {result.operations:,} operations in {result.seconds:.4f}s"
    )
    print(f"hotpath report: {args.output}")


if __name__ == "__main__":
    main()
