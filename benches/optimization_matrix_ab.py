#!/usr/bin/env python3
"""Interleave baseline/candidate wheels in fresh workload-matrix processes.

Each process performs two discarded warmups and one measured run. Every run
uses 16 connections and 500 requests per connection. This supplements the
standard sustained matrix with independent, alternating A/B process blocks.
"""

import argparse
import json
import os
import statistics
import subprocess
import sys
from pathlib import Path

from idle_statistics import latency_comparison


def percentile(values, fraction):
    ordered = sorted(values)
    return ordered[min(len(ordered) - 1, int(len(ordered) * fraction))]


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline-pythonpath", required=True, type=Path)
    parser.add_argument("--repeat", type=int, default=7)
    parser.add_argument(
        "--scenarios",
        default="http_keepalive,tls_http,websockets_messages,websockets_tls,"
        "aiohttp_websocket_messages,aiohttp_websocket_tls,mixed_streams,bulk_transfer",
    )
    args = parser.parse_args()
    if args.repeat < 1:
        parser.error("repeat must be positive")
    baseline = args.baseline_pythonpath.resolve()
    if not (baseline / "rsloop" / "__init__.py").is_file():
        parser.error("baseline directory must contain an installed rsloop package")
    root = Path(__file__).resolve().parents[1]
    output = {}
    for scenario in args.scenarios.split(","):
        rows = {"before": [], "after": []}
        for block in range(args.repeat):
            for variant in (
                ("before", "after") if block % 2 == 0 else ("after", "before")
            ):
                env = os.environ.copy()
                env["RSLOOP_USE_FAST_STREAMS"] = "1"
                if variant == "before":
                    env["PYTHONPATH"] = (
                        str(baseline) + os.pathsep + env.get("PYTHONPATH", "")
                    )
                result = subprocess.run(
                    [
                        sys.executable,
                        str(root / "benches" / "workload_matrix.py"),
                        "--child",
                        "--loop",
                        "rsloop",
                        "--scenario",
                        scenario,
                        "--requests-per-connection",
                        "500",
                        "--child-runs",
                        "3",
                    ],
                    env=env,
                    cwd=root,
                    capture_output=True,
                    text=True,
                    check=True,
                    timeout=120,
                )
                measured = json.loads(result.stdout.strip().splitlines()[-1])[-1]
                latencies = measured.pop("latency_ms")
                measured["p95_ms"] = percentile(latencies, 0.95)
                measured["p99_ms"] = percentile(latencies, 0.99)
                measured["block"] = block
                rows[variant].append(measured)
        comparison = latency_comparison(
            [r["seconds"] for r in rows["before"]],
            [r["seconds"] for r in rows["after"]],
            paired=True,
            threshold=3,
        )
        comparison["metric"] = "elapsed_seconds"
        output[scenario] = {"runs": rows, "comparison": comparison}
        medians = {
            key: statistics.median(r["seconds"] for r in runs)
            for key, runs in rows.items()
        }
        print(
            f"{scenario}: {medians}; {comparison['classification']}",
            file=sys.stderr,
            flush=True,
        )
    print(
        json.dumps(
            {
                "method": __doc__,
                "baseline_pythonpath": str(baseline),
                "python": sys.version,
                "results": output,
            },
            indent=2,
        )
    )


if __name__ == "__main__":
    main()
