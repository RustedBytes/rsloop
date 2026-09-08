#!/usr/bin/env python3
"""Run a small ASGI application on Granian with a selectable event loop."""

from __future__ import annotations

import argparse
import asyncio
import importlib
import json
import os
from collections.abc import Callable
from pathlib import Path
from typing import Any

from granian import Granian, loops
from granian.constants import HTTPModes, Interfaces, Loops, RuntimeModes

EVENT_LOOP_ENV = "RSLOOP_GRANIAN_EVENT_LOOP"
EVENT_LOOP_CHOICES = ("asyncio", "uvloop", "winloop", "rsloop")
BENCHMARK_BODY = b"x" * (10 * 1024)


def loop_factory(loop_name: str) -> Callable[[], asyncio.AbstractEventLoop]:
    if loop_name == "asyncio":
        return asyncio.new_event_loop
    if loop_name == "uvloop":
        return importlib.import_module("uvloop").new_event_loop
    if loop_name == "winloop":
        return importlib.import_module("winloop").new_event_loop
    if loop_name == "rsloop":
        return importlib.import_module("rsloop").new_event_loop
    raise RuntimeError(f"unsupported event loop: {loop_name}")


@loops.register("auto")
def build_loop() -> asyncio.AbstractEventLoop:
    """Build the loop Granian will run in each worker."""
    loop_name = os.environ.get(EVENT_LOOP_ENV, "rsloop")
    loop = loop_factory(loop_name)()
    asyncio.set_event_loop(loop)
    return loop


def loop_payload() -> bytes:
    loop = asyncio.get_running_loop()
    return json.dumps(
        {
            "selected": os.environ.get(EVENT_LOOP_ENV, "rsloop"),
            "module": type(loop).__module__,
            "class": type(loop).__name__,
        },
        separators=(",", ":"),
    ).encode()


async def send_response(
    send: Any,
    body: bytes,
    *,
    content_type: bytes = b"text/plain",
    status: int = 200,
) -> None:
    await send(
        {
            "type": "http.response.start",
            "status": status,
            "headers": [
                (b"content-type", content_type),
                (b"content-length", str(len(body)).encode()),
            ],
        }
    )
    await send({"type": "http.response.body", "body": body})


async def app(scope: dict[str, Any], receive: Any, send: Any) -> None:
    if scope["type"] == "lifespan":
        while True:
            message = await receive()
            if message["type"] == "lifespan.startup":
                await send({"type": "lifespan.startup.complete"})
            elif message["type"] == "lifespan.shutdown":
                await send({"type": "lifespan.shutdown.complete"})
                return

    if scope["type"] != "http":
        return

    path = scope["path"]
    if path == "/benchmark":
        await send_response(send, BENCHMARK_BODY)
    elif path in {"/", "/loop", "/health"}:
        await send_response(send, loop_payload(), content_type=b"application/json")
    else:
        await send_response(send, b"not found\n", status=404)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Serve a minimal ASGI app with Granian and a selected loop."
    )
    parser.add_argument(
        "--event-loop",
        choices=EVENT_LOOP_CHOICES,
        default="rsloop",
        help="Event loop Granian creates in each worker (default: rsloop).",
    )
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=8000)
    parser.add_argument("--workers", type=int, default=1)
    parser.add_argument("--runtime-threads", type=int, default=1)
    parser.add_argument("--backpressure", type=int, default=1024)
    parser.add_argument("--access-log", action="store_true")
    parser.add_argument("--no-log", action="store_true")
    return parser.parse_args()


def main() -> None:
    args = parse_args()
    if not 1 <= args.port <= 65535:
        raise SystemExit("--port must be between 1 and 65535")
    if args.workers < 1:
        raise SystemExit("--workers must be >= 1")
    if args.runtime_threads < 1:
        raise SystemExit("--runtime-threads must be >= 1")
    if args.backpressure < 1:
        raise SystemExit("--backpressure must be >= 1")

    try:
        loop_factory(args.event_loop)
    except ImportError as exc:
        raise SystemExit(
            f"{args.event_loop} is not importable; install it before starting Granian"
        ) from exc

    os.environ[EVENT_LOOP_ENV] = args.event_loop
    Granian(
        "granian_service:app",
        address=args.host,
        port=args.port,
        interface=Interfaces.ASGI,
        workers=args.workers,
        runtime_threads=args.runtime_threads,
        runtime_mode=RuntimeModes.auto,
        loop=Loops.auto,
        http=HTTPModes.http1,
        websockets=False,
        backpressure=args.backpressure,
        log_enabled=not args.no_log,
        log_access=args.access_log,
        working_dir=Path(__file__).resolve().parent,
    ).serve()


if __name__ == "__main__":
    main()
