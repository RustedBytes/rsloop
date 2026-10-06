"""Enforce zero Rust heap allocations for stable vibeio completion owners.

Also checks borrowed TCP vectored polling with the standalone counting allocator.
Completion cancellation and socket/runtime setup are outside these budgets.
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
        "--output", type=Path, default=Path("target/allocations/vibeio.json")
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
        RSLOOP_VIBEIO_ALLOCATION_REPORT=str(report),
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
            "stable_completion_owners_reuse_storage",
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
    probe = "::stable_completion_allocation_probe"
    rows = [row for row in data["data"] if row["name"].endswith(probe)]
    if len(rows) != 1 or rows[0]["calls"] != 1 or rows[0]["total"] != "0":
        raise SystemExit(f"Completion allocation budget failed: {rows}")
    measured = subprocess.check_output(
        [
            "cargo",
            "run",
            "--quiet",
            "--release",
            "--locked",
            "--manifest-path",
            "tools/vibeio-check/Cargo.toml",
            "--example",
            "vectored_poll",
            "--",
            "1000",
        ],
        cwd=root,
        text=True,
    )
    result = json.loads(measured)
    if result["allocations"] != 0 or result["rounds"] != 1000:
        raise SystemExit(f"Vectored polling allocation budget failed: {result}")
    for tasks in (0, 16):
        result = json.loads(
            subprocess.check_output(
                [
                    "cargo",
                    "run",
                    "--quiet",
                    "--release",
                    "--locked",
                    "--manifest-path",
                    "tools/vibeio-check/Cargo.toml",
                    "--example",
                    "embedded_turns",
                    "--",
                    "1000",
                    str(tasks),
                ],
                cwd=root,
                text=True,
            )
        )
        if result["allocations"] != 0 or result["rounds"] != 1000:
            raise SystemExit(f"Embedded turn allocation budget failed: {result}")
    print(
        "PASS: 0 Rust heap allocations for 1,000 stable owner handoffs, vectored writes, and embedded turns (idle and 16 ready tasks)."
    )


if __name__ == "__main__":
    main()
