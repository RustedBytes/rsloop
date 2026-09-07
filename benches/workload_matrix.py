#!/usr/bin/env python3
from __future__ import annotations

import argparse
import asyncio
import base64
import hashlib
import json
import math
import os
import platform
import socket
import ssl
import statistics
import struct
import subprocess
import sys
import time
from collections.abc import Awaitable, Callable
from dataclasses import asdict, dataclass, field
from pathlib import Path

from compare_event_loops import (
    LOOP_CHOICES,
    default_loops_csv,
    format_bytes,
    get_peak_rss_bytes,
    is_loop_available,
    loop_factory_for,
    maybe_wait_closed,
    normalize_csv,
)
from idle_statistics import latency_comparison

SCENARIO_CHOICES = (
    "http_keepalive",
    "tls_http",
    "websocket_messages",
    "websocket_tls",
    "websockets_messages",
    "websockets_tls",
    "aiohttp_websocket_messages",
    "aiohttp_websocket_tls",
    "starlette_websocket_messages",
    "starlette_websocket_tls",
    "mixed_streams",
    "bulk_transfer",
    "idle_connections",
)
DEFAULT_SCENARIOS = ",".join(SCENARIO_CHOICES)
ROOT = Path(__file__).resolve().parent.parent
TLS_DIR = ROOT / "tests" / "fixtures" / "tls"
HTTP_REQUEST = (
    b"GET /resource HTTP/1.1\r\nHost: localhost\r\nConnection: keep-alive\r\n\r\n"
)
WEBSOCKET_KEY = "dGhlIHNhbXBsZSBub25jZQ=="
WEBSOCKET_REQUEST = (
    "GET /socket HTTP/1.1\r\n"
    "Host: localhost\r\n"
    "Upgrade: websocket\r\n"
    "Connection: Upgrade\r\n"
    f"Sec-WebSocket-Key: {WEBSOCKET_KEY}\r\n"
    "Sec-WebSocket-Version: 13\r\n\r\n"
).encode("ascii")
WEBSOCKET_ACCEPT = base64.b64encode(
    hashlib.sha1(
        (WEBSOCKET_KEY + "258EAFA5-E914-47DA-95CA-C5AB0DC85B11").encode("ascii")
    ).digest()
).decode("ascii")
WEBSOCKET_RESPONSE = (
    "HTTP/1.1 101 Switching Protocols\r\n"
    "Upgrade: websocket\r\n"
    "Connection: Upgrade\r\n"
    f"Sec-WebSocket-Accept: {WEBSOCKET_ACCEPT}\r\n\r\n"
).encode("ascii")


