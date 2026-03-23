from __future__ import annotations

import asyncio
import os
import pathlib
import socket
import ssl
import tempfile
import unittest

import rsloop


TLS_FIXTURES_DIR = pathlib.Path(__file__).with_name("fixtures").joinpath("tls")


def make_cert_files(tmpdir: str) -> tuple[str, str, str]:
    ca_cert_path = os.path.join(tmpdir, "ca-cert.pem")
    cert_path = os.path.join(tmpdir, "cert.pem")
    key_path = os.path.join(tmpdir, "key.pem")
    pathlib.Path(ca_cert_path).write_bytes(
        TLS_FIXTURES_DIR.joinpath("ca-cert.pem").read_bytes()
    )
    pathlib.Path(cert_path).write_bytes(
        TLS_FIXTURES_DIR.joinpath("cert.pem").read_bytes()
    )
    pathlib.Path(key_path).write_bytes(
        TLS_FIXTURES_DIR.joinpath("key.pem").read_bytes()
    )
    return ca_cert_path, cert_path, key_path


def make_ssl_contexts(tmpdir: str):
    import ssl

    ca_cert_path, cert_path, key_path = make_cert_files(tmpdir)
    server_ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    server_ctx.load_cert_chain(cert_path, key_path)

    client_ctx = ssl.create_default_context(ssl.Purpose.SERVER_AUTH)
    client_ctx.load_verify_locations(cafile=ca_cert_path)
    client_ctx.check_hostname = True

    return server_ctx, client_ctx


