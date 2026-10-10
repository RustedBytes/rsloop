"""HTTPX/AnyIO and aiohttp against a real, event-controlled HTTP server."""

import asyncio
import socket
import ssl
from contextlib import asynccontextmanager
from pathlib import Path

import pytest

aiohttp = pytest.importorskip("aiohttp")
httpx = pytest.importorskip("httpx")
web = pytest.importorskip("aiohttp.web")

pytestmark = pytest.mark.ecosystem


def run(loop, coro):
    return loop.run_until_complete(asyncio.wait_for(coro, 20))


async def expect_prefix(chunks, expected):
    received = bytearray()
    while len(received) < len(expected):
        received.extend(await chunks.__anext__())
    assert received == expected


@asynccontextmanager
async def server(handler, tls=False):
    failures = []

    async def checked(request):
        try:
            return await handler(request)
        except Exception as error:
            failures.append(error)
            raise

    app = web.Application()
    app.router.add_route("*", "/{path:.*}", checked)
    runner = web.AppRunner(app, shutdown_timeout=2)
    listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    listener.bind(("127.0.0.1", 0))
    listener.listen()
    listener.setblocking(False)
    host, port = listener.getsockname()
    context = None
    verify = True
    if tls:
        fixtures = Path(__file__).parents[1] / "fixtures" / "tls"
        context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
        context.load_cert_chain(fixtures / "cert.pem", fixtures / "key.pem")
        verify = ssl.create_default_context(cafile=str(fixtures / "ca-cert.pem"))
    try:
        await runner.setup()
        await web.SockSite(runner, listener, ssl_context=context).start()
        yield f"{'https' if tls else 'http'}://{host}:{port}", verify
    finally:
        await asyncio.wait_for(runner.cleanup(), 5)
        listener.close()
        assert not failures, failures


@pytest.mark.parametrize("tls", [False, True], ids=["http", "https"])
@pytest.mark.parametrize("interruption", ["cancel", "read_timeout"])
def test_httpx_interrupted_download_releases_pool(loop, tls, interruption):
    async def main():
        release = asyncio.Event()
        connections = []

        async def handler(request):
            connections.append(request.transport)
            if request.path == "/ok":
                return web.json_response({"ok": True})
            response = web.StreamResponse()
            await response.prepare(request)
            await response.write(b"prefix")
            await release.wait()
            return response

        async with (
            server(handler, tls) as (url, verify),
            httpx.AsyncClient(
                verify=verify,
                trust_env=False,
                limits=httpx.Limits(max_connections=1),
                timeout=httpx.Timeout(5, read=1),
            ) as client,
        ):
            try:
                async with client.stream("GET", url + "/stream") as response:
                    assert response.status_code == 200
                    chunks = response.aiter_bytes()
                    await expect_prefix(chunks, b"prefix")
                    if interruption == "read_timeout":
                        with pytest.raises(httpx.ReadTimeout):
                            await chunks.__anext__()
                    else:
                        entered = asyncio.Event()

                        async def read_next():
                            entered.set()
                            return await chunks.__anext__()

                        task = asyncio.create_task(read_next())
                        await entered.wait()
                        task.cancel()
                        with pytest.raises(asyncio.CancelledError):
                            await task
                assert response.is_closed
                recovered = await client.get(url + "/ok")
                assert recovered.json() == {"ok": True}
                assert len(connections) == 2
                assert connections[0] is not connections[1]
            finally:
                release.set()

    run(loop, main())


def test_httpx_pool_timeout_then_keepalive_reuse(loop):
    async def main():
        finish = asyncio.Event()
        connections = []

        async def handler(request):
            connections.append(request.transport)
            if request.path == "/ok":
                return web.Response(body=b"ok")
            response = web.StreamResponse()
            await response.prepare(request)
            await response.write(b"first")
            await finish.wait()
            await response.write(b"last")
            return response

        async with (
            server(handler) as (url, _),
            httpx.AsyncClient(
                trust_env=False, limits=httpx.Limits(max_connections=1), timeout=5
            ) as client,
        ):
            try:
                async with client.stream("GET", url + "/hold") as response:
                    chunks = response.aiter_bytes()
                    await expect_prefix(chunks, b"first")
                    with pytest.raises(httpx.PoolTimeout):
                        await client.get(
                            url + "/ok", timeout=httpx.Timeout(5, pool=0.02)
                        )
                    assert len(connections) == 1
                    finish.set()
                    assert b"".join([chunk async for chunk in chunks]) == b"last"
                assert (await client.get(url + "/ok")).content == b"ok"
                assert connections[0] is connections[1]
            finally:
                finish.set()

    run(loop, main())


