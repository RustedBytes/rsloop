"""Exercise the feature-only native profiler in a fresh interpreter."""

import json
import os
import subprocess
import sys

import pytest
from rsloop import _loop

pytestmark = pytest.mark.skipif(
    not hasattr(_loop, "hotpath_start"), reason="requires a hotpath-profile build"
)


def test_profile_lifecycle_and_complete_native_report(tmp_path):
    output = tmp_path / "profile.json"
    program = r"""
import asyncio
import sys
import rsloop
from rsloop import _loop

def raises(kind, function, *args, **kwargs):
    try:
        function(*args, **kwargs)
    except kind:
        return
    raise AssertionError(f"expected {kind.__name__}")

raises(RuntimeError, _loop.hotpath_stop)
raises(ValueError, _loop.hotpath_start, sys.argv[1], format="invalid")
raises(ValueError, _loop.hotpath_start, sys.argv[1], time_sampling_rate=float("nan"))
_loop.hotpath_start(sys.argv[1])
raises(RuntimeError, _loop.hotpath_start, sys.argv[1])

async def main():
    loop = asyncio.get_running_loop()
    seen = []
    for i in range(100):
        loop.call_soon(seen.append, i)
    await asyncio.sleep(0.001)
    assert seen == list(range(100))
    async def echo(reader, writer):
        writer.write(await reader.readexactly(4))
        await writer.drain()
        writer.close()
        await writer.wait_closed()
    server = await asyncio.start_server(echo, "127.0.0.1", 0)
    try:
        reader, writer = await asyncio.open_connection("127.0.0.1", server.sockets[0].getsockname()[1])
        try:
            writer.write(b"test")
            assert await reader.readexactly(4) == b"test"
        finally:
            writer.close()
            await writer.wait_closed()
    finally:
        server.close()
        await server.wait_closed()

try:
    rsloop.run(asyncio.wait_for(main(), 10))
finally:
    _loop.hotpath_stop()
raises(RuntimeError, _loop.hotpath_stop)
raises(RuntimeError, _loop.hotpath_start, sys.argv[1])
"""
    env = {k: v for k, v in os.environ.items() if not k.startswith("HOTPATH_")}
    subprocess.run(
        [sys.executable, "-c", program, str(output)], env=env, check=True, timeout=30
    )
    report = json.loads(output.read_text())
    section = report.get("functions_timing", report.get("functions_alloc"))
    assert section["total_count"] == section["included_count"] > 50
    names = [row["name"] for row in section["data"]]
    for namespace in ("context", "engine", "bindings", "transport::stream", "vibeio"):
        assert any(name.startswith(f"rsloop::{namespace}::") for name in names)
    assert report["futures"]["data"]
    assert all(
        row["sampled_polls"] == row["total_polls"] for row in report["futures"]["data"]
    )
