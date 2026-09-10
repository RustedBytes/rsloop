from __future__ import annotations

import importlib.util
import sys
from pathlib import Path


def sampling_profiler_command(command: list[str], output: Path) -> list[str]:
    """Wrap a Python command with Python 3.15's sampling profiler."""
    if sys.version_info < (3, 15):
        raise RuntimeError(
            "profiling requires Python 3.15 or newer; rerun with "
            "`uv run --python 3.15 ...`"
        )
    if importlib.util.find_spec("profiling.sampling") is None:
        raise RuntimeError(
            "this interpreter does not provide the `profiling.sampling` module"
        )
    if not command or Path(command[0]).resolve() != Path(sys.executable).resolve():
        raise ValueError("the profiled command must use the current Python interpreter")

    output = output.expanduser().resolve().with_suffix(".html")
    output.parent.mkdir(parents=True, exist_ok=True)
    return [
        sys.executable,
        "-m",
        "profiling.sampling",
        "run",
        "--all-threads",
        "--native",
        "--flamegraph",
        "-o",
        str(output),
        *command[1:],
    ]
