"""Subprocess defaults that differ between asyncio's high- and low-level APIs."""

from __future__ import annotations

import asyncio
import sys

import rsloop


def test_create_subprocess_exec_defaults_to_inherit() -> None:
    async def main() -> None:
        proc = await asyncio.create_subprocess_exec(
            sys.executable,
            "-c",
            "import sys;sys.exit(0)",
        )
        await proc.wait()
        assert proc.stdin is None
        assert proc.stdout is None
        assert proc.stderr is None

    rsloop.run(main())


def test_loop_subprocess_exec_defaults_to_pipe() -> None:
    async def main() -> None:
        loop = asyncio.get_running_loop()
        transport, _ = await loop.subprocess_exec(
            asyncio.SubprocessProtocol,
            sys.executable,
            "-c",
            "import sys;sys.exit(0)",
        )
        try:
            assert transport.get_pipe_transport(0) is not None
            assert transport.get_pipe_transport(1) is not None
            assert transport.get_pipe_transport(2) is not None
        finally:
            transport.close()

    rsloop.run(main())
