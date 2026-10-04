"""Native stream selection is mandatory on rsloop, including deferred calls."""

import asyncio
import os
import subprocess
import sys
from pathlib import Path
from typing import Any, cast

import pytest
import rsloop
import rsloop._loop as loop_module

PyFastStreamReader = cast(Any, loop_module).PyFastStreamReader
PyFastStreamWriter = cast(Any, loop_module).PyFastStreamWriter


def test_rsloop_streams_do_not_call_stdlib_helpers(monkeypatch):
    def forbidden(*args, **kwargs):
        raise AssertionError("rsloop fell back to a stdlib stream helper")

    monkeypatch.setattr(asyncio.streams, "open_connection", forbidden)
    monkeypatch.setattr(asyncio.streams, "start_server", forbidden)
    loop = rsloop.new_event_loop()
    accepted = []

    async def echo(reader, writer):
        accepted.append((type(reader), type(writer)))
        try:
            writer.write(await reader.readexactly(4))
            await writer.drain()
        finally:
            writer.close()
            await writer.wait_closed()

    # Constructing either coroutine without a running loop must not select
    # stdlib readers, and both helpers must remain usable with create_task().
    startup = asyncio.start_server(echo, "127.0.0.1", 0)
    try:
        server = loop.run_until_complete(startup)
        pending = asyncio.open_connection("127.0.0.1", server.sockets[0].getsockname()[1])

        async def exchange():
            reader, writer = await asyncio.create_task(pending)
            assert type(reader) is PyFastStreamReader
            assert type(writer) is PyFastStreamWriter
            try:
                writer.write(b"test")
                await writer.drain()
                assert await reader.readexactly(4) == b"test"
            finally:
                writer.close()
                await writer.wait_closed()

        try:
            loop.run_until_complete(asyncio.wait_for(exchange(), 5))
            assert accepted == [(PyFastStreamReader, PyFastStreamWriter)]
        finally:
            server.close()
            loop.run_until_complete(server.wait_closed())
    finally:
        loop.close()


def test_legacy_environment_switch_cannot_disable_native_streams():
    env = os.environ.copy()
    env["RSLOOP_USE_FAST_STREAMS"] = "0"
    subprocess.run(
        [
            sys.executable,
            "-c",
            (
                "import runpy, sys\n"
                "from pytest import MonkeyPatch\n"
                "with MonkeyPatch.context() as patch:\n"
                "    runpy.run_path(sys.argv[1])['test_rsloop_streams_do_not_call_stdlib_helpers'](patch)\n"
            ),
            str(Path(__file__).resolve()),
        ],
        env=env,
        check=True,
        timeout=15,
    )


def test_asyncio_streams_namespace_also_uses_native_helpers():
    assert asyncio.streams.open_connection is asyncio.open_connection
    assert asyncio.streams.start_server is asyncio.start_server


def test_native_entry_points_reject_other_loops():
    from rsloop import _loop

    async def main():
        with pytest.raises(RuntimeError, match="require an rsloop event loop"):
            _ = _loop.open_connection("127.0.0.1", 1)
        with pytest.raises(RuntimeError, match="require an rsloop event loop"):
            _ = _loop.start_server(lambda *_: None, "127.0.0.1", 0)

    asyncio.run(main())


@pytest.mark.parametrize("loop_name", ["asyncio", "uvloop"])
def test_other_loops_keep_their_stdlib_streams(loop_name):
    if loop_name == "uvloop":
        factory = pytest.importorskip("uvloop").new_event_loop
    else:
        factory = asyncio.new_event_loop
    loop = factory()
    accepted = []

    async def echo(reader, writer):
        accepted.append(type(reader))
        writer.close()
        await writer.wait_closed()

    async def main():
        server = await asyncio.start_server(echo, "127.0.0.1", 0)
        try:
            reader, writer = await asyncio.open_connection(
                "127.0.0.1", server.sockets[0].getsockname()[1]
            )
            try:
                assert type(reader) is asyncio.StreamReader
                assert type(writer) is asyncio.StreamWriter
                await reader.read()
                assert accepted == [asyncio.StreamReader]
            finally:
                writer.close()
                await writer.wait_closed()
        finally:
            server.close()
            await server.wait_closed()

    try:
        loop.run_until_complete(asyncio.wait_for(main(), 5))
    finally:
        loop.close()
