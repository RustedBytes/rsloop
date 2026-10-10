"""Exercise the redis-py/hiredis connection checkout regression in issue #106.

Requires redis-server on PATH (or RSLOOP_REDIS_SERVER), redis-py and hiredis.
Only a private temporary server is used; no existing database is modified.
"""

from __future__ import annotations

import asyncio
import os
import shutil
import socket
import subprocess
import time

import pytest
import rsloop


@pytest.fixture
def redis_port(tmp_path):
    pytest.importorskip("redis")
    pytest.importorskip("hiredis")
    executable = os.environ.get("RSLOOP_REDIS_SERVER") or shutil.which("redis-server")
    if not executable:
        pytest.skip("redis-server is unavailable")
    with socket.socket() as sock:
        sock.bind(("127.0.0.1", 0))
        port = sock.getsockname()[1]
    with (tmp_path / "redis.log").open("w+") as log:
        process = subprocess.Popen(
            [
                executable,
                "--bind",
                "127.0.0.1",
                "--port",
                str(port),
                "--save",
                "",
                "--appendonly",
                "no",
                "--dir",
                str(tmp_path),
            ],
            stdout=log,
            stderr=subprocess.STDOUT,
        )
        try:
            deadline = time.monotonic() + 10
            while True:
                try:
                    with socket.create_connection(("127.0.0.1", port), timeout=0.1):
                        break
                except OSError:
                    if process.poll() is not None or time.monotonic() >= deadline:
                        log.seek(0)
                        pytest.fail(f"Redis startup failed: {log.read()}")
                    time.sleep(0.01)
            yield port
        finally:
            process.terminate()
            try:
                process.wait(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=5)


@pytest.mark.parametrize("backend", ["asyncio", "rsloop", "uvloop"])
def test_hiredis_checkout_set_get_pipeline(redis_port, backend):
    redis_asyncio = pytest.importorskip("redis.asyncio")
    hiredis_parser = pytest.importorskip("redis._parsers.hiredis")._AsyncHiredisParser

    factory = (
        pytest.importorskip("uvloop").new_event_loop
        if backend == "uvloop"
        else (asyncio if backend == "asyncio" else rsloop).new_event_loop
    )

    async def main():
        pool = redis_asyncio.ConnectionPool(
            host="127.0.0.1",
            port=redis_port,
            max_connections=1,
            parser_class=hiredis_parser,
        )
        client = redis_asyncio.Redis(connection_pool=pool)
        try:
            # A single connection forces each checkout to probe the same reader.
            for index in range(3):
                assert await client.set("issue-106", f"value-{index}")
                assert await client.get("issue-106") == f"value-{index}".encode()
                for transaction in (False, True):
                    async with client.pipeline(transaction=transaction) as pipe:
                        pipe.set("issue-106", "pipeline")
                        pipe.get("issue-106")
                        assert await pipe.execute() == [True, b"pipeline"]
            connection = await pool.get_connection()
            try:
                assert isinstance(connection._parser, hiredis_parser)
                if backend == "rsloop":
                    assert type(connection._reader).__name__ == "PyFastStreamReader"
                assert await connection._parser.can_read_destructive() is False
                await connection.send_command("PING")
                assert await connection.read_response() == b"PONG"
            finally:
                await pool.release(connection)
        finally:
            await client.aclose()
            await pool.aclose()

    loop = factory()
    try:
        loop.run_until_complete(asyncio.wait_for(main(), 10))
    finally:
        loop.run_until_complete(loop.shutdown_asyncgens())
        loop.close()