@dataclass(frozen=True)
class MatrixResult:
    loop: str
    scenario: str
    seconds: float
    operations: int
    bytes_transferred: int
    latency_ms: list[float]
    peak_rss_bytes: int = 0
    connection_setup_seconds: float = 0.0
    traffic_seconds: float = 0.0
    teardown_seconds: float = 0.0
    idle_residency_seconds: float = 0.0
    benchmark_version: int = 1
    idle_cycles: list[dict[str, float]] = field(default_factory=list)
    warmup_seconds: float = 0.0
    environment: dict[str, object] = field(default_factory=dict)

    @property
    def ops_per_sec(self) -> float:
        return self.operations / self.seconds if self.seconds else float("inf")

    @property
    def traffic_ops_per_sec(self) -> float:
        denominator = self.traffic_seconds or self.seconds
        return self.operations / denominator if denominator else float("inf")

    @property
    def mib_per_sec(self) -> float:
        return (
            self.bytes_transferred / (1024 * 1024) / self.seconds
            if self.seconds
            else 0.0
        )

    @property
    def traffic_mib_per_sec(self) -> float:
        denominator = self.traffic_seconds or self.seconds
        return (
            self.bytes_transferred / (1024 * 1024) / denominator if denominator else 0.0
        )


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Run production-shaped networking workloads against asyncio event loops.",
    )
    parser.add_argument(
        "--loops",
        default=default_loops_csv(),
        help=(
            "Comma-separated loops. Defaults to asyncio, uvloop, and rsloop on "
            "Unix; asyncio, winloop, and rsloop on Windows. "
            "Python 3.14+ also includes zuvloop."
        ),
    )
    parser.add_argument("--scenarios", default=DEFAULT_SCENARIOS)
    parser.add_argument("--warmups", type=int, default=1)
    parser.add_argument("--repeat", type=int, default=3)
    parser.add_argument(
        "--sustained",
        action="store_true",
        help=(
            "Use at least 2 warmups, 7 measured runs, and 500 operations per "
            "connection so short loopback workloads expose steady-state behavior."
        ),
    )
    parser.add_argument(
        "--measurement-mode",
        choices=("warm", "cold"),
        default="warm",
        help=(
            "Run warmups and measurements in one child process (warm, default), "
            "or isolate every run in a fresh process (cold)."
        ),
    )
    parser.add_argument("--concurrency", type=int, default=16)
    parser.add_argument("--requests-per-connection", type=int, default=50)
    parser.add_argument("--http-response-size", type=int, default=4096)
    parser.add_argument(
        "--app-work-iterations",
        type=int,
        default=8,
        help="SHA-256 iterations per HTTP request to model modest application work.",
    )
    parser.add_argument(
        "--mixed-payload-sizes",
        default="64,1024,16384,65536",
        help="Comma-separated payload sizes cycled by mixed_streams clients.",
    )
    parser.add_argument(
        "--websocket-payload-sizes",
        default="32,256,4096",
        help="Comma-separated binary frame sizes cycled by WebSocket clients.",
    )
    parser.add_argument("--bulk-bytes", type=int, default=2 * 1024 * 1024)
    parser.add_argument("--bulk-chunk-size", type=int, default=64 * 1024)
    parser.add_argument("--idle-connections", type=int, default=200)
    parser.add_argument("--idle-seconds", type=float, default=0.2)
    parser.add_argument("--idle-cycles", type=int, default=100)
    parser.add_argument("--idle-warmup-cycles", type=int, default=5)
    parser.add_argument(
        "--idle-timeout",
        type=float,
        default=30.0,
        help="Timeout in seconds for connection setup or one activation burst.",
    )
    parser.add_argument(
        "--cpu-affinity",
        help="Optional comma-separated CPU IDs (where supported); inherited by child threads.",
    )
    parser.add_argument("--tls-dir", type=Path, default=TLS_DIR)
    parser.add_argument("--json-output", type=Path)
    parser.add_argument(
        "--profile-rsloop-dir",
        type=Path,
        help="Run one unmeasured Tracy pass per rsloop scenario before measurements.",
    )
    parser.add_argument(
        "--allow-profiler-build",
        action="store_true",
        help="Allow measured rsloop runs from a Tracy-enabled build.",
    )
    parser.add_argument("--child", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument("--child-runs", type=int, default=1, help=argparse.SUPPRESS)
    parser.add_argument("--loop", choices=LOOP_CHOICES, help=argparse.SUPPRESS)
    parser.add_argument("--scenario", choices=SCENARIO_CHOICES, help=argparse.SUPPRESS)
    parser.add_argument("--profile-label", help=argparse.SUPPRESS)
    return parser.parse_args()


def positive(value: int, name: str) -> None:
    if value <= 0:
        raise SystemExit(f"{name} must be > 0")


def validate_args(args: argparse.Namespace) -> None:
    if args.sustained:
        args.warmups = max(args.warmups, 2)
        args.repeat = max(args.repeat, 7)
        args.requests_per_connection = max(args.requests_per_connection, 500)
    if args.warmups < 0:
        raise SystemExit("--warmups must be >= 0")
    positive(args.repeat, "--repeat")
    positive(args.child_runs, "--child-runs")
    positive(args.concurrency, "--concurrency")
    positive(args.requests_per_connection, "--requests-per-connection")
    positive(args.http_response_size, "--http-response-size")
    if args.app_work_iterations < 0:
        raise SystemExit("--app-work-iterations must be >= 0")
    positive(args.bulk_bytes, "--bulk-bytes")
    positive(args.bulk_chunk_size, "--bulk-chunk-size")
    positive(args.idle_connections, "--idle-connections")
    positive(args.idle_cycles, "--idle-cycles")
    if args.idle_warmup_cycles < 0:
        raise SystemExit("--idle-warmup-cycles must be >= 0")
    if not math.isfinite(args.idle_timeout) or args.idle_timeout <= 0:
        raise SystemExit("--idle-timeout must be finite and > 0")
    if not math.isfinite(args.idle_seconds) or args.idle_seconds < 0:
        raise SystemExit("--idle-seconds must be >= 0")
    if args.cpu_affinity:
        if not hasattr(os, "sched_setaffinity"):
            raise SystemExit("--cpu-affinity is unsupported on this platform")
        try:
            cpus = {int(cpu) for cpu in args.cpu_affinity.split(",")}
            if not cpus or min(cpus) < 0:
                raise ValueError("CPU IDs must be nonnegative")
            os.sched_setaffinity(0, cpus)
        except (ValueError, OSError) as exc:
            raise SystemExit(f"invalid --cpu-affinity: {exc}") from exc
    for attribute, option in (
        ("mixed_payload_sizes", "--mixed-payload-sizes"),
        ("websocket_payload_sizes", "--websocket-payload-sizes"),
    ):
        try:
            sizes = [
                int(item.strip())
                for item in getattr(args, attribute).split(",")
                if item.strip()
            ]
        except ValueError as exc:
            raise SystemExit(f"{option} must contain integers") from exc
        if not sizes or any(size <= 0 for size in sizes):
            raise SystemExit(f"{option} values must be > 0")
        setattr(args, attribute, sizes)


async def close_writer(writer: asyncio.StreamWriter) -> None:
    writer.close()
    await maybe_wait_closed(writer)


async def close_server(server: asyncio.AbstractServer) -> None:
    server.close()
    await server.wait_closed()


def application_digest(iterations: int) -> bytes:
    digest = b"rsloop-real-world-workload"
    for _ in range(iterations):
        digest = hashlib.sha256(digest).digest()
    return digest


def ensure_idle_connection_capacity(connection_count: int) -> None:
    if os.name == "nt":
        return

    try:
        import resource
    except ImportError:
        return

    # The client and server live in the same process. Account for both steady-
    # state sockets and descriptors held transiently while the runtime registers
    # each endpoint, then leave room for the listener and interpreter runtime.
    required = connection_count * 4 + 64
    soft, hard = resource.getrlimit(resource.RLIMIT_NOFILE)
    if soft >= required:
        return
    if hard < required:
        raise RuntimeError(
            f"--idle-connections {connection_count} requires a file-descriptor "
            f"limit of at least {required}, but the hard limit is {hard}; lower "
            "--idle-connections or raise the hard limit"
        )
    try:
        resource.setrlimit(resource.RLIMIT_NOFILE, (required, hard))
    except (OSError, ValueError) as exc:
        raise RuntimeError(
            f"could not raise the file-descriptor soft limit from {soft} to "
            f"{required}; lower --idle-connections or run with `ulimit -n "
            f"{required}`"
        ) from exc


def tls_contexts(tls_dir: Path) -> tuple[ssl.SSLContext, ssl.SSLContext]:
    cert = tls_dir / "cert.pem"
    key = tls_dir / "key.pem"
    ca = tls_dir / "ca-cert.pem"
    missing = [path for path in (cert, key, ca) if not path.is_file()]
    if missing:
        names = ", ".join(str(path) for path in missing)
        raise RuntimeError(
            f"missing TLS fixtures: {names}. Generate them with: "
            "uv run --no-project python scripts/generate_test_tls_certs.py tests/fixtures/tls"
        )
    server_context = ssl.SSLContext(ssl.PROTOCOL_TLS_SERVER)
    server_context.load_cert_chain(cert, key)
    client_context = ssl.create_default_context(ssl.Purpose.SERVER_AUTH, cafile=ca)
    return server_context, client_context


async def run_http(
    loop_name: str,
    args: argparse.Namespace,
    *,
    use_tls: bool,
) -> MatrixResult:
    response_body = application_digest(args.app_work_iterations) or b"x"
    response_body = (
        response_body * ((args.http_response_size // len(response_body)) + 1)
    )[: args.http_response_size]
    response_head = (
        "HTTP/1.1 200 OK\r\n"
        f"Content-Length: {len(response_body)}\r\n"
        "Content-Type: application/octet-stream\r\n"
        "Connection: keep-alive\r\n\r\n"
    ).encode("ascii")

    async def handle(
        reader: asyncio.StreamReader, writer: asyncio.StreamWriter
    ) -> None:
        try:
            while True:
                try:
                    request = await reader.readexactly(len(HTTP_REQUEST))
                except asyncio.IncompleteReadError:
                    return
                if request != HTTP_REQUEST:
                    raise RuntimeError("unexpected HTTP request")
                application_digest(args.app_work_iterations)
                writer.write(response_head)
                writer.write(response_body)
                await writer.drain()
        finally:
            await close_writer(writer)

    server_ssl = client_ssl = None
    if use_tls:
        server_ssl, client_ssl = tls_contexts(args.tls_dir)
    server = await asyncio.start_server(handle, "127.0.0.1", 0, ssl=server_ssl)
    host, port = server.sockets[0].getsockname()[:2]
    latencies: list[float] = []

    async def open_client() -> tuple[asyncio.StreamReader, asyncio.StreamWriter]:
        connection = await asyncio.open_connection(
            host,
            port,
            ssl=client_ssl,
            server_hostname="localhost" if use_tls else None,
        )
        clients.append(connection)
        return connection

    async def client(reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> int:
        transferred = 0
        for _ in range(args.requests_per_connection):
            started = time.perf_counter()
            writer.write(HTTP_REQUEST)
            await writer.drain()
            await reader.readexactly(len(response_head))
            await reader.readexactly(len(response_body))
            latencies.append((time.perf_counter() - started) * 1000)
            transferred += len(HTTP_REQUEST) + len(response_head) + len(response_body)
        return transferred

    started = time.perf_counter()
    clients: list[tuple[asyncio.StreamReader, asyncio.StreamWriter]] = []
    setup_finished = traffic_finished = started
    try:
        clients = list(
            await asyncio.gather(*(open_client() for _ in range(args.concurrency)))
        )
        setup_finished = time.perf_counter()
        transferred = sum(await asyncio.gather(*(client(*pair) for pair in clients)))
        traffic_finished = time.perf_counter()
    finally:
        await asyncio.gather(
            *(close_writer(writer) for _, writer in clients),
            return_exceptions=True,
        )
        await close_server(server)
    finished = time.perf_counter()
    scenario = "tls_http" if use_tls else "http_keepalive"
    return MatrixResult(
        loop_name,
        scenario,
        finished - started,
        args.concurrency * args.requests_per_connection,
        transferred,
        latencies,
        connection_setup_seconds=setup_finished - started,
        traffic_seconds=traffic_finished - setup_finished,
        teardown_seconds=finished - traffic_finished,
    )


def websocket_frame(payload: bytes, mask_key: bytes | None = None) -> bytes:
    length = len(payload)
    masked = mask_key is not None
    if length < 126:
        header = bytes((0x82, length | (0x80 if masked else 0)))
    elif length <= 0xFFFF:
        header = bytes((0x82, 126 | (0x80 if masked else 0))) + struct.pack(
            "!H", length
        )
    else:
        header = bytes((0x82, 127 | (0x80 if masked else 0))) + struct.pack(
            "!Q", length
        )
    if mask_key is None:
        return header + payload
    masked_payload = bytes(
        value ^ mask_key[index % 4] for index, value in enumerate(payload)
    )
    return header + mask_key + masked_payload


async def read_websocket_frame(reader: asyncio.StreamReader) -> bytes:
    first, second = await reader.readexactly(2)
    if first != 0x82:
        raise RuntimeError(f"unexpected WebSocket frame type: {first:#x}")
    length = second & 0x7F
    if length == 126:
        length = struct.unpack("!H", await reader.readexactly(2))[0]
    elif length == 127:
        length = struct.unpack("!Q", await reader.readexactly(8))[0]
    mask_key = await reader.readexactly(4) if second & 0x80 else None
    payload = await reader.readexactly(length)
    if mask_key is not None:
        payload = bytes(
            value ^ mask_key[index % 4] for index, value in enumerate(payload)
        )
    return payload


async def run_websocket_messages(
    loop_name: str, args: argparse.Namespace, *, use_tls: bool = False
) -> MatrixResult:
    async def echo(reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        try:
            request = await reader.readexactly(len(WEBSOCKET_REQUEST))
            if request != WEBSOCKET_REQUEST:
                raise RuntimeError("unexpected WebSocket handshake")
            writer.write(WEBSOCKET_RESPONSE)
            await writer.drain()
            while True:
                try:
                    payload = await read_websocket_frame(reader)
                except asyncio.IncompleteReadError:
                    return
                writer.write(websocket_frame(payload))
                await writer.drain()
        finally:
            await close_writer(writer)

    server_ssl = client_ssl = None
    if use_tls:
        server_ssl, client_ssl = tls_contexts(args.tls_dir)
    server = await asyncio.start_server(echo, "127.0.0.1", 0, ssl=server_ssl)
    host, port = server.sockets[0].getsockname()[:2]
    latencies: list[float] = []

    connections: list[tuple[asyncio.StreamReader, asyncio.StreamWriter]] = []

    async def open_client(
        client_id: int,
    ) -> tuple[int, asyncio.StreamReader, asyncio.StreamWriter]:
        reader, writer = await asyncio.open_connection(
            host,
            port,
            ssl=client_ssl,
            server_hostname="localhost" if use_tls else None,
        )
        connections.append((reader, writer))
        writer.write(WEBSOCKET_REQUEST)
        await writer.drain()
        response = await reader.readexactly(len(WEBSOCKET_RESPONSE))
        if response != WEBSOCKET_RESPONSE:
            raise RuntimeError("WebSocket upgrade failed")
        return client_id, reader, writer

    async def client(
        client_id: int, reader: asyncio.StreamReader, writer: asyncio.StreamWriter
    ) -> int:
        transferred = len(WEBSOCKET_REQUEST) + len(WEBSOCKET_RESPONSE)
        for index in range(args.requests_per_connection):
            size = args.websocket_payload_sizes[
                (client_id + index) % len(args.websocket_payload_sizes)
            ]
            payload = bytes([(client_id + index) % 251]) * size
            mask_key = struct.pack("!I", (client_id << 16) ^ index ^ 0xA5A55A5A)
            outbound = websocket_frame(payload, mask_key)
            started = time.perf_counter()
            writer.write(outbound)
            await writer.drain()
            response_payload = await read_websocket_frame(reader)
            if response_payload != payload:
                raise RuntimeError("WebSocket echo mismatch")
            latencies.append((time.perf_counter() - started) * 1000)
            transferred += len(outbound) + len(websocket_frame(payload))
        return transferred

    started = time.perf_counter()
    setup_finished = traffic_finished = started
    try:
        opened = await asyncio.gather(
            *(open_client(index) for index in range(args.concurrency))
        )
        setup_finished = time.perf_counter()
        transferred = sum(await asyncio.gather(*(client(*item) for item in opened)))
        traffic_finished = time.perf_counter()
    finally:
        await asyncio.gather(
            *(close_writer(writer) for _, writer in connections),
            return_exceptions=True,
        )
        await close_server(server)
    finished = time.perf_counter()
    return MatrixResult(
        loop_name,
        "websocket_tls" if use_tls else "websocket_messages",
        finished - started,
        args.concurrency * args.requests_per_connection,
        transferred,
        latencies,
        connection_setup_seconds=setup_finished - started,
        traffic_seconds=traffic_finished - setup_finished,
        teardown_seconds=finished - traffic_finished,
    )


async def run_library_websocket_messages(
    loop_name: str,
    args: argparse.Namespace,
    *,
    library: str,
    use_tls: bool,
) -> MatrixResult:
    from websockets.asyncio.client import connect

    server_ssl = client_ssl = None
    if use_tls:
        server_ssl, client_ssl = tls_contexts(args.tls_dir)

    if library == "websockets":
        from websockets.asyncio.server import serve

        async def echo(websocket: object) -> None:
            async for message in websocket:
                await websocket.send(message)

        server = await serve(
            echo,
            "127.0.0.1",
            0,
            ssl=server_ssl,
            compression=None,
            ping_interval=None,
        )
        host, port = server.sockets[0].getsockname()[:2]

        async def stop_server() -> None:
            server.close()
            await server.wait_closed()

    elif library == "aiohttp":
        from aiohttp import WSMsgType, web

        async def echo(request: object) -> object:
            websocket = web.WebSocketResponse(compress=False)
            await websocket.prepare(request)
            async for message in websocket:
                if message.type == WSMsgType.BINARY:
                    await websocket.send_bytes(message.data)
                elif message.type == WSMsgType.TEXT:
                    await websocket.send_str(message.data)
            return websocket

        app = web.Application()
        app.router.add_get("/socket", echo)
        runner = web.AppRunner(app, access_log=None)
        await runner.setup()
        site = web.TCPSite(runner, "127.0.0.1", 0, ssl_context=server_ssl)
        await site.start()
        sockets = site._server.sockets
        host, port = sockets[0].getsockname()[:2]

        async def stop_server() -> None:
            await runner.cleanup()

    elif library == "starlette":
        import uvicorn
        from starlette.applications import Starlette
        from starlette.routing import WebSocketRoute
        from starlette.websockets import WebSocketDisconnect

        async def echo(websocket: object) -> None:
            await websocket.accept()
            try:
                while True:
                    await websocket.send_bytes(await websocket.receive_bytes())
            except WebSocketDisconnect:
                pass

        app = Starlette(routes=[WebSocketRoute("/socket", echo)])
        listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        listener.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
        listener.bind(("127.0.0.1", 0))
        listener.listen(2048)
        listener.setblocking(False)
        host, port = listener.getsockname()[:2]
        config = uvicorn.Config(
            app,
            loop="none",
            lifespan="off",
            ws="websockets",
            ws_per_message_deflate=False,
            ws_ping_interval=None,
            log_config=None,
            access_log=False,
            ssl_certfile=str(args.tls_dir / "cert.pem") if use_tls else None,
            ssl_keyfile=str(args.tls_dir / "key.pem") if use_tls else None,
        )
        uvicorn_server = uvicorn.Server(config)
        server_task = asyncio.create_task(uvicorn_server.serve(sockets=[listener]))
        while not uvicorn_server.started:
            if server_task.done():
                await server_task
                raise RuntimeError("Starlette ASGI server stopped during startup")
            await asyncio.sleep(0)

        async def stop_server() -> None:
            uvicorn_server.should_exit = True
            await server_task
            listener.close()

    else:
        raise ValueError(f"unknown WebSocket library: {library}")

    scheme = "wss" if use_tls else "ws"
    uri = f"{scheme}://{host}:{port}/socket"
    connections: list[object] = []
    latencies: list[float] = []

    async def open_client() -> object:
        websocket = await connect(
            uri,
            ssl=client_ssl,
            compression=None,
            ping_interval=None,
        )
        connections.append(websocket)
        return websocket

    async def client(client_id: int, websocket: object) -> int:
        transferred = 0
        for index in range(args.requests_per_connection):
            size = args.websocket_payload_sizes[
                (client_id + index) % len(args.websocket_payload_sizes)
            ]
            payload = bytes([(client_id + index) % 251]) * size
            started = time.perf_counter()
            await websocket.send(payload)
            response = await websocket.recv()
            if response != payload:
                raise RuntimeError("WebSocket echo mismatch")
            latencies.append((time.perf_counter() - started) * 1000)
            transferred += size * 2
        return transferred

    started = time.perf_counter()
    setup_finished = traffic_finished = started
    try:
        opened = await asyncio.gather(*(open_client() for _ in range(args.concurrency)))
        setup_finished = time.perf_counter()
        transferred = sum(
            await asyncio.gather(
                *(client(index, websocket) for index, websocket in enumerate(opened))
            )
        )
        traffic_finished = time.perf_counter()
    finally:
        await asyncio.gather(
            *(websocket.close() for websocket in connections),
            return_exceptions=True,
        )
        await stop_server()
    finished = time.perf_counter()
    scenario = f"{library}_{'tls' if use_tls else 'messages'}"
    if library == "aiohttp":
        scenario = f"aiohttp_websocket_{'tls' if use_tls else 'messages'}"
    elif library == "starlette":
        scenario = f"starlette_websocket_{'tls' if use_tls else 'messages'}"
    return MatrixResult(
        loop_name,
        scenario,
        finished - started,
        args.concurrency * args.requests_per_connection,
        transferred,
        latencies,
        connection_setup_seconds=setup_finished - started,
        traffic_seconds=traffic_finished - setup_finished,
        teardown_seconds=finished - traffic_finished,
    )


async def run_mixed_streams(loop_name: str, args: argparse.Namespace) -> MatrixResult:
    async def echo(reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        try:
            while data := await reader.read(64 * 1024):
                writer.write(data)
                await writer.drain()
        finally:
            await close_writer(writer)

    server = await asyncio.start_server(echo, "127.0.0.1", 0)
    host, port = server.sockets[0].getsockname()[:2]
    latencies: list[float] = []

    connections: list[tuple[asyncio.StreamReader, asyncio.StreamWriter]] = []

    async def open_client(
        client_id: int,
    ) -> tuple[int, asyncio.StreamReader, asyncio.StreamWriter]:
        reader, writer = await asyncio.open_connection(host, port)
        connections.append((reader, writer))
        return client_id, reader, writer

    async def client(
        client_id: int, reader: asyncio.StreamReader, writer: asyncio.StreamWriter
    ) -> int:
        transferred = 0
        for index in range(args.requests_per_connection):
            size = args.mixed_payload_sizes[
                (client_id + index) % len(args.mixed_payload_sizes)
            ]
            payload = bytes([(client_id + index) % 251]) * size
            started = time.perf_counter()
            writer.write(payload)
            await writer.drain()
            response = await reader.readexactly(size)
            if response != payload:
                raise RuntimeError("mixed stream echo mismatch")
            latencies.append((time.perf_counter() - started) * 1000)
            transferred += size * 2
        return transferred

    started = time.perf_counter()
    setup_finished = traffic_finished = started
    try:
        opened = await asyncio.gather(
            *(open_client(index) for index in range(args.concurrency))
        )
        setup_finished = time.perf_counter()
        transferred = sum(await asyncio.gather(*(client(*item) for item in opened)))
        traffic_finished = time.perf_counter()
    finally:
        await asyncio.gather(
            *(close_writer(writer) for _, writer in connections),
            return_exceptions=True,
        )
        await close_server(server)
    finished = time.perf_counter()
    return MatrixResult(
        loop_name,
        "mixed_streams",
        finished - started,
        args.concurrency * args.requests_per_connection,
        transferred,
        latencies,
        connection_setup_seconds=setup_finished - started,
        traffic_seconds=traffic_finished - setup_finished,
        teardown_seconds=finished - traffic_finished,
    )


async def run_bulk_transfer(loop_name: str, args: argparse.Namespace) -> MatrixResult:
    chunk = b"b" * min(args.bulk_chunk_size, args.bulk_bytes)

    async def send_bulk(
        reader: asyncio.StreamReader, writer: asyncio.StreamWriter
    ) -> None:
        try:
            await reader.readexactly(1)
            remaining = args.bulk_bytes
            while remaining:
                data = chunk[:remaining]
                writer.write(data)
                await writer.drain()
                remaining -= len(data)
        finally:
            await close_writer(writer)

    server = await asyncio.start_server(send_bulk, "127.0.0.1", 0)
    host, port = server.sockets[0].getsockname()[:2]
    latencies: list[float] = []

    connections: list[tuple[asyncio.StreamReader, asyncio.StreamWriter]] = []

    async def open_client() -> tuple[asyncio.StreamReader, asyncio.StreamWriter]:
        reader, writer = await asyncio.open_connection(host, port)
        connections.append((reader, writer))
        return reader, writer

    async def client(reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> int:
        started = time.perf_counter()
        writer.write(b"!")
        await writer.drain()
        await reader.readexactly(args.bulk_bytes)
        latencies.append((time.perf_counter() - started) * 1000)
        return args.bulk_bytes + 1

    started = time.perf_counter()
    setup_finished = traffic_finished = started
    try:
        opened = await asyncio.gather(*(open_client() for _ in range(args.concurrency)))
        setup_finished = time.perf_counter()
        transferred = sum(
            await asyncio.gather(*(client(*connection) for connection in opened))
        )
        traffic_finished = time.perf_counter()
    finally:
        await asyncio.gather(
            *(close_writer(writer) for _, writer in connections),
            return_exceptions=True,
        )
        await close_server(server)
    finished = time.perf_counter()
    return MatrixResult(
        loop_name,
        "bulk_transfer",
        finished - started,
        args.concurrency,
        transferred,
        latencies,
        connection_setup_seconds=setup_finished - started,
        traffic_seconds=traffic_finished - setup_finished,
        teardown_seconds=finished - traffic_finished,
    )


async def run_idle_connections(
    loop_name: str, args: argparse.Namespace
) -> MatrixResult:
    async def ping(reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
        try:
            while data := await reader.read(1):
                writer.write(data)
                await writer.drain()
        finally:
            await close_writer(writer)

    server = await asyncio.start_server(
        ping, "127.0.0.1", 0, backlog=max(100, args.idle_connections)
    )
    host, port = server.sockets[0].getsockname()[:2]
    connections: list[tuple[asyncio.StreamReader, asyncio.StreamWriter]] = []
    latencies: list[float] = []
    cycles: list[dict[str, float]] = []
    setup_seconds = warmup_seconds = idle_seconds = traffic_seconds = 0.0
    started = time.perf_counter()

    async def open_client() -> None:
        connections.append(await asyncio.open_connection(host, port))

    opening = [asyncio.create_task(open_client()) for _ in range(args.idle_connections)]
    try:
        await asyncio.wait_for(asyncio.gather(*opening), args.idle_timeout)
        setup_seconds = time.perf_counter() - started
        for index in range(args.idle_warmup_cycles + args.idle_cycles):
            cycle_started = time.perf_counter()
            await asyncio.sleep(args.idle_seconds)
            activated = time.perf_counter()
            replies: list[float] = []

            async def activate(
                connection: tuple[asyncio.StreamReader, asyncio.StreamWriter],
                origin: float,
                completions: list[float],
            ) -> None:
                reader, writer = connection
                writer.write(b"p")
                await writer.drain()
                if await reader.readexactly(1) != b"p":
                    raise RuntimeError("idle connection ping mismatch")
                # One origin includes scheduling delay before this client starts.
                completions.append((time.perf_counter() - origin) * 1000)

            await asyncio.wait_for(
                asyncio.gather(
                    *(
                        activate(connection, activated, replies)
                        for connection in connections
                    )
                ),
                args.idle_timeout,
            )
            finished = time.perf_counter()
            if index < args.idle_warmup_cycles:
                warmup_seconds += finished - cycle_started
                continue
            residency = activated - cycle_started
            duration = finished - activated
            cycles.append(
                {
                    "first_ms": min(replies),
                    "p50_ms": percentile(replies, 0.50),
                    "p95_ms": percentile(replies, 0.95),
                    "all_ms": max(replies),
                    "idle_seconds": residency,
                    "traffic_seconds": duration,
                }
            )
            latencies.extend(replies)
            idle_seconds += residency
            traffic_seconds += duration
    finally:
        teardown_started = time.perf_counter()
        for task in opening:
            if not task.done():
                task.cancel()
        await asyncio.gather(*opening, return_exceptions=True)
        await asyncio.gather(
            *(close_writer(writer) for _, writer in connections), return_exceptions=True
        )
        await close_server(server)
        teardown_seconds = time.perf_counter() - teardown_started

    operations = args.idle_connections * len(cycles)
    return MatrixResult(
        loop_name,
        "idle_connections",
        setup_seconds + traffic_seconds + teardown_seconds,
        operations,
        operations * 2,
        latencies,
        connection_setup_seconds=setup_seconds,
        traffic_seconds=traffic_seconds,
        teardown_seconds=teardown_seconds,
        idle_residency_seconds=idle_seconds,
        benchmark_version=2,
        idle_cycles=cycles,
        warmup_seconds=warmup_seconds,
    )


SCENARIO_RUNNERS: dict[
    str, Callable[[str, argparse.Namespace], Awaitable[MatrixResult]]
] = {
    "http_keepalive": lambda loop, args: run_http(loop, args, use_tls=False),
    "tls_http": lambda loop, args: run_http(loop, args, use_tls=True),
    "websocket_messages": run_websocket_messages,
    "websocket_tls": lambda loop, args: run_websocket_messages(
        loop, args, use_tls=True
    ),
    "websockets_messages": lambda loop, args: run_library_websocket_messages(
        loop, args, library="websockets", use_tls=False
    ),
    "websockets_tls": lambda loop, args: run_library_websocket_messages(
        loop, args, library="websockets", use_tls=True
    ),
    "aiohttp_websocket_messages": lambda loop, args: run_library_websocket_messages(
        loop, args, library="aiohttp", use_tls=False
    ),
    "aiohttp_websocket_tls": lambda loop, args: run_library_websocket_messages(
        loop, args, library="aiohttp", use_tls=True
    ),
    "starlette_websocket_messages": lambda loop, args: run_library_websocket_messages(
        loop, args, library="starlette", use_tls=False
    ),
    "starlette_websocket_tls": lambda loop, args: run_library_websocket_messages(
        loop, args, library="starlette", use_tls=True
    ),
    "mixed_streams": run_mixed_streams,
    "bulk_transfer": run_bulk_transfer,
    "idle_connections": run_idle_connections,
}


def run_with_loop(loop_name: str, awaitable: Awaitable[MatrixResult]) -> MatrixResult:
    factory = loop_factory_for(loop_name)
    if sys.version_info[:2] >= (3, 12):
        return asyncio.run(awaitable, loop_factory=factory)
    loop = factory()
    try:
        asyncio.set_event_loop(loop)
        return loop.run_until_complete(awaitable)
    finally:
        asyncio.set_event_loop(None)
        loop.close()


def child_main(args: argparse.Namespace) -> int:
    connection_count = (
        args.idle_connections
        if args.scenario == "idle_connections"
        else args.concurrency
    )
    ensure_idle_connection_capacity(connection_count)

    if args.profile_label:
        if args.loop != "rsloop":
            raise RuntimeError("Tracy profiling is only supported for rsloop")
        import rsloop

        if not rsloop.profiler_compiled():
            raise RuntimeError(
                "Tracy profiling was requested, but rsloop was built without profiler "
                "support; rebuild with `uv run --with maturin maturin develop "
                "--release --features profiler`"
            )
    elif args.loop == "rsloop":
        import rsloop

        if rsloop.profiler_compiled() and not args.allow_profiler_build:
            raise RuntimeError(
                "refusing to measure a Tracy-enabled rsloop build; rebuild without "
                "--features profiler or pass --allow-profiler-build explicitly"
            )

    results: list[MatrixResult] = []
    if args.profile_label:
        print(f"[profile] Tracy session label: {args.profile_label}", flush=True)
    for _ in range(args.child_runs):
        environment = {
            "platform": platform.platform(),
            "python": sys.version,
            "cpu_count": os.cpu_count(),
            "cpu_affinity": sorted(os.sched_getaffinity(0))
            if hasattr(os, "sched_getaffinity")
            else None,
            "load_average_start": os.getloadavg()
            if hasattr(os, "getloadavg")
            else None,
            "pid": os.getpid(),
        }
        if args.profile_label:
            with rsloop.profile():
                awaitable = SCENARIO_RUNNERS[args.scenario](args.loop, args)
                result = run_with_loop(args.loop, awaitable)
        else:
            awaitable = SCENARIO_RUNNERS[args.scenario](args.loop, args)
            result = run_with_loop(args.loop, awaitable)
        environment["load_average_end"] = (
            os.getloadavg() if hasattr(os, "getloadavg") else None
        )
        results.append(
            MatrixResult(
                **{
                    **asdict(result),
                    "peak_rss_bytes": get_peak_rss_bytes(),
                    "environment": environment,
                }
            )
        )
    payload: object = (
        asdict(results[0])
        if len(results) == 1
        else [asdict(result) for result in results]
    )
    print(json.dumps(payload))
    return 0


def child_command(
    args: argparse.Namespace,
    loop_name: str,
    scenario: str,
    profile_label: str | None = None,
    child_runs: int = 1,
) -> list[str]:
    cmd = [
        sys.executable,
        str(Path(__file__).resolve()),
        "--child",
        "--loop",
        loop_name,
        "--scenario",
        scenario,
        "--concurrency",
        str(args.concurrency),
        "--requests-per-connection",
        str(args.requests_per_connection),
        "--http-response-size",
        str(args.http_response_size),
        "--app-work-iterations",
        str(args.app_work_iterations),
        "--mixed-payload-sizes",
        ",".join(str(size) for size in args.mixed_payload_sizes),
        "--websocket-payload-sizes",
        ",".join(str(size) for size in args.websocket_payload_sizes),
        "--bulk-bytes",
        str(args.bulk_bytes),
        "--bulk-chunk-size",
        str(args.bulk_chunk_size),
        "--idle-connections",
        str(args.idle_connections),
        "--idle-seconds",
        str(args.idle_seconds),
        "--idle-cycles",
        str(args.idle_cycles),
        "--idle-warmup-cycles",
        str(args.idle_warmup_cycles),
        "--idle-timeout",
        str(args.idle_timeout),
        "--tls-dir",
        str(args.tls_dir),
        "--child-runs",
        str(child_runs),
    ]
    if profile_label:
        cmd.extend(("--profile-label", profile_label))
    if args.cpu_affinity:
        cmd.extend(("--cpu-affinity", args.cpu_affinity))
    if args.allow_profiler_build:
        cmd.append("--allow-profiler-build")
    return cmd


def run_child_batch(
    args: argparse.Namespace,
    loop_name: str,
    scenario: str,
    profile_label: str | None = None,
    child_runs: int = 1,
) -> list[MatrixResult]:
    env = os.environ.copy()
    if loop_name == "rsloop":
        env["RSLOOP_USE_FAST_STREAMS"] = "1"
    proc = subprocess.run(
        child_command(args, loop_name, scenario, profile_label, child_runs),
        cwd=ROOT,
        env=env,
        capture_output=True,
        text=True,
        check=False,
        timeout=max(
            300,
            child_runs
            * (
                (args.idle_cycles + args.idle_warmup_cycles)
                * (args.idle_seconds + args.idle_timeout)
                + args.idle_timeout
                + 60
            ),
        )
        if scenario == "idle_connections"
        else 300,
    )
    if proc.returncode:
        raise RuntimeError(
            f"{loop_name}/{scenario} failed with exit code {proc.returncode}\n"
            f"stdout:\n{proc.stdout}\nstderr:\n{proc.stderr}"
        )
    lines = [line for line in proc.stdout.splitlines() if line.strip()]
    if not lines:
        raise RuntimeError(f"{loop_name}/{scenario} produced no output")
    payload = json.loads(lines[-1])
    if isinstance(payload, dict):
        payload = [payload]
    return [MatrixResult(**item) for item in payload]


def run_child(
    args: argparse.Namespace,
    loop_name: str,
    scenario: str,
    profile_label: str | None = None,
) -> MatrixResult:
    return run_child_batch(args, loop_name, scenario, profile_label)[0]


def percentile(values: list[float], fraction: float) -> float:
    if not values:
        return 0.0
    ordered = sorted(values)
    index = max(0, min(len(ordered) - 1, int(len(ordered) * fraction + 0.999999) - 1))
    return ordered[index]


def summarize(scenario: str, runs: dict[str, list[MatrixResult]]) -> None:
    if scenario == "idle_connections":
        print(
            "\nidle_connections v2: shared-origin activation latency (lower is better)"
        )
        print(
            "loop         first_ms     50%_ms     95%_ms     all_ms   cycle95 min/p10/p50/p90/max ms"
        )
        for name, measured in runs.items():
            milestones = [
                statistics.median(
                    statistics.median(cycle[key] for cycle in run.idle_cycles)
                    for run in measured
                )
                for key in ("first_ms", "p50_ms", "p95_ms", "all_ms")
            ]
            cycle95 = [cycle["p95_ms"] for run in measured for cycle in run.idle_cycles]
            distribution = "/".join(
                f"{percentile(cycle95, q):.3f}" for q in (0, 0.1, 0.5, 0.9, 1)
            )
            print(
                f"{name:<10} "
                + " ".join(f"{value:>10.3f}" for value in milestones)
                + "   "
                + distribution
            )
        return
    rows = []
    for loop_name, measured in runs.items():
        median_seconds = statistics.median(item.seconds for item in measured)
        rows.append(
            (
                loop_name,
                median_seconds,
                statistics.median(item.ops_per_sec for item in measured),
                statistics.median(item.traffic_ops_per_sec for item in measured),
                statistics.median(item.mib_per_sec for item in measured),
                statistics.median(item.traffic_mib_per_sec for item in measured),
                statistics.median(
                    percentile(item.latency_ms, 0.50) for item in measured
                ),
                statistics.median(
                    percentile(item.latency_ms, 0.95) for item in measured
                ),
                statistics.median(
                    percentile(item.latency_ms, 0.99) for item in measured
                ),
                int(statistics.median(item.peak_rss_bytes for item in measured)),
                statistics.median(item.connection_setup_seconds for item in measured),
                statistics.median(item.traffic_seconds for item in measured),
                statistics.median(item.teardown_seconds for item in measured),
                statistics.median(item.idle_residency_seconds for item in measured),
            )
        )
    rows.sort(key=lambda row: row[1])
    print(f"\n{scenario}")
    print(
        f"{'loop':<10} {'median_s':>10} {'total_ops/s':>12} {'traffic_ops/s':>14} "
        f"{'MiB/s':>10} {'traffic MiB/s':>14} "
        f"{'p50_ms':>10} {'p95_ms':>10} {'p99_ms':>10} {'peak_rss':>12} "
        f"{'setup_s':>9} {'traffic_s':>10} {'close_s':>9} {'idle_s':>8}"
    )
    for row in rows:
        print(
            f"{row[0]:<10} {row[1]:>10.4f} {row[2]:>12,.0f} {row[3]:>14,.0f} "
            f"{row[4]:>10.1f} {row[5]:>14.1f} {row[6]:>10.3f} {row[7]:>10.3f} "
            f"{row[8]:>10.3f} {format_bytes(row[9]):>12} {row[10]:>9.4f} "
            f"{row[11]:>10.4f} {row[12]:>9.4f} {row[13]:>8.4f}"
        )


def parent_main(args: argparse.Namespace) -> int:
    loops = normalize_csv(args.loops, allowed=LOOP_CHOICES, label="loops")
    scenarios = normalize_csv(
        args.scenarios, allowed=SCENARIO_CHOICES, label="scenarios"
    )
    available = []
    for loop_name in loops:
        ok, reason = is_loop_available(loop_name)
        if ok:
            available.append(loop_name)
        else:
            print(f"Skipping {loop_name}: {reason}")
    if not available:
        raise SystemExit("no benchmarkable loops are available")

    if args.profile_rsloop_dir:
        if "rsloop" not in available:
            raise SystemExit("--profile-rsloop-dir requires rsloop in --loops")
        import rsloop

        if not rsloop.profiler_compiled():
            raise SystemExit(
                "--profile-rsloop-dir requires a Tracy-enabled rsloop build. Run:\n"
                "  uv run --with maturin maturin develop --release --features profiler"
            )
        if not args.allow_profiler_build:
            raise SystemExit(
                "this invocation profiles and then measures the same Tracy-enabled build; "
                "pass --allow-profiler-build to acknowledge that its measured results are "
                "not comparable to a normal release build"
            )

    output: list[dict[str, object]] = []
    for scenario in scenarios:
        if scenario == "idle_connections":
            # Every process has its own within-run warmup cycles. Outer warmups
            # and warm-process batching are deliberately not used for idle v2.
            print(
                f"Idle v2: {args.repeat} fresh-process blocks, {args.idle_cycles} cycles/run, "
                f"{args.idle_warmup_cycles} warmup cycles, {args.idle_seconds}s idle/cycle."
            )
            scenario_runs = {name: [] for name in available}
            orders = []
            if args.profile_rsloop_dir:
                args.profile_rsloop_dir.mkdir(parents=True, exist_ok=True)
                run_child(
                    args,
                    "rsloop",
                    scenario,
                    str(args.profile_rsloop_dir / "rsloop-idle_connections"),
                )
            for block in range(args.repeat):
                # AB/BA for two loops; rotate the first loop for larger sets.
                offset = block % len(available)
                order = available[offset:] + available[:offset]
                orders.append(order)
                for name in order:
                    print(
                        f"Running idle block {block + 1}/{args.repeat} on {name}...",
                        flush=True,
                    )
                    scenario_runs[name].append(run_child(args, name, scenario))
            summarize(scenario, scenario_runs)
            reference = "uvloop" if "uvloop" in available else available[0]
            for name, measured in scenario_runs.items():
                comparison = None
                if name != reference:
                    comparison = latency_comparison(
                        [
                            statistics.median(c["p95_ms"] for c in run.idle_cycles)
                            for run in scenario_runs[reference]
                        ],
                        [
                            statistics.median(c["p95_ms"] for c in run.idle_cycles)
                            for run in measured
                        ],
                    )
                    interval = comparison["ci95_percent"]
                    ci_text = (
                        f"95% CI [{interval[0]:+.1f}%, {interval[1]:+.1f}%]"
                        if interval
                        else "95% CI unavailable (<7 process runs)"
                    )
                    print(
                        f"{name} vs {reference}: {comparison['change_percent']:+.1f}% latency, "
                        f"{ci_text}, {comparison['classification']} "
                        "(5% threshold; geometric mean paired run ratio)"
                    )
                output.append(
                    {
                        "scenario": scenario,
                        "loop": name,
                        "measurement_mode": "paired-cold",
                        "benchmark_version": 2,
                        "settings": {
                            "connections": args.idle_connections,
                            "cycles": args.idle_cycles,
                            "warmup_cycles": args.idle_warmup_cycles,
                            "idle_seconds": args.idle_seconds,
                            "timeout_seconds": args.idle_timeout,
                            "cpu_affinity": args.cpu_affinity,
                        },
                        "block_orders": orders,
                        "comparison_reference": reference,
                        "idle_comparison": comparison,
                        "runs": [asdict(item) for item in measured],
                    }
                )
            continue
        scenario_runs: dict[str, list[MatrixResult]] = {}
        for loop_name in available:
            print(f"Running {scenario} on {loop_name}...")
            if args.profile_rsloop_dir and loop_name == "rsloop":
                args.profile_rsloop_dir.mkdir(parents=True, exist_ok=True)
                label = str(args.profile_rsloop_dir / f"rsloop-{scenario}")
                run_child(args, loop_name, scenario, label)
            if args.measurement_mode == "warm":
                batch = run_child_batch(
                    args,
                    loop_name,
                    scenario,
                    child_runs=args.warmups + args.repeat,
                )
                measured = batch[args.warmups :]
            else:
                for _ in range(args.warmups):
                    run_child(args, loop_name, scenario)
                measured = [
                    run_child(args, loop_name, scenario) for _ in range(args.repeat)
                ]
            scenario_runs[loop_name] = measured
            output.append(
                {
                    "scenario": scenario,
                    "loop": loop_name,
                    "measurement_mode": args.measurement_mode,
                    "runs": [asdict(item) for item in measured],
                }
            )
        summarize(scenario, scenario_runs)

    if args.json_output:
        args.json_output.parent.mkdir(parents=True, exist_ok=True)
        args.json_output.write_text(json.dumps(output, indent=2), encoding="utf-8")
        print(f"\nWrote raw results to {args.json_output}")
    return 0


def main() -> int:
    args = parse_args()
    validate_args(args)
    if args.child:
        return child_main(args)
    return parent_main(args)


if __name__ == "__main__":
    raise SystemExit(main())