class TlsTests(unittest.TestCase):
    def test_create_default_context_marks_default_verify_paths(self) -> None:
        context = ssl.create_default_context()
        self.assertTrue(context.__dict__.get("_rsloop_use_default_verify_paths"))

    def test_create_connection_and_server_tls_round_trip(self) -> None:
        async def main() -> tuple[str, tuple[int, ...]]:
            loop = asyncio.get_running_loop()
            done: asyncio.Future[str] = loop.create_future()
            result = ""
            server_fds: tuple[int, ...] = ()

            class ServerProtocol(asyncio.Protocol):
                def connection_made(self, transport: asyncio.BaseTransport) -> None:
                    self.transport = transport

                def data_received(self, data: bytes) -> None:
                    self.transport.write(data.upper())
                    self.transport.close()

                def connection_lost(self, exc: Exception | None) -> None:
                    if not done.done():
                        done.set_result("server-closed")

            class ClientProtocol(asyncio.Protocol):
                def __init__(self) -> None:
                    self.parts: list[bytes] = []
                    self.result: asyncio.Future[str] = loop.create_future()

                def connection_made(self, transport: asyncio.BaseTransport) -> None:
                    self.transport = transport
                    transport.write(b"tls-ok")

                def data_received(self, data: bytes) -> None:
                    self.parts.append(data)

                def connection_lost(self, exc: Exception | None) -> None:
                    if not self.result.done():
                        self.result.set_result(b"".join(self.parts).decode())

            with tempfile.TemporaryDirectory() as tmpdir:
                server_ctx, client_ctx = make_ssl_contexts(tmpdir)
                server = await loop.create_server(
                    ServerProtocol,
                    "127.0.0.1",
                    0,
                    ssl=server_ctx,
                )
                try:
                    port = server.sockets[0].getsockname()[1]
                    client_protocol = ClientProtocol()
                    await loop.create_connection(
                        lambda: client_protocol,
                        "127.0.0.1",
                        port,
                        ssl=client_ctx,
                        server_hostname="localhost",
                    )
                    self.assertEqual(
                        await asyncio.wait_for(client_protocol.result, 5.0), "TLS-OK"
                    )
                    self.assertEqual(await asyncio.wait_for(done, 5.0), "server-closed")
                    result = "ok"
                finally:
                    server.close()
                    await server.wait_closed()
                    server_fds = tuple(sock.fileno() for sock in server.sockets)

            return result, server_fds

        result, server_fds = rsloop.run(main())
        self.assertEqual(result, "ok")
        self.assertEqual(server_fds, (-1,))

    @unittest.skipIf(os.name == "nt", "Unix sockets are Unix-only")
    def test_create_unix_connection_and_server_tls_round_trip(self) -> None:
        async def main() -> tuple[str, tuple[int, ...]]:
            loop = asyncio.get_running_loop()
            result = ""
            server_fds: tuple[int, ...] = ()

            class ServerProtocol(asyncio.Protocol):
                def connection_made(self, transport: asyncio.BaseTransport) -> None:
                    self.transport = transport

                def data_received(self, data: bytes) -> None:
                    self.transport.write(b"unix:" + data)
                    self.transport.close()

            class ClientProtocol(asyncio.Protocol):
                def __init__(self) -> None:
                    self.parts: list[bytes] = []
                    self.done: asyncio.Future[str] = loop.create_future()

                def connection_made(self, transport: asyncio.BaseTransport) -> None:
                    transport.write(b"tls")

                def data_received(self, data: bytes) -> None:
                    self.parts.append(data)

                def connection_lost(self, exc: Exception | None) -> None:
                    if not self.done.done():
                        self.done.set_result(b"".join(self.parts).decode())

            with tempfile.TemporaryDirectory() as tmpdir:
                server_ctx, client_ctx = make_ssl_contexts(tmpdir)
                path = os.path.join(tmpdir, "sock")
                server = await loop.create_unix_server(
                    ServerProtocol,
                    path,
                    ssl=server_ctx,
                )
                try:
                    client_protocol = ClientProtocol()
                    await loop.create_unix_connection(
                        lambda: client_protocol,
                        path,
                        ssl=client_ctx,
                        server_hostname="localhost",
                    )
                    result = await asyncio.wait_for(client_protocol.done, 5.0)
                finally:
                    server.close()
                    await server.wait_closed()
                    server_fds = tuple(sock.fileno() for sock in server.sockets)

            return result, server_fds

        result, server_fds = rsloop.run(main())
        self.assertEqual(result, "unix:tls")
        self.assertEqual(server_fds, (-1,))

    def test_connect_accepted_socket_tls_round_trip(self) -> None:
        async def main() -> str:
            loop = asyncio.get_running_loop()

            class AcceptedProtocol(asyncio.Protocol):
                def connection_made(self, transport: asyncio.BaseTransport) -> None:
                    self.transport = transport

                def data_received(self, data: bytes) -> None:
                    self.transport.write(b"accepted:" + data)
                    self.transport.close()

            class ClientProtocol(asyncio.Protocol):
                def __init__(self) -> None:
                    self.parts: list[bytes] = []
                    self.done: asyncio.Future[str] = loop.create_future()

                def connection_made(self, transport: asyncio.BaseTransport) -> None:
                    transport.write(b"socket")

                def data_received(self, data: bytes) -> None:
                    self.parts.append(data)

                def connection_lost(self, exc: Exception | None) -> None:
                    if not self.done.done():
                        self.done.set_result(b"".join(self.parts).decode())

            with tempfile.TemporaryDirectory() as tmpdir:
                server_ctx, client_ctx = make_ssl_contexts(tmpdir)
                listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
                listener.bind(("127.0.0.1", 0))
                listener.listen(1)
                listener.setblocking(False)
                try:
                    port = listener.getsockname()[1]
                    connect_task = asyncio.ensure_future(
                        loop.create_connection(
                            ClientProtocol,
                            "127.0.0.1",
                            port,
                            ssl=client_ctx,
                            server_hostname="localhost",
                        )
                    )
                    accepted, _ = await loop.sock_accept(listener)
                    await loop.connect_accepted_socket(
                        AcceptedProtocol,
                        accepted,
                        ssl=server_ctx,
                    )
                    _, client_protocol = await connect_task
                    return await asyncio.wait_for(client_protocol.done, 5.0)
                finally:
                    listener.close()

        self.assertEqual(rsloop.run(main()), "accepted:socket")

    def test_start_tls_upgrades_existing_transport(self) -> None:
        async def main() -> str:
            loop = asyncio.get_running_loop()
            server_upgraded = asyncio.Event()

            class ServerProtocol(asyncio.Protocol):
                def __init__(self) -> None:
                    self.upgraded: asyncio.Future[None] = loop.create_future()

                def connection_made(self, transport: asyncio.BaseTransport) -> None:
                    self.transport = transport
                    if not isinstance(
                        transport.get_extra_info("sslcontext"), type(None)
                    ):
                        if not self.upgraded.done():
                            self.upgraded.set_result(None)
                            server_upgraded.set()

                async def upgrade(self, ssl_context) -> None:
                    self.transport = await loop.start_tls(
                        self.transport,
                        self,
                        ssl_context,
                        server_side=True,
                    )

                def data_received(self, data: bytes) -> None:
                    self.transport.write(b"upgraded:" + data)
                    self.transport.close()

            class ClientProtocol(asyncio.Protocol):
                def __init__(self) -> None:
                    self.done: asyncio.Future[str] = loop.create_future()

                def connection_made(self, transport: asyncio.BaseTransport) -> None:
                    self.transport = transport

                def data_received(self, data: bytes) -> None:
                    if not self.done.done():
                        self.done.set_result(data.decode())

                async def upgrade(self, ssl_context) -> None:
                    self.transport = await loop.start_tls(
                        self.transport,
                        self,
                        ssl_context,
                        server_hostname="localhost",
                    )
                    await asyncio.wait_for(server_upgraded.wait(), 5.0)
                    self.transport.write(b"starttls")

            with tempfile.TemporaryDirectory() as tmpdir:
                server_ctx, client_ctx = make_ssl_contexts(tmpdir)
                server_protocols: list[ServerProtocol] = []

                def server_factory() -> ServerProtocol:
                    protocol = ServerProtocol()
                    server_protocols.append(protocol)
                    return protocol

                server = await loop.create_server(server_factory, "127.0.0.1", 0)
                try:
                    port = server.sockets[0].getsockname()[1]
                    client_protocol = ClientProtocol()
                    await loop.create_connection(
                        lambda: client_protocol,
                        "127.0.0.1",
                        port,
                    )
                    while not server_protocols:
                        await asyncio.sleep(0.01)
                    await asyncio.gather(
                        server_protocols[0].upgrade(server_ctx),
                        client_protocol.upgrade(client_ctx),
                    )
                    return await asyncio.wait_for(client_protocol.done, 5.0)
                finally:
                    server.close()

        self.assertEqual(rsloop.run(main()), "upgraded:starttls")


if __name__ == "__main__":
    unittest.main()
