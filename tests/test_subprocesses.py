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


def test_exited_process_releases_stdin_for_inherited_pipe_reader() -> None:
    code = (
        "import subprocess, sys; "
        "subprocess.Popen([sys.executable, '-c', "
        "'import select, sys; select.select([sys.stdin], [], [], 10)'])"
    )

    async def main() -> None:
        proc = await asyncio.create_subprocess_exec(
            sys.executable,
            "-c",
            code,
            stdin=asyncio.subprocess.PIPE,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        assert proc.stdin is not None
        assert proc.stdout is not None
        assert proc.stderr is not None
        assert await asyncio.wait_for(proc.wait(), 5) == 0
        proc.stdin.close()
        await asyncio.wait_for(proc.stdout.read(), 3)
        await asyncio.wait_for(proc.stderr.read(), 3)

    rsloop.run(main())


def test_process_exit_notification_follows_stdin_release() -> None:
    class Protocol(asyncio.SubprocessProtocol):
        def __init__(self) -> None:
            self.events: list[str] = []
            self.exited = asyncio.Event()

        def pipe_connection_lost(self, fd: int, exc: Exception | None) -> None:
            if fd == 0:
                self.events.append("stdin closed")

        def process_exited(self) -> None:
            self.events.append("process exited")
            self.exited.set()

    async def main() -> None:
        loop = asyncio.get_running_loop()
        transport, protocol = await loop.subprocess_exec(
            Protocol,
            sys.executable,
            "-c",
            "pass",
            stdin=asyncio.subprocess.PIPE,
            stdout=asyncio.subprocess.DEVNULL,
            stderr=asyncio.subprocess.DEVNULL,
        )
        try:
            await asyncio.wait_for(protocol.exited.wait(), 5)
            assert protocol.events == ["stdin closed", "process exited"]
        finally:
            transport.close()

    rsloop.run(main())
