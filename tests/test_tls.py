from __future__ import annotations

import asyncio
import importlib.util
import inspect
import os
import pathlib
import shutil
import socket
import ssl
import subprocess
import sys
import tempfile
from typing import Any, cast

import pytest
import rsloop

TLS_FIXTURES_DIR = pathlib.Path(__file__).with_name("fixtures").joinpath("tls")
TLS_GENERATOR = (
    pathlib.Path(__file__)
    .resolve()
    .parents[1]
    .joinpath("scripts", "generate_test_tls_certs.py")
)
UV = shutil.which("uv") or "uv"


def ensure_tls_fixtures() -> None:
    if all(
        TLS_FIXTURES_DIR.joinpath(name).is_file()
        for name in ("ca-cert.pem", "cert.pem", "key.pem")
    ):
        return

    subprocess.run(
        [
            UV,
            "run",
            "--no-project",
            "--python",
            sys.executable,
            "python",
            str(TLS_GENERATOR),
            str(TLS_FIXTURES_DIR),
        ],
        check=True,
    )


ensure_tls_fixtures()


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


class TestTls:
    @pytest.mark.parametrize("method", ["write", "writelines"])
    @pytest.mark.parametrize("chunk_size", [1024, 16384, 147456])
    def test_tls_streams_use_native_objects_without_stdlib_fallback(
        self, monkeypatch, chunk_size, method
    ):
        from rsloop import _loop

        native = cast(Any, _loop)

        def forbidden(*args, **kwargs):
            raise AssertionError("TLS streams fell back to stdlib")

        monkeypatch.setattr(asyncio.streams, "open_connection", forbidden)
        monkeypatch.setattr(asyncio.streams, "start_server", forbidden)

        async def main():
            accepted = []
            payload = b"native TLS streams" * 8192

            async def echo(reader, writer):
                accepted.append((type(reader), type(writer)))
                try:
                    writer.write(await reader.readexactly(len(payload)))
                    await writer.drain()
                finally:
                    writer.close()
                    await writer.wait_closed()

            with tempfile.TemporaryDirectory() as tmpdir:
                server_ctx, client_ctx = make_ssl_contexts(tmpdir)
                server = await asyncio.create_task(
                    asyncio.start_server(echo, "127.0.0.1", 0, ssl=server_ctx, limit=32)
                )
                try:
                    reader, writer = await asyncio.create_task(
                        asyncio.open_connection(
                            "127.0.0.1",
                            server.sockets[0].getsockname()[1],
                            ssl=client_ctx,
                            server_hostname="localhost",
                            limit=32,
                        )
                    )
                    try:
                        assert type(reader) is native.PyFastStreamReader
                        assert type(writer) is native.PyFastStreamWriter
                        assert cast(Any, reader)._limit == 32
                        assert writer.get_extra_info("sslcontext") is client_ctx
                        assert not writer.can_write_eof()
                        pieces = [
                            payload[offset : offset + chunk_size]
                            for offset in range(0, len(payload), chunk_size)
                        ]
                        if method == "writelines":
                            writer.writelines(pieces)
                        else:
                            for piece in pieces:
                                writer.write(piece)
                        await writer.drain()
                        assert await reader.readexactly(len(payload)) == payload
                        assert await reader.read() == b""
                        await writer.wait_closed()
                        assert accepted == [
                            (native.PyFastStreamReader, native.PyFastStreamWriter)
                        ]
                    finally:
                        writer.close()
                        await writer.wait_closed()
                finally:
                    server.close()
                    await server.wait_closed()

        rsloop.run(asyncio.wait_for(main(), 10))

    def test_create_default_context_marks_default_verify_paths(self) -> None:
        context = ssl.create_default_context()
        assert context.__dict__.get("_rsloop_use_default_verify_paths")

    def test_create_default_context_with_explicit_ca_skips_default_paths(self) -> None:
        context = ssl.create_default_context(
            cafile=TLS_FIXTURES_DIR.joinpath("ca-cert.pem")
        )
        assert context.__dict__.get("_rsloop_use_default_verify_paths") is None

    def test_client_config_cache_reuses_and_invalidates(self) -> None:
        async def main() -> tuple[bool, bool]:
            async def echo(
                reader: asyncio.StreamReader, writer: asyncio.StreamWriter
            ) -> None:
                try:
                    writer.write(await reader.readexactly(1))
                    await writer.drain()
                finally:
                    writer.close()
                    await writer.wait_closed()

            with tempfile.TemporaryDirectory() as tmpdir:
                ca_cert_path, _, _ = make_cert_files(tmpdir)
                server_ctx, client_ctx = make_ssl_contexts(tmpdir)
                server = await asyncio.start_server(
                    echo, "127.0.0.1", 0, ssl=server_ctx
                )
                port = server.sockets[0].getsockname()[1]

                async def connect_once() -> None:
                    reader, writer = await asyncio.open_connection(
                        "127.0.0.1",
                        port,
                        ssl=client_ctx,
                        server_hostname="localhost",
                    )
                    writer.write(b"x")
                    await writer.drain()
                    assert await reader.readexactly(1) == b"x"
                    writer.close()
                    await writer.wait_closed()

                try:
                    await connect_once()
                    first = client_ctx.__dict__["_rsloop_client_config_cache"]
                    await connect_once()
                    reused = client_ctx.__dict__["_rsloop_client_config_cache"] is first

                    client_ctx.load_verify_locations(cafile=ca_cert_path)
                    await connect_once()
                    invalidated = (
                        client_ctx.__dict__["_rsloop_client_config_cache"] is not first
                    )
                    return reused, invalidated
                finally:
                    server.close()
                    await server.wait_closed()

        assert rsloop.run(main()) == (True, True)

    def test_server_close_cancels_pending_tls_handshake(self) -> None:
        async def main() -> None:
            async def handle(
                reader: asyncio.StreamReader, writer: asyncio.StreamWriter
            ) -> None:
                writer.close()
                await writer.wait_closed()

            with tempfile.TemporaryDirectory() as tmpdir:
                server_ctx, _ = make_ssl_contexts(tmpdir)
                server = await asyncio.start_server(
                    handle, "127.0.0.1", 0, ssl=server_ctx
                )
                port = server.sockets[0].getsockname()[1]
                plain_socket = socket.create_connection(("127.0.0.1", port))
                try:
                    await asyncio.sleep(0.05)
                    server.close()
                    await asyncio.wait_for(server.wait_closed(), 1.0)
                finally:
                    plain_socket.close()

        rsloop.run(main())

    def test_create_connection_and_server_tls_round_trip(self) -> None:
        async def main() -> tuple[str, tuple[int, ...]]:
            loop = asyncio.get_running_loop()
            done: asyncio.Future[str] = loop.create_future()
            result = ""
            server_fds: tuple[int, ...] = ()

            class ServerProtocol(asyncio.Protocol):
                def connection_made(self, transport: asyncio.BaseTransport) -> None:
                    transport = cast(asyncio.Transport, transport)
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
                    transport = cast(asyncio.Transport, transport)
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
                    assert (
                        await asyncio.wait_for(client_protocol.result, 5.0) == "TLS-OK"
                    )
                    assert await asyncio.wait_for(done, 5.0) == "server-closed"
                    result = "ok"
                finally:
                    server.close()
                    await server.wait_closed()
                    server_fds = tuple(sock.fileno() for sock in server.sockets)

            return result, server_fds

        result, server_fds = rsloop.run(main())
        assert result == "ok"
        assert server_fds == (-1,)

    @pytest.mark.skipif(os.name == "nt", reason="Unix sockets are Unix-only")
    def test_create_unix_connection_and_server_tls_round_trip(self) -> None:
        async def main() -> tuple[str, tuple[int, ...]]:
            loop = asyncio.get_running_loop()
            result = ""
            server_fds: tuple[int, ...] = ()

            class ServerProtocol(asyncio.Protocol):
                def connection_made(self, transport: asyncio.BaseTransport) -> None:
                    transport = cast(asyncio.Transport, transport)
                    self.transport = transport

                def data_received(self, data: bytes) -> None:
                    self.transport.write(b"unix:" + data)
                    self.transport.close()

            class ClientProtocol(asyncio.Protocol):
                def __init__(self) -> None:
                    self.parts: list[bytes] = []
                    self.done: asyncio.Future[str] = loop.create_future()

                def connection_made(self, transport: asyncio.BaseTransport) -> None:
                    transport = cast(asyncio.Transport, transport)
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
        assert result == "unix:tls"
        assert server_fds == (-1,)

    def test_connect_accepted_socket_tls_round_trip(self) -> None:
        async def main() -> str:
            loop = asyncio.get_running_loop()

            class AcceptedProtocol(asyncio.Protocol):
                def connection_made(self, transport: asyncio.BaseTransport) -> None:
                    transport = cast(asyncio.Transport, transport)
                    self.transport = transport

                def data_received(self, data: bytes) -> None:
                    self.transport.write(b"accepted:" + data)
                    self.transport.close()

            class ClientProtocol(asyncio.Protocol):
                def __init__(self) -> None:
                    self.parts: list[bytes] = []
                    self.done: asyncio.Future[str] = loop.create_future()

                def connection_made(self, transport: asyncio.BaseTransport) -> None:
                    transport = cast(asyncio.Transport, transport)
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

        assert rsloop.run(main()) == "accepted:socket"

    def test_start_tls_upgrades_existing_transport(self) -> None:
        async def main(*, client_first: bool) -> str:
            loop = asyncio.get_running_loop()
            server_upgraded = asyncio.Event()

            class ServerProtocol(asyncio.Protocol):
                def __init__(self) -> None:
                    self.upgraded: asyncio.Future[None] = loop.create_future()
                    self.connected = asyncio.Event()

                def connection_made(self, transport: asyncio.BaseTransport) -> None:
                    transport = cast(asyncio.Transport, transport)
                    self.transport = transport
                    self.connected.set()
                    if (
                        transport.get_extra_info("sslcontext") is not None
                        and not self.upgraded.done()
                    ):
                        self.upgraded.set_result(None)
                        server_upgraded.set()

                async def upgrade(self, ssl_context) -> None:
                    transport = await loop.start_tls(
                        self.transport,
                        self,
                        ssl_context,
                        server_side=True,
                    )
                    transport = cast(asyncio.Transport, transport)
                    self.transport = transport

                def data_received(self, data: bytes) -> None:
                    self.transport.write(b"upgraded:" + data)
                    self.transport.close()

            class ClientProtocol(asyncio.Protocol):
                def __init__(self) -> None:
                    self.done: asyncio.Future[str] = loop.create_future()

                def connection_made(self, transport: asyncio.BaseTransport) -> None:
                    transport = cast(asyncio.Transport, transport)
                    self.transport = transport

                def data_received(self, data: bytes) -> None:
                    if not self.done.done():
                        self.done.set_result(data.decode())

                async def upgrade(self, ssl_context) -> None:
                    transport = await loop.start_tls(
                        self.transport,
                        self,
                        ssl_context,
                        server_hostname="localhost",
                    )
                    transport = cast(asyncio.Transport, transport)
                    self.transport = transport
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
                    await asyncio.wait_for(server_protocols[0].connected.wait(), 5.0)
                    server_upgrade = server_protocols[0].upgrade(server_ctx)
                    client_upgrade = client_protocol.upgrade(client_ctx)
                    upgrades = (
                        (client_upgrade, server_upgrade)
                        if client_first
                        else (server_upgrade, client_upgrade)
                    )
                    await asyncio.wait_for(asyncio.gather(*upgrades), 15.0)
                    return await asyncio.wait_for(client_protocol.done, 5.0)
                finally:
                    server.close()

        # Both scheduling orders must retire plaintext readers before either
        # side can put handshake bytes on the socket.
        assert rsloop.run(main(client_first=False)) == "upgraded:starttls"
        assert rsloop.run(main(client_first=True)) == "upgraded:starttls"

    @pytest.mark.skipif(
        importlib.util.find_spec("websockets") is None,
        reason="websockets package is required",
    )
    def test_wsbench_websockets_respects_cert_none_context(self) -> None:
        from websockets import serve

        from examples import wsbench_websockets

        async def main() -> list[tuple[str, str]]:
            async def echo(websocket) -> None:
                async for message in websocket:
                    await websocket.send(message.upper())

            with tempfile.TemporaryDirectory() as tmpdir:
                _, cert_path, key_path = make_cert_files(tmpdir)
                server_ctx = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
                server_ctx.load_cert_chain(cert_path, key_path)

                client_ctx = ssl.create_default_context(ssl.Purpose.SERVER_AUTH)
                client_ctx.check_hostname = False
                client_ctx.hostname_checks_common_name = False
                client_ctx.verify_mode = ssl.CERT_NONE

                async with serve(
                    echo,
                    "127.0.0.1",
                    0,
                    ssl=server_ctx,
                    compression=None,
                    ping_interval=None,
                ) as server:
                    port = server.sockets[0].getsockname()[1]
                    return await wsbench_websockets.run_messages(
                        f"wss://127.0.0.1:{port}/",
                        ssl_context=client_ctx,
                        count=2,
                    )

        assert rsloop.run(main()) == [("hello 0", "HELLO 0"), ("hello 1", "HELLO 1")]


