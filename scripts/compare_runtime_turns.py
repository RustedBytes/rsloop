"""Compare identical runtime_turns benchmark sources built against two revisions."""

from __future__ import annotations

import argparse
import json
import os
import platform
import random
import subprocess
from pathlib import Path

from hotpath_lab import balanced_orders, paired_estimate, sha256, write_json


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--baseline", type=Path, required=True)
    parser.add_argument("--candidate", type=Path, required=True)
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--blocks", type=int, default=24)
    parser.add_argument("--iterations", type=int, default=3_000_000)
    parser.add_argument("--seed", type=int, default=123)
    parser.add_argument("--tasks", type=int, default=0)
    args = parser.parse_args()
    if args.iterations <= 0 or args.tasks < 0:
        parser.error("--iterations must be positive and --tasks nonnegative")
    binaries = {
        name: getattr(args, name).resolve() for name in ("baseline", "candidate")
    }
    orders = balanced_orders(args.blocks, random.Random(args.seed))
    args.out.mkdir(parents=True, exist_ok=False)
    hashes = {name: sha256(path) for name, path in binaries.items()}
    write_json(
        args.out / "plan.json",
        {
            "binaries": {name: str(path) for name, path in binaries.items()},
            "sha256": hashes,
            "orders": orders,
            "iterations": args.iterations,
            "tasks": args.tasks,
            "seed": args.seed,
            "minimum_gain_pct": 1,
            "minimum_sample_seconds": 0.25,
            "platform": platform.platform(),
            "affinity": sorted(os.sched_getaffinity(0))
            if hasattr(os, "sched_getaffinity")
            else None,
            "runner_sha256": sha256(Path(__file__)),
        },
    )
    values = {name: [] for name in binaries}
    with (args.out / "samples.jsonl").open("x") as samples:
        for block, order in enumerate(orders):
            for name in order:
                result = json.loads(
                    subprocess.check_output(
                        [str(binaries[name]), str(args.iterations), str(args.tasks)],
                        text=True,
                        timeout=120,
                    )
                )
                assert result["iterations"] == args.iterations
                assert result["tasks"] == args.tasks
                values[name].append(result["seconds"])
                samples.write(
                    json.dumps({"block": block, "label": name, **result}) + "\n"
                )
                samples.flush()
            print(f"Completed paired block {block + 1}/{args.blocks}", flush=True)
    if hashes != {name: sha256(path) for name, path in binaries.items()}:
        raise ValueError("Benchmark binaries changed during measurement")
    report = paired_estimate(**values, seed=args.seed)
    report["min_observed_seconds"] = min(values["baseline"] + values["candidate"])
    reliable = args.blocks >= 12 and report["min_observed_seconds"] >= 0.25
    report["decision"] = (
        "timing_gate_passed"
        if reliable and report["ci_pct"][1] < -1
        else "inconclusive"
    )
    write_json(args.out / "report.json", report)
    print(json.dumps(report, indent=2))


if __name__ == "__main__":
    main()
