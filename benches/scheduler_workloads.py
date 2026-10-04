"""Opt-in timer lifetime and generic TCP startup probes for hotpath_lab."""

from __future__ import annotations

import asyncio
import time
from typing import cast

from compare_event_loops import ChildResult


async def bench_timers(
    loop_name: str, iterations: int, batch_size: int, mode: str
) -> ChildResult:
    """Include scheduling, expiration/cancellation, and handle release."""
    if iterations <= 0 or batch_size <= 0:
        raise ValueError("iterations and batch_size must be positive")
    if mode not in {
        "timers_retained",
        "timers_discarded",
        "timers_cancelled",
        "timers_mixed",
    }:
        raise ValueError(f"unknown timer mode: {mode}")
    cancelled = mode in {"timers_cancelled", "timers_mixed"}
    loop = asyncio.get_running_loop()
    started = time.perf_counter()
    outstanding = iterations
    while outstanding:
        count = min(outstanding, batch_size)
        remaining = count
        done = loop.create_future()

        def fire(completion: asyncio.Future[None] = done) -> None:
            nonlocal remaining
            remaining -= 1
            if remaining == 0:
                completion.set_result(None)

        handles = []
        for index in range(count):
            if mode == "timers_discarded":
                loop.call_later(0, fire)
            else:
                # Deterministic scattered future deadlines exercise the heap
                # independently of the zero-delay FIFO. Cancel before expiry.
                delay = (
                    60 + ((index * 15319) % 2048) / 1024
                    if mode == "timers_mixed"
                    else 0
                )
                handles.append(loop.call_later(delay, fire))
        if cancelled:
            handle = None
            for handle in handles:
                handle.cancel()
            del handle
            # An expiring timer, rather than call_soon, waits for this batch's
            # cancelled deadlines to be processed as well.
            loop.call_later(0, done.set_result, None)
        await done
        assert remaining == (count if cancelled else 0)
        handles.clear()
        outstanding -= count
    return ChildResult(
        loop_name, mode, time.perf_counter() - started, iterations, 0, 0, 0
    )


async def bench_tcp_connect_churn(
    loop_name: str, iterations: int, payload_size: int
) -> ChildResult:
    """Use explicit stdlib protocols so native stream shortcuts cannot mask startup."""
    loop = asyncio.get_running_loop()

    class Echo(asyncio.Protocol):
        def connection_made(self, transport: asyncio.BaseTransport) -> None:
            self.transport = cast(asyncio.Transport, transport)

        def data_received(self, data: bytes) -> None:
            self.transport.write(data)

    server = await loop.create_server(Echo, "127.0.0.1", 0)
    port = server.sockets[0].getsockname()[1]
    payload = b"x" * payload_size
    started = time.perf_counter()
    try:
        for _ in range(iterations):
            reader = asyncio.StreamReader()
            protocol = asyncio.StreamReaderProtocol(reader)
            transport, _ = await loop.create_connection(
                lambda protocol=protocol: protocol, "127.0.0.1", port
            )
            writer = asyncio.StreamWriter(transport, protocol, reader, loop)
            try:
                writer.write(payload)
                assert await reader.readexactly(payload_size) == payload
            finally:
                writer.close()
                await writer.wait_closed()
    finally:
        server.close()
        await server.wait_closed()
    return ChildResult(
        loop_name,
        "tcp_connect_churn",
        time.perf_counter() - started,
        iterations,
        0,
        0,
        0,
    )
