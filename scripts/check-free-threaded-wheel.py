"""Install and check a cp315t wheel in a fresh, isolated uv environment."""

from __future__ import annotations

import argparse
import subprocess
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SMOKE = r"""
import asyncio
import json
from pathlib import Path
import sys
import sysconfig

assert sys.version_info[:2] == (3, 15), sys.version
assert sysconfig.get_config_var("Py_GIL_DISABLED") == 1
assert not sys._is_gil_enabled(), "GIL enabled before import"
import rsloop
import rsloop._loop as native
assert Path(rsloop.__file__).resolve().is_relative_to(Path(sys.prefix).resolve())
assert Path(native.__file__).resolve().is_relative_to(Path(sys.prefix).resolve())
info = rsloop.build_info()
assert info["free_threaded"] is True, info
assert not sys._is_gil_enabled(), "import re-enabled the GIL"
print(json.dumps({"python": sys.version, "package": rsloop.__file__,
                  "extension": native.__file__, "build_info": info}), flush=True)

async def echo(reader, writer):
    try:
        writer.write(await reader.readexactly(4))
        await writer.drain()
    finally:
        writer.close()
        await writer.wait_closed()

async def exercise():
    loop = asyncio.get_running_loop()
    seen = []
    loop.call_soon(seen.append, "soon")
    loop.call_soon_threadsafe(seen.append, "threadsafe")
    await asyncio.sleep(0)
    assert seen == ["soon", "threadsafe"], seen
    server = await asyncio.start_server(echo, "127.0.0.1", 0)
    try:
        port = server.sockets[0].getsockname()[1]
        reader, writer = await asyncio.open_connection("127.0.0.1", port)
        try:
            writer.write(b"ping")
            await writer.drain()
            assert await reader.readexactly(4) == b"ping"
        finally:
            writer.close()
            await writer.wait_closed()
    finally:
        server.close()
        await server.wait_closed()

loop = rsloop.new_event_loop()
asyncio.set_event_loop(loop)
try:
    loop.run_until_complete(asyncio.wait_for(exercise(), 15))
finally:
    asyncio.set_event_loop(None)
    loop.close()
assert not sys._is_gil_enabled(), "smoke re-enabled the GIL"
print("PASS: installed-wheel import, build_info, callbacks and TCP", flush=True)
"""


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("wheel_dir", type=Path)
    args = parser.parse_args()
    wheels = sorted(args.wheel_dir.resolve().glob("*-cp315-cp315t-*.whl"))
    if len(wheels) != 1:
        raise SystemExit(f"Expected one cp315t wheel, found: {wheels}")
    print(f"Checking wheel: {wheels[0].name}", flush=True)
    with tempfile.TemporaryDirectory(prefix="rsloop-cp315t-") as work:
        venv = Path(work) / "venv"
        subprocess.run(
            ["uv", "venv", "--no-project", "--python", "3.15t", str(venv)], check=True
        )
        python = venv / (
            "Scripts/python.exe" if (venv / "Scripts").exists() else "bin/python"
        )
        subprocess.run(
            [
                "uv",
                "pip",
                "install",
                "--python",
                str(python),
                str(wheels[0]),
                "pytest>=8.4",
            ],
            check=True,
        )
        subprocess.run(
            [str(python), "-I", "-c", SMOKE], cwd=work, check=True, timeout=60
        )
        subprocess.run(
            [
                str(python),
                "-I",
                "-u",
                str(ROOT / "scripts/run_python_tests.py"),
                "-q",
                "tests/test_free_threading.py",
                "tests/test_fast_callbacks.py",
                "tests/test_run.py::TestRun::test_build_info_describes_native_extension",
            ],
            cwd=ROOT,
            check=True,
            timeout=300,
        )


if __name__ == "__main__":
    main()
