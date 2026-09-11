"""Run Rust library tests against an explicitly linked Python interpreter."""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys
from pathlib import Path


def project_python(project_root: Path) -> Path:
    relative = Path("Scripts/python.exe") if os.name == "nt" else Path("bin/python")
    candidate = project_root / ".venv" / relative
    return candidate if candidate.is_file() else Path(sys.executable)


def python_link_config(interpreter: Path) -> tuple[str, str | None, str]:
    code = """
import json
import sys
import sysconfig

print(json.dumps({
    "python_home": sys.base_prefix,
    "libdir": sysconfig.get_config_var("LIBDIR"),
    "python_abi": (
        f"cp{sys.version_info.major}{sys.version_info.minor}"
        f"{'t' if sysconfig.get_config_var('Py_GIL_DISABLED') else ''}"
    ),
}))
"""
    result = subprocess.run(
        [str(interpreter), "-c", code],
        check=True,
        capture_output=True,
        text=True,
    )
    config = json.loads(result.stdout)
    return config["python_home"], config["libdir"], config["python_abi"]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--all-features", action="store_true")
    parser.add_argument("--features", help="Comma-separated Cargo features")
    args = parser.parse_args()
    project_root = Path(__file__).resolve().parent.parent
    interpreter = project_python(project_root)
    python_home, libdir, python_abi = python_link_config(interpreter)

    env = os.environ.copy()
    env["PYO3_PYTHON"] = str(interpreter)
    env["PYTHONHOME"] = python_home
    if os.name == "nt":
        env["PATH"] = os.pathsep.join((python_home, env.get("PATH", "")))
        env.setdefault(
            "CARGO_TARGET_DIR",
            str(project_root / "target" / "rust-tests" / python_abi),
        )
    elif libdir:
        rpath = f"-C link-arg=-Wl,-rpath,{libdir}"
        env["RUSTFLAGS"] = f"{env.get('RUSTFLAGS', '')} {rpath}".strip()

    command = ["cargo", "test", "--lib", "--locked"]
    if args.all_features:
        command.append("--all-features")
    if args.features:
        command.extend(["--features", args.features])
    return subprocess.run(
        command,
        cwd=project_root,
        env=env,
        check=False,
    ).returncode


if __name__ == "__main__":
    raise SystemExit(main())
