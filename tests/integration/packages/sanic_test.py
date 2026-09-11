from __future__ import annotations

import asyncio
import os
import socket
import subprocess
import sys
from typing import Any

import rsloop
from sanic import Sanic
from sanic.response import json


app = Sanic("rsloop_sanic_smoke")


@app.get("/")
async def index(request: Any) -> Any:
    loop = asyncio.get_running_loop()
    return json(
        {
            "ok": "sanic-rsloop",
            "loop": f"{type(loop).__module__}.{type(loop).__name__}",
        }
    )


def reserve_port() -> int:
    sock = socket.socket()
    try:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])
    finally:
        sock.close()


async def get(path: str, port: int) -> bytes:
    reader, writer = await asyncio.open_connection("127.0.0.1", port)
    try:
        writer.write(
            (
                f"GET {path} HTTP/1.1\r\n"
                f"Host: 127.0.0.1:{port}\r\n"
                "Connection: close\r\n\r\n"
            ).encode()
        )
        await writer.drain()
        return await reader.read()
    finally:
        writer.close()
        await writer.wait_closed()


async def wait_for_server(port: int) -> bytes:
    last_error: BaseException | None = None
    for _ in range(100):
        try:
            return await get("/", port)
        except (ConnectionRefusedError, OSError) as exc:
            last_error = exc
            await asyncio.sleep(0.05)
    raise RuntimeError("Sanic server did not start") from last_error


async def parent_main() -> None:
    port = reserve_port()
    env = os.environ.copy()
    env["PYTHONPATH"] = os.getcwd() + os.pathsep + env.get("PYTHONPATH", "")
    proc = subprocess.Popen(
        [sys.executable, __file__, "--serve", str(port)],
        env=env,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        text=True,
    )
    try:
        response = await wait_for_server(port)
        assert b"sanic-rsloop" in response, response
        assert b"rsloop" in response, response
        print("sanic ok")
    finally:
        proc.terminate()
        try:
            proc.communicate(timeout=5)
        except subprocess.TimeoutExpired:
            proc.kill()
            proc.communicate(timeout=5)


def serve(port: int) -> None:
    rsloop.install()
    app.run(
        host="127.0.0.1",
        port=port,
        access_log=False,
        motd=False,
        single_process=True,
    )


if __name__ == "__main__":
    if len(sys.argv) == 3 and sys.argv[1] == "--serve":
        serve(int(sys.argv[2]))
    else:
        rsloop.run(parent_main())
