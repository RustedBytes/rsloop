"""Run the independent Rust and Python core suites concurrently."""

from __future__ import annotations

import subprocess
import sys
from pathlib import Path

ROOT_DIR = Path(__file__).resolve().parents[1]


def main() -> int:
    commands = [
        ("rust", [sys.executable, "scripts/run_rust_tests.py"]),
        ("python", [sys.executable, "-u", "scripts/run_python_tests.py"]),
    ]
    processes: list[tuple[str, subprocess.Popen[bytes]]] = []
    try:
        for name, command in commands:
            print(f"Starting {name} tests", flush=True)
            processes.append((name, subprocess.Popen(command, cwd=ROOT_DIR)))
        results = [(name, process.wait()) for name, process in processes]
    except BaseException:
        for _, process in processes:
            if process.poll() is None:
                process.terminate()
        for _, process in processes:
            process.wait()
        raise

    failures = [(name, code) for name, code in results if code != 0]
    for name, code in failures:
        print(f"{name} tests failed with exit code {code}", file=sys.stderr)
    return failures[0][1] if failures else 0


if __name__ == "__main__":
    raise SystemExit(main())
