#!/usr/bin/env python3
from __future__ import annotations

import argparse
import asyncio
import importlib
import os
import signal
import sys
import time
from collections.abc import Callable
from typing import Any

from fastapi import FastAPI, Query
import uvicorn


EventLoopFactory = Callable[[], asyncio.AbstractEventLoop]
EVENT_LOOP_CHOICES = ("asyncio", "std-async", "uvloop", "rsloop")

app = FastAPI(
    title="rsloop FastAPI demo",
    description="Run the same FastAPI app on stdlib asyncio, uvloop, or rsloop.",
)
app.state.selected_event_loop = "asyncio"


def normalize_event_loop(value: str) -> str:
    if value == "std-async":
        return "asyncio"
    return value


def configure_rsloop_fast_streams(loop_name: str) -> None:
    if normalize_event_loop(loop_name) == "rsloop":
        os.environ["RSLOOP_USE_FAST_STREAMS"] = "1"


def loop_factory_for(loop_name: str) -> EventLoopFactory:
    normalized = normalize_event_loop(loop_name)
    if normalized == "asyncio":
        return asyncio.new_event_loop
    if normalized == "uvloop":
        try:
            return importlib.import_module("uvloop").new_event_loop
        except ImportError as exc:  # pragma: no cover - depends on local env
            raise SystemExit(
                "uvloop is not installed. Run with `uv run --with uvloop ...`."
            ) from exc
    if normalized == "rsloop":
        try:
            return importlib.import_module("rsloop").new_event_loop
        except ImportError as exc:  # pragma: no cover - depends on local env
            raise SystemExit(
                "rsloop is not importable. Build/install it first, for example with "
                "`uv run --with maturin maturin develop --release`."
            ) from exc
    raise AssertionError(f"unsupported event loop: {loop_name}")


def run_with_loop(loop_name: str, coro: Any) -> Any:
    loop_factory = loop_factory_for(loop_name)
    if sys.version_info[:2] >= (3, 12):
        return asyncio.run(coro, loop_factory=loop_factory)

    loop = loop_factory()
    try:
        asyncio.set_event_loop(loop)
        return loop.run_until_complete(coro)
    finally:
        asyncio.set_event_loop(None)
        loop.close()


def current_loop_payload() -> dict[str, str]:
    loop = asyncio.get_running_loop()
    stream_mode = "stdlib asyncio streams"
    if app.state.selected_event_loop == "rsloop":
        stream_mode = (
            "rsloop fast streams"
            if os.environ.get("RSLOOP_USE_FAST_STREAMS", "1") != "0"
            else "stdlib asyncio streams"
        )
    return {
        "selected": app.state.selected_event_loop,
        "module": type(loop).__module__,
        "class": type(loop).__name__,
        "streams": stream_mode,
    }


@app.get("/")
async def root() -> dict[str, Any]:
    return {
        "service": "rsloop-fastapi-demo",
        "python": sys.version.split()[0],
        "event_loop": current_loop_payload(),
        "try": [
            "/health",
            "/sleep?delay=0.05",
            "/fanout?tasks=500&delay=0",
            "/stream-loopback?roundtrips=100&payload_size=256",
        ],
    }


@app.get("/health")
async def health() -> dict[str, Any]:
    return {"ok": True, "event_loop": current_loop_payload()}


@app.get("/sleep")
async def sleep_for(
    delay: float = Query(default=0.05, ge=0.0, le=10.0),
) -> dict[str, Any]:
    started = time.perf_counter()
    await asyncio.sleep(delay)
    return {
        "delay": delay,
        "elapsed": round(time.perf_counter() - started, 6),
        "event_loop": current_loop_payload(),
    }


@app.get("/fanout")
async def fanout(
    tasks: int = Query(default=500, ge=1, le=20_000),
    delay: float = Query(default=0.0, ge=0.0, le=1.0),
) -> dict[str, Any]:
    async def worker(index: int) -> int:
        await asyncio.sleep(delay)
        return index

    started = time.perf_counter()
    results = await asyncio.gather(*(worker(index) for index in range(tasks)))
    return {
        "tasks": tasks,
        "delay": delay,
        "elapsed": round(time.perf_counter() - started, 6),
        "result_checksum": sum(results),
        "event_loop": current_loop_payload(),
    }


async def maybe_wait_closed(writer: asyncio.StreamWriter) -> None:
    wait_closed = getattr(writer, "wait_closed", None)
    if wait_closed is None:
        return
    try:
        await wait_closed()
    except Exception:
        return


@app.get("/stream-loopback")
async def stream_loopback(
    roundtrips: int = Query(default=100, ge=1, le=5_000),
    payload_size: int = Query(default=256, ge=1, le=64 * 1024),
) -> dict[str, Any]:
    payload = b"x" * payload_size

    async def handle_echo(
        reader: asyncio.StreamReader, writer: asyncio.StreamWriter
    ) -> None:
        try:
            while True:
                data = await reader.read(64 * 1024)
                if not data:
                    break
                writer.write(data)
                await writer.drain()
        finally:
            writer.close()
            await maybe_wait_closed(writer)

    server = await asyncio.start_server(handle_echo, "127.0.0.1", 0)
    host, port = server.sockets[0].getsockname()[:2]
    started = time.perf_counter()
    try:
        reader, writer = await asyncio.open_connection(host, port)
        try:
            for _ in range(roundtrips):
                writer.write(payload)
                await writer.drain()
                response = await reader.readexactly(payload_size)
                if response != payload:
                    raise RuntimeError("echo payload mismatch")
        finally:
            writer.close()
            await maybe_wait_closed(writer)
    finally:
        server.close()
        await server.wait_closed()

    return {
        "roundtrips": roundtrips,
        "payload_size": payload_size,
        "elapsed": round(time.perf_counter() - started, 6),
        "event_loop": current_loop_payload(),
    }


async def serve(args: argparse.Namespace) -> None:
    config = uvicorn.Config(
        app,
        host=args.host,
        port=args.port,
        log_level=args.log_level,
        access_log=not args.no_access_log,
    )
    server = uvicorn.Server(config)
    await server.serve()


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Run the FastAPI demo on stdlib asyncio, uvloop, or rsloop.",
    )
    parser.add_argument(
        "--event-loop",
        default="asyncio",
        choices=EVENT_LOOP_CHOICES,
        help="Event loop to use: asyncio (stdlib), std-async (alias), uvloop, or rsloop.",
    )
    parser.add_argument("--host", default="127.0.0.1", help="Bind address for Uvicorn.")
    parser.add_argument("--port", type=int, default=8000, help="Bind port for Uvicorn.")
    parser.add_argument(
        "--log-level",
        default="info",
        choices=("critical", "error", "warning", "info", "debug", "trace"),
        help="Uvicorn log level.",
    )
    parser.add_argument(
        "--no-access-log",
        action="store_true",
        help="Disable the Uvicorn access log.",
    )
    return parser.parse_args()


def main() -> int:
    args = parse_args()
    configure_rsloop_fast_streams(args.event_loop)
    app.state.selected_event_loop = normalize_event_loop(args.event_loop)
    previous_sigint_handler = signal.getsignal(signal.SIGINT)

    def interrupt_handler(signum: int, frame: Any) -> None:
        raise KeyboardInterrupt

    try:
        signal.signal(signal.SIGINT, interrupt_handler)
        run_with_loop(args.event_loop, serve(args))
    except KeyboardInterrupt:
        return 0
    finally:
        signal.signal(signal.SIGINT, previous_sigint_handler)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
