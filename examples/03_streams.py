from __future__ import annotations

import asyncio
from typing import cast

import rsloop


class EchoServerProtocol(asyncio.Protocol):
    transport: asyncio.Transport

    def connection_made(self, transport: asyncio.BaseTransport) -> None:
        transport = cast(asyncio.Transport, transport)
        self.transport = transport

    def data_received(self, data: bytes) -> None:
        self.transport.write(data.upper())


class ClientProtocol(asyncio.Protocol):
    transport: asyncio.Transport

    def __init__(self, done: asyncio.Future[str]) -> None:
        self.done = done

    def connection_made(self, transport: asyncio.BaseTransport) -> None:
        transport = cast(asyncio.Transport, transport)
        self.transport = transport
        transport.write(b"hello over tcp")

    def data_received(self, data: bytes) -> None:
        if not self.done.done():
            self.done.set_result(data.decode())
        self.transport.close()


async def main() -> None:
    loop = asyncio.get_running_loop()

    server = await loop.create_server(EchoServerProtocol, "127.0.0.1", 0)
    host, port = server.sockets[0].getsockname()[:2]

    transport: asyncio.BaseTransport | None = None
    try:
        done: asyncio.Future[str] = loop.create_future()
        transport, _ = await loop.create_connection(
            lambda: ClientProtocol(done), host, port
        )
        print(
            "create_connection/create_server:",
            await asyncio.wait_for(done, 1.0),
        )
    finally:
        if transport is not None:
            transport.close()
        server.close()
        await server.wait_closed()


if __name__ == "__main__":
    rsloop.run(main())
