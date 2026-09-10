from __future__ import annotations

import asyncio
import socket
from typing import Any, cast

import rsloop
from granian.server.embed import Server
from litestar import Litestar, get


@get("/")
async def index() -> dict[str, str]:
    loop = asyncio.get_running_loop()
    return {
        "ok": "litestar-granian-rsloop",
        "loop": f"{type(loop).__module__}.{type(loop).__name__}",
    }


app = Litestar([index])


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


async def wait_for_server(port: int) -> bytes:
    last_error: BaseException | None = None
    for _ in range(100):
        try:
            return await get_http("/", port)
        except (ConnectionRefusedError, OSError) as exc:
            last_error = exc
            await asyncio.sleep(0.05)
    raise RuntimeError("Litestar/Granian server did not start") from last_error


async def main() -> None:
    port = reserve_port()
    server = Server(
        app,
        address="127.0.0.1",
        port=port,
        interface=cast(Any, "asgi"),
        log_enabled=False,
        log_access=False,
    )
    task = asyncio.create_task(server.serve())
    try:
        response = await wait_for_server(port)
        assert b"litestar-granian-rsloop" in response, response
        assert b"rsloop" in response, response
        print("litestar-granian ok")
    finally:
        server.stop()
        await task


if __name__ == "__main__":
    rsloop.run(main())