@pytest.mark.parametrize("backend", ["asyncio", "rsloop", "uvloop"])
@pytest.mark.parametrize("client_first", [False, True])
def test_stream_writer_start_tls_round_trip(tmp_path, backend, client_first):
    if backend != "rsloop" and not hasattr(asyncio.StreamWriter, "start_tls"):
        pytest.skip("stdlib StreamWriter.start_tls requires Python 3.11+")
    timeouts = {"ssl_handshake_timeout": 3}
    if backend == "rsloop" or "ssl_shutdown_timeout" in inspect.signature(
        asyncio.StreamWriter.start_tls
    ).parameters:
        timeouts["ssl_shutdown_timeout"] = 1
    factory = (
        pytest.importorskip("uvloop").new_event_loop
        if backend == "uvloop"
        else (asyncio if backend == "asyncio" else rsloop).new_event_loop
    )
    server_ctx, client_ctx = make_ssl_contexts(str(tmp_path))

    async def main():
        loop = asyncio.get_running_loop()
        server_go = asyncio.Event()
        done = loop.create_future()
        callbacks = 0

        async def serve(reader, writer):
            nonlocal callbacks
            callbacks += 1
            try:
                assert await reader.readline() == b"STARTTLS\n"
                writer.write(b"READY\n")
                await writer.drain()
                await server_go.wait()
                old_transport = writer.transport
                assert (
                    await writer.start_tls(
                        server_ctx, **timeouts
                    )
                    is None
                )
                assert writer.transport is not old_transport
                assert cast(Any, reader)._transport is writer.transport
                assert writer.get_extra_info("sslcontext") is server_ctx
                assert not writer.can_write_eof()
                assert await reader.readexactly(6) == b"secret"
                writer.write(b"encrypted reply")
                await writer.drain()
            except BaseException as exc:  # noqa: BLE001 - forward callback failure to test
                if not done.done():
                    done.set_exception(exc)
            finally:
                writer.close()
                await writer.wait_closed()
                if not done.done():
                    done.set_result(None)

        server = await asyncio.start_server(serve, "127.0.0.1", 0)
        try:
            reader, writer = await asyncio.open_connection(
                "127.0.0.1", server.sockets[0].getsockname()[1]
            )
            try:
                writer.write(b"STARTTLS\n")
                await writer.drain()
                assert await reader.readline() == b"READY\n"
                old_transport = writer.transport
                if not client_first:
                    server_go.set()
                    await asyncio.sleep(0)
                upgrade = asyncio.create_task(
                    writer.start_tls(
                        client_ctx,
                        server_hostname="localhost",
                        **timeouts,
                    )
                )
                if client_first:
                    await asyncio.sleep(0)
                    server_go.set()
                assert await upgrade is None
                assert writer.transport is not old_transport
                assert cast(Any, reader)._transport is writer.transport
                assert writer.get_extra_info("sslcontext") is client_ctx
                assert not writer.can_write_eof()
                writer.write(b"secret")
                await writer.drain()
                assert await reader.readexactly(15) == b"encrypted reply"
                assert await reader.read(1) == b""
                assert callbacks == 1
            finally:
                server_go.set()
                writer.close()
                await writer.wait_closed()
                await asyncio.wait_for(done, 3)
        finally:
            server.close()
            await server.wait_closed()

    loop = factory()
    try:
        loop.run_until_complete(asyncio.wait_for(main(), 10))
    finally:
        loop.run_until_complete(loop.shutdown_asyncgens())
        loop.close()


