from __future__ import annotations

import asyncio
import os
import socket
import tempfile

import rsloop


class UppercaseProtocol(asyncio.Protocol):
    def __init__(self, done: asyncio.Future[str] | None = None) -> None:
        self.done = done

    def connection_made(self, transport: asyncio.BaseTransport) -> None:
        self.transport = transport
        if self.done is not None:
            transport.write(b"hello over unix")

    def data_received(self, data: bytes) -> None:
        if self.done is None:
            self.transport.write(data.upper())
            return

        if not self.done.done():
            self.done.set_result(data.decode())
        self.transport.close()


async def demo_unix_streams() -> None:
    loop = asyncio.get_running_loop()

    with tempfile.TemporaryDirectory() as tmpdir:
        path = os.path.join(tmpdir, "rsloop-rust.sock")
        server = await loop.create_unix_server(UppercaseProtocol, path)

        done: asyncio.Future[str] = loop.create_future()
        transport, _ = await loop.create_unix_connection(
            lambda: UppercaseProtocol(done), path
        )
        print(
            "create_unix_connection/create_unix_server:",
            await asyncio.wait_for(done, 1.0),
        )

        transport.close()
        server.close()
        await server.wait_closed()


class AcceptedSocketProtocol(asyncio.Protocol):
    def __init__(self, done: asyncio.Future[str]) -> None:
        self.done = done

    def connection_made(self, transport: asyncio.BaseTransport) -> None:
        self.transport = transport

    def data_received(self, data: bytes) -> None:
        self.transport.write(data.upper())

    def connection_lost(self, exc: Exception | None) -> None:
        if not self.done.done():
            self.done.set_result("transport-closed")


async def demo_connect_accepted_socket() -> None:
    loop = asyncio.get_running_loop()

    listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    listener.bind(("127.0.0.1", 0))
    listener.listen(1)

    host, port = listener.getsockname()[:2]
    accept_future = loop.run_in_executor(None, listener.accept)

    client = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    client.setblocking(False)
    await loop.sock_connect(client, (host, port))

    accepted, _ = await asyncio.wait_for(accept_future, 1.0)
    accepted.setblocking(False)

    done: asyncio.Future[str] = loop.create_future()
    transport, _ = await loop.connect_accepted_socket(
        lambda: AcceptedSocketProtocol(done), accepted
    )

    await loop.sock_sendall(client, b"accepted socket")
    reply = await loop.sock_recv(client, 32)
    print("connect_accepted_socket:", reply.decode())

    transport.close()
    await asyncio.wait_for(done, 1.0)
    client.close()
    listener.close()


async def main() -> None:
    await demo_unix_streams()
    await demo_connect_accepted_socket()


if __name__ == "__main__":
    rsloop.run(main())
