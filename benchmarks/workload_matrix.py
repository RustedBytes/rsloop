#!/usr/bin/env python3
from __future__ import annotations

import argparse
import asyncio
import base64
import hashlib
import json
import os
import ssl
import statistics
import struct
import subprocess
import sys
import time
from dataclasses import asdict, dataclass
from pathlib import Path
from typing import Awaitable, Callable

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


SCENARIO_CHOICES = (
    "http_keepalive",
    "tls_http",
    "websocket_messages",
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

    @property
    def ops_per_sec(self) -> float:
        return self.operations / self.seconds if self.seconds else float("inf")

    @property
    def mib_per_sec(self) -> float:
        return (
            self.bytes_transferred / (1024 * 1024) / self.seconds
            if self.seconds
            else 0.0
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
            "Unix; asyncio, winloop, and rsloop on Windows."
        ),
    )
    parser.add_argument("--scenarios", default=DEFAULT_SCENARIOS)
    parser.add_argument("--warmups", type=int, default=1)
    parser.add_argument("--repeat", type=int, default=3)
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
    parser.add_argument("--tls-dir", type=Path, default=TLS_DIR)
    parser.add_argument("--json-output", type=Path)
    parser.add_argument(
        "--profile-rsloop-dir",
        type=Path,
        help="Run one unmeasured Tracy pass per rsloop scenario before measurements.",
    )
    parser.add_argument("--child", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument("--loop", choices=LOOP_CHOICES, help=argparse.SUPPRESS)
    parser.add_argument("--scenario", choices=SCENARIO_CHOICES, help=argparse.SUPPRESS)
    parser.add_argument("--profile-label", help=argparse.SUPPRESS)
    return parser.parse_args()


def positive(value: int, name: str) -> None:
    if value <= 0:
        raise SystemExit(f"{name} must be > 0")


def validate_args(args: argparse.Namespace) -> None:
    if args.warmups < 0:
        raise SystemExit("--warmups must be >= 0")
    positive(args.repeat, "--repeat")
    positive(args.concurrency, "--concurrency")
    positive(args.requests_per_connection, "--requests-per-connection")
    positive(args.http_response_size, "--http-response-size")
    if args.app_work_iterations < 0:
        raise SystemExit("--app-work-iterations must be >= 0")
    positive(args.bulk_bytes, "--bulk-bytes")
    positive(args.bulk_chunk_size, "--bulk-chunk-size")
    positive(args.idle_connections, "--idle-connections")
    if args.idle_seconds < 0:
        raise SystemExit("--idle-seconds must be >= 0")
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

    async def client() -> int:
        reader, writer = await asyncio.open_connection(
            host,
            port,
            ssl=client_ssl,
            server_hostname="localhost" if use_tls else None,
        )
        transferred = 0
        try:
            for _ in range(args.requests_per_connection):
                started = time.perf_counter()
                writer.write(HTTP_REQUEST)
                await writer.drain()
                await reader.readexactly(len(response_head))
                await reader.readexactly(len(response_body))
                latencies.append((time.perf_counter() - started) * 1000)
                transferred += (
                    len(HTTP_REQUEST) + len(response_head) + len(response_body)
                )
        finally:
            await close_writer(writer)
        return transferred

    started = time.perf_counter()
    try:
        transferred = sum(
            await asyncio.gather(*(client() for _ in range(args.concurrency)))
        )
    finally:
        await close_server(server)
    scenario = "tls_http" if use_tls else "http_keepalive"
    return MatrixResult(
        loop_name,
        scenario,
        time.perf_counter() - started,
        args.concurrency * args.requests_per_connection,
        transferred,
        latencies,
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
    loop_name: str, args: argparse.Namespace
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

    server = await asyncio.start_server(echo, "127.0.0.1", 0)
    host, port = server.sockets[0].getsockname()[:2]
    latencies: list[float] = []

    async def client(client_id: int) -> int:
        reader, writer = await asyncio.open_connection(host, port)
        transferred = len(WEBSOCKET_REQUEST) + len(WEBSOCKET_RESPONSE)
        try:
            writer.write(WEBSOCKET_REQUEST)
            await writer.drain()
            response = await reader.readexactly(len(WEBSOCKET_RESPONSE))
            if response != WEBSOCKET_RESPONSE:
                raise RuntimeError("WebSocket upgrade failed")
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
        finally:
            await close_writer(writer)
        return transferred

    started = time.perf_counter()
    try:
        transferred = sum(
            await asyncio.gather(*(client(index) for index in range(args.concurrency)))
        )
    finally:
        await close_server(server)
    return MatrixResult(
        loop_name,
        "websocket_messages",
        time.perf_counter() - started,
        args.concurrency * args.requests_per_connection,
        transferred,
        latencies,
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

    async def client(client_id: int) -> int:
        reader, writer = await asyncio.open_connection(host, port)
        transferred = 0
        try:
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
        finally:
            await close_writer(writer)
        return transferred

    started = time.perf_counter()
    try:
        transferred = sum(
            await asyncio.gather(*(client(index) for index in range(args.concurrency)))
        )
    finally:
        await close_server(server)
    return MatrixResult(
        loop_name,
        "mixed_streams",
        time.perf_counter() - started,
        args.concurrency * args.requests_per_connection,
        transferred,
        latencies,
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

    async def client() -> int:
        reader, writer = await asyncio.open_connection(host, port)
        started = time.perf_counter()
        try:
            writer.write(b"!")
            await writer.drain()
            await reader.readexactly(args.bulk_bytes)
            latencies.append((time.perf_counter() - started) * 1000)
        finally:
            await close_writer(writer)
        return args.bulk_bytes + 1

    started = time.perf_counter()
    try:
        transferred = sum(
            await asyncio.gather(*(client() for _ in range(args.concurrency)))
        )
    finally:
        await close_server(server)
    return MatrixResult(
        loop_name,
        "bulk_transfer",
        time.perf_counter() - started,
        args.concurrency,
        transferred,
        latencies,
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
        ping,
        "127.0.0.1",
        0,
        backlog=max(100, args.idle_connections),
    )
    host, port = server.sockets[0].getsockname()[:2]
    connections = await asyncio.gather(
        *(asyncio.open_connection(host, port) for _ in range(args.idle_connections))
    )
    await asyncio.sleep(args.idle_seconds)
    latencies: list[float] = []

    async def activate(
        connection: tuple[asyncio.StreamReader, asyncio.StreamWriter],
    ) -> int:
        reader, writer = connection
        started = time.perf_counter()
        writer.write(b"p")
        await writer.drain()
        if await reader.readexactly(1) != b"p":
            raise RuntimeError("idle connection ping mismatch")
        latencies.append((time.perf_counter() - started) * 1000)
        return 2

    started = time.perf_counter()
    try:
        transferred = sum(
            await asyncio.gather(*(activate(item) for item in connections))
        )
    finally:
        await asyncio.gather(*(close_writer(writer) for _, writer in connections))
        await close_server(server)
    return MatrixResult(
        loop_name,
        "idle_connections",
        time.perf_counter() - started,
        args.idle_connections,
        transferred,
        latencies,
    )


SCENARIO_RUNNERS: dict[
    str, Callable[[str, argparse.Namespace], Awaitable[MatrixResult]]
] = {
    "http_keepalive": lambda loop, args: run_http(loop, args, use_tls=False),
    "tls_http": lambda loop, args: run_http(loop, args, use_tls=True),
    "websocket_messages": run_websocket_messages,
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
    awaitable = SCENARIO_RUNNERS[args.scenario](args.loop, args)
    if args.profile_label:
        if args.loop != "rsloop":
            raise RuntimeError("Tracy profiling is only supported for rsloop")
        print(f"[profile] Tracy session label: {args.profile_label}", flush=True)
        import rsloop

        with rsloop.profile():
            result = run_with_loop(args.loop, awaitable)
    else:
        result = run_with_loop(args.loop, awaitable)
    result = MatrixResult(**{**asdict(result), "peak_rss_bytes": get_peak_rss_bytes()})
    print(json.dumps(asdict(result)))
    return 0


def child_command(
    args: argparse.Namespace,
    loop_name: str,
    scenario: str,
    profile_label: str | None = None,
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
        "--tls-dir",
        str(args.tls_dir),
    ]
    if profile_label:
        cmd.extend(("--profile-label", profile_label))
    return cmd


def run_child(
    args: argparse.Namespace,
    loop_name: str,
    scenario: str,
    profile_label: str | None = None,
) -> MatrixResult:
    env = os.environ.copy()
    if loop_name == "rsloop":
        env["RSLOOP_USE_FAST_STREAMS"] = "1"
    proc = subprocess.run(
        child_command(args, loop_name, scenario, profile_label),
        cwd=ROOT,
        env=env,
        capture_output=True,
        text=True,
        check=False,
    )
    if proc.returncode:
        raise RuntimeError(
            f"{loop_name}/{scenario} failed with exit code {proc.returncode}\n"
            f"stdout:\n{proc.stdout}\nstderr:\n{proc.stderr}"
        )
    lines = [line for line in proc.stdout.splitlines() if line.strip()]
    if not lines:
        raise RuntimeError(f"{loop_name}/{scenario} produced no output")
    return MatrixResult(**json.loads(lines[-1]))


def percentile(values: list[float], fraction: float) -> float:
    if not values:
        return 0.0
    ordered = sorted(values)
    index = max(0, min(len(ordered) - 1, int(len(ordered) * fraction + 0.999999) - 1))
    return ordered[index]


def summarize(scenario: str, runs: dict[str, list[MatrixResult]]) -> None:
    rows = []
    for loop_name, measured in runs.items():
        median_seconds = statistics.median(item.seconds for item in measured)
        representative = min(
            measured, key=lambda item: abs(item.seconds - median_seconds)
        )
        rows.append(
            (
                loop_name,
                median_seconds,
                representative.operations / median_seconds,
                representative.bytes_transferred / (1024 * 1024) / median_seconds,
                percentile(representative.latency_ms, 0.50),
                percentile(representative.latency_ms, 0.95),
                percentile(representative.latency_ms, 0.99),
                int(statistics.median(item.peak_rss_bytes for item in measured)),
            )
        )
    rows.sort(key=lambda row: row[1])
    print(f"\n{scenario}")
    print(
        f"{'loop':<10} {'median_s':>10} {'ops/s':>12} {'MiB/s':>10} "
        f"{'p50_ms':>10} {'p95_ms':>10} {'p99_ms':>10} {'peak_rss':>12}"
    )
    for row in rows:
        print(
            f"{row[0]:<10} {row[1]:>10.4f} {row[2]:>12,.0f} {row[3]:>10.1f} "
            f"{row[4]:>10.3f} {row[5]:>10.3f} {row[6]:>10.3f} "
            f"{format_bytes(row[7]):>12}"
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

    output: list[dict[str, object]] = []
    for scenario in scenarios:
        scenario_runs: dict[str, list[MatrixResult]] = {}
        for loop_name in available:
            print(f"Running {scenario} on {loop_name}...")
            if args.profile_rsloop_dir and loop_name == "rsloop":
                args.profile_rsloop_dir.mkdir(parents=True, exist_ok=True)
                label = str(args.profile_rsloop_dir / f"rsloop-{scenario}")
                run_child(args, loop_name, scenario, label)
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