@pytest.mark.parametrize("cancelled", [False, True])
def test_native_start_tls_handler_error_closes_upgraded_transport(tmp_path, cancelled):
    server_ctx, client_ctx = make_ssl_contexts(str(tmp_path))

    async def main():
        loop = asyncio.get_running_loop()
        errors = []
        loop.set_exception_handler(lambda loop, context: errors.append(context))
        callbacks = 0

        async def serve(reader, writer):
            nonlocal callbacks
            callbacks += 1
            await writer.start_tls(server_ctx, ssl_handshake_timeout=3)
            assert await reader.readexactly(1) == b"!"
            if cancelled:
                raise asyncio.CancelledError
            raise RuntimeError("handler failed after upgrade")

        server = await asyncio.start_server(serve, "127.0.0.1", 0)
        try:
            reader, writer = await asyncio.open_connection(
                "127.0.0.1", server.sockets[0].getsockname()[1]
            )
            try:
                await writer.start_tls(
                    client_ctx, server_hostname="localhost", ssl_handshake_timeout=3
                )
                writer.write(b"!")
                await writer.drain()
                assert await reader.read(1) == b""
                assert callbacks == 1
                if cancelled:
                    assert not errors
                else:
                    assert len(errors) == 1
                    assert str(errors[0]["exception"]) == "handler failed after upgrade"
                    assert (
                        errors[0]["transport"].get_extra_info("sslcontext")
                        is server_ctx
                    )
            finally:
                writer.close()
                await writer.wait_closed()
        finally:
            server.close()
            await server.wait_closed()

    rsloop.run(asyncio.wait_for(main(), 10))


