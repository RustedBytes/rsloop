from __future__ import annotations

import asyncio
import socket
from typing import Any


def reserve_port() -> int:
    sock = socket.socket()
    try:
        sock.bind(("127.0.0.1", 0))
        return int(sock.getsockname()[1])
    finally:
        sock.close()


async def get_http(path: str, port: int) -> bytes:
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


async def wait_for_http(port: int, path: str = "/") -> bytes:
    last_error: BaseException | None = None
    for _ in range(100):
        try:
            return await get_http(path, port)
        except (ConnectionRefusedError, OSError) as exc:
            last_error = exc
            await asyncio.sleep(0.05)
    raise RuntimeError("server did not start") from last_error


async def wait_started(server: Any, name: str) -> None:
    for _ in range(100):
        if server.started:
            return
        await asyncio.sleep(0.05)
    raise RuntimeError(f"{name} server did not start")


async def run_uvicorn_app(
    app: Any,
    marker: bytes,
    *,
    name: str,
    lifespan: str = "on",
) -> None:
    import uvicorn

    port = reserve_port()
    server = uvicorn.Server(
        uvicorn.Config(
            app,
            host="127.0.0.1",
            port=port,
            loop="none",
            lifespan=lifespan,
            log_level="warning",
            access_log=False,
        )
    )

    task = asyncio.create_task(server.serve())
    try:
        await wait_started(server, name)
        response = await get_http("/", port)
        assert marker in response, response
        assert b"rsloop" in response, response
        print(f"{name} ok")
    finally:
        server.should_exit = True
        await task
