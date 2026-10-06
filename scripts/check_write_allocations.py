"""Enforce zero Rust heap allocations for warmed, bounded write batches.

Runs the real buffer/queue operations under the existing hotpath allocator.
Counts include nested calls, but exclude CPython's allocator and queue warmup.
"""

from __future__ import annotations

import argparse
import json
import os
import subprocess
from pathlib import Path

from run_rust_tests import project_python, python_link_config


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--output", type=Path, default=Path("target/allocations/write-batches.json")
    )
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[1]
    interpreter = project_python(root).resolve()
    python_home, libdir, python_abi = python_link_config(interpreter)
    report = args.output.resolve()
    report.parent.mkdir(parents=True, exist_ok=True)
    report.unlink(missing_ok=True)
    env = {
        key: value
        for key, value in os.environ.items()
        if not key.startswith("HOTPATH_")
    }
    env.update(
        PYO3_PYTHON=str(interpreter),
        PYTHONHOME=python_home,
        RSLOOP_ALLOCATION_REPORT=str(report),
        HOTPATH_ALLOC_METRIC="count",
        HOTPATH_ALLOC_CUMULATIVE="true",
    )
    env.setdefault("RUST_MIN_STACK", str(32 * 1024 * 1024))
    if os.name == "nt":
        env["PATH"] = os.pathsep.join((python_home, env.get("PATH", "")))
        env.setdefault(
            "CARGO_TARGET_DIR", str(root / "target" / "rust-tests" / python_abi)
        )
    elif libdir:
        env["RUSTFLAGS"] = (
            f"{env.get('RUSTFLAGS', '')} -C link-arg=-Wl,-rpath,{libdir}".strip()
        )
    subprocess.run(
        [
            "cargo",
            "test",
            "--lib",
            "--locked",
            "--features",
            "hotpath-alloc-profile",
            "warmed_python_batches_reuse_queue_storage",
            "--",
            "--nocapture",
        ],
        cwd=root,
        env=env,
        check=True,
    )
    data = json.loads(report.read_text())["functions_alloc"]
    if (
        data["profiling_mode"] != "alloc-count"
        or "cumulative" not in data["description"].lower()
    ):
        raise SystemExit(
            "Expected cumulative allocation counts, including nested calls"
        )
    probes = ["::warmed_batch_allocation_probe"]
    if os.name == "posix":
        probes.append("::warmed_transport_batch_allocation_probe")
    for probe in probes:
        rows = [row for row in data["data"] if row["name"].endswith(probe)]
        if len(rows) != 1 or rows[0]["calls"] != 1 or rows[0]["total"] != "0":
            raise SystemExit(f"Write allocation budget failed for {probe}: {rows}")
    print(
        "PASS: 0 Rust heap allocations per probe across 100 warmed batches (1,600 segments)."
    )


if __name__ == "__main__":
    main()