@pytest.mark.parametrize("interruption", ["cancel", "timeout"])
def test_aiohttp_queued_request_cancellation_preserves_pool(loop, interruption):
    async def main():
        finish = asyncio.Event()
        queued = asyncio.Event()
        connections = []

        async def handler(request):
            connections.append(request.transport)
            if request.path == "/ok":
                return web.Response(body=b"recovered")
            response = web.StreamResponse()
            await response.prepare(request)
            await response.write(b"first")
            await finish.wait()
            await response.write(b"last")
            return response

        async def on_queued(*_):
            queued.set()

        trace = aiohttp.TraceConfig()
        trace.on_connection_queued_start.append(on_queued)
        async with (
            server(handler) as (url, _),
            aiohttp.ClientSession(
                connector=aiohttp.TCPConnector(limit=1),
                trace_configs=[trace],
                timeout=aiohttp.ClientTimeout(total=5),
            ) as client,
        ):
            try:
                async with client.get(url + "/hold") as response:
                    assert await response.content.readexactly(5) == b"first"
                    task = asyncio.create_task(client.get(url + "/ok"))
                    await queued.wait()
                    assert not task.done()
                    if interruption == "cancel":
                        task.cancel()
                        with pytest.raises(asyncio.CancelledError):
                            await task
                    else:
                        with pytest.raises(asyncio.TimeoutError):
                            await asyncio.wait_for(task, 0)
                    assert len(connections) == 1
                    finish.set()
                    assert await response.read() == b"last"
                async with client.get(url + "/ok") as response:
                    assert await response.read() == b"recovered"
                assert connections[0] is connections[1]
            finally:
                finish.set()

    run(loop, main())


@pytest.mark.parametrize("client_kind", ["httpx", "aiohttp"])
def test_chunked_upload_and_response_preserve_bytes(loop, client_kind):
    async def main():
        seen = asyncio.Event()
        expected = "Привіт, async 🌍!".encode() * 4096

        async def handler(request):
            received = bytearray()
            async for chunk in request.content.iter_chunked(3071):
                received.extend(chunk)
                seen.set()
            assert received == expected
            response = web.StreamResponse()
            await response.prepare(request)
            for offset in range(0, len(received), 4093):
                await response.write(received[offset : offset + 4093])
            return response

        async def upload():
            yield expected[:13]
            # Require the server to consume a prefix before the next chunk.
            await seen.wait()
            for offset in range(13, len(expected), 1021):
                yield expected[offset : offset + 1021]

        async with server(handler) as (url, _):
            if client_kind == "httpx":
                async with (
                    httpx.AsyncClient(trust_env=False, timeout=5) as client,
                    client.stream("POST", url, content=upload()) as response,
                ):
                    assert response.status_code == 200
                    received = b"".join(
                        [chunk async for chunk in response.aiter_bytes()]
                    )
            else:
                async with (
                    aiohttp.ClientSession() as client,
                    client.post(url, data=upload()) as response,
                ):
                    assert response.status == 200
                    received = b"".join(
                        [chunk async for chunk in response.content.iter_any()]
                    )
            assert received == expected

    run(loop, main())


@pytest.mark.parametrize("client_kind", ["httpx", "aiohttp"])
def test_truncated_response_reports_error_and_client_recovers(loop, client_kind):
    async def main():
        handlers = []
        failures = []

        async def handler(reader, writer):
            handlers.append(asyncio.current_task())
            try:
                headers = await reader.readuntil(b"\r\n\r\n")
                body = b"bad" if headers.startswith(b"GET /truncated ") else b"ok"
                length = 10 if body == b"bad" else len(body)
                writer.write(
                    f"HTTP/1.1 200 OK\r\nContent-Length: {length}\r\n"
                    "Connection: close\r\n\r\n".encode()
                    + body
                )
                await writer.drain()
            except Exception as error:  # noqa: BLE001 - asserted by the parent test
                failures.append(error)
            finally:
                writer.close()
                await asyncio.wait_for(writer.wait_closed(), 5)

        listener = await asyncio.start_server(handler, "127.0.0.1", 0)
        url = f"http://127.0.0.1:{listener.sockets[0].getsockname()[1]}"
        try:
            if client_kind == "httpx":
                async with httpx.AsyncClient(trust_env=False, timeout=5) as client:
                    with pytest.raises(httpx.RemoteProtocolError):
                        await client.get(url + "/truncated")
                    assert (await client.get(url + "/ok")).content == b"ok"
            else:
                async with aiohttp.ClientSession(
                    timeout=aiohttp.ClientTimeout(total=5)
                ) as client:
                    with pytest.raises(aiohttp.ClientPayloadError):
                        async with client.get(url + "/truncated") as response:
                            await response.read()
                    async with client.get(url + "/ok") as response:
                        assert await response.read() == b"ok"
        finally:
            listener.close()
            await listener.wait_closed()
            if handlers:
                await asyncio.wait_for(asyncio.gather(*handlers), 5)
            assert not failures, failures

    run(loop, main())