def test_start_tls_handshake_failure_releases_server(tmp_path):
    server_ctx, _ = make_ssl_contexts(str(tmp_path))

    async def main():
        loop = asyncio.get_running_loop()
        done = loop.create_future()

        async def serve(reader, writer):
            try:
                with pytest.raises(RuntimeError) as caught:
                    await writer.start_tls(server_ctx, ssl_handshake_timeout=1)
                assert writer.is_closing()
                with pytest.raises(RuntimeError):
                    await writer.wait_closed()
                assert reader.exception() is caught.value
            except BaseException as exc:  # noqa: BLE001 - forward callback failure
                done.set_exception(exc)
            else:
                done.set_result(None)

        server = await asyncio.start_server(serve, "127.0.0.1", 0)
        try:
            reader, writer = await asyncio.open_connection(
                "127.0.0.1", server.sockets[0].getsockname()[1]
            )
            try:
                writer.write(b"not a TLS client")
                await writer.drain()
                await done
                assert await reader.read() == b""
            finally:
                writer.close()
                await writer.wait_closed()
        finally:
            server.close()
            await server.wait_closed()

    rsloop.run(asyncio.wait_for(main(), 5))


def test_start_tls_cancellation_finishes_wait_closed(tmp_path, monkeypatch):
    _, client_ctx = make_ssl_contexts(str(tmp_path))
    upgrades = []
    original = cast(Any, rsloop.Loop).start_tls

    def capture_upgrade(self, *args, **kwargs):
        future = original(self, *args, **kwargs)
        upgrades.append(future)
        return future

    monkeypatch.setattr(rsloop.Loop, "start_tls", capture_upgrade)

    async def main():
        loop = asyncio.get_running_loop()
        release = asyncio.Event()
        done = loop.create_future()
        errors = []
        loop.set_exception_handler(lambda loop, context: errors.append(context))

        async def serve(reader, writer):
            await release.wait()
            writer.close()
            await writer.wait_closed()
            done.set_result(None)

        server = await asyncio.start_server(serve, "127.0.0.1", 0)
        try:
            _, writer = await asyncio.open_connection(
                "127.0.0.1", server.sockets[0].getsockname()[1]
            )
            try:
                task = asyncio.create_task(
                    writer.start_tls(
                        client_ctx, server_hostname="localhost", ssl_handshake_timeout=1
                    )
                )
                await asyncio.sleep(0)
                assert len(upgrades) == 1
                task.cancel()
                with pytest.raises(asyncio.CancelledError):
                    await task
                assert writer.is_closing()
                await writer.wait_closed()
                release.set()
                await done
                with pytest.raises(RuntimeError):
                    await upgrades[0]
                await asyncio.sleep(0)
                assert not errors
            finally:
                release.set()
                writer.close()
                await writer.wait_closed()
        finally:
            server.close()
            await server.wait_closed()

    rsloop.run(asyncio.wait_for(main(), 5))
