"""Concurrency checks that matter once the interpreter stops serializing us.

On a GIL-enabled build these all still pass, they just do not prove much: the
interpreter is doing the mutual exclusion. On a free-threaded build they run
several `rsloop.Loop` instances truly in parallel, which is the only way to
exercise the transport fast paths without the GIL underneath them.
"""

from __future__ import annotations

import asyncio
import socket
import sys
import threading

import pytest
import rsloop

LOOP_THREADS = 4
ROUND_TRIPS = 24
CHUNK = b"".join(bytes([value]) * 512 for value in range(256))


def payload_for(thread_index: int, round_index: int) -> bytes:
    # Distinct per thread and per round so a torn or cross-connection copy
    # shows up as a mismatch rather than as accidentally-correct bytes.
    header = f"{thread_index:04d}:{round_index:04d}:".encode()
    return header + CHUNK


HEADER_SIZE = 8


async def echo_once(reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
    # Framed with a fixed-size header rather than `readline()`, because the
    # native fast reader only implements `read`/`readexactly` and this handler
    # has to work against both reader implementations.
    try:
        while True:
            try:
                header = await reader.readexactly(HEADER_SIZE)
            except asyncio.IncompleteReadError:
                break
            size = int.from_bytes(header, "big")
            writer.write(await reader.readexactly(size))
            await writer.drain()
    finally:
        writer.close()
        try:
            await writer.wait_closed()
        except (ConnectionResetError, BrokenPipeError):
            pass


async def round_trip(
    reader: asyncio.StreamReader,
    writer: asyncio.StreamWriter,
    data: bytes,
) -> bytes:
    writer.write(len(data).to_bytes(HEADER_SIZE, "big"))
    writer.write(data)
    await writer.drain()
    return await reader.readexactly(len(data))


def stdlib_streams_pair(
    limit: int = 65536,
) -> tuple[asyncio.StreamReader, asyncio.StreamReaderProtocol]:
    """Build the stdlib protocol, which is rsloop's *generic* reader fast path.

    `asyncio.StreamReaderProtocol` is fed by writing straight into the reader's
    `bytearray`, so this is the path guarded by the critical section in
    `transport::stream::protocol`.
    """
    loop = asyncio.get_running_loop()
    reader = asyncio.StreamReader(limit=limit, loop=loop)
    protocol = asyncio.StreamReaderProtocol(reader, loop=loop)
    return reader, protocol


async def stdlib_open_connection(
    host: str, port: int
) -> tuple[asyncio.StreamReader, asyncio.StreamWriter]:
    loop = asyncio.get_running_loop()
    reader, protocol = stdlib_streams_pair()
    transport, _ = await loop.create_connection(lambda: protocol, host, port)
    writer = asyncio.StreamWriter(transport, protocol, reader, loop)
    return reader, writer


async def stdlib_start_server(handler, host: str, port: int) -> asyncio.AbstractServer:
    loop = asyncio.get_running_loop()

    def factory() -> asyncio.StreamReaderProtocol:
        reader, _ = stdlib_streams_pair()
        return asyncio.StreamReaderProtocol(reader, handler, loop=loop)

    return await loop.create_server(factory, host, port)


def run_in_own_loop(coro_factory):
    """Run `coro_factory()` on a fresh rsloop loop owned by the calling thread."""
    loop = rsloop.new_event_loop()
    asyncio.set_event_loop(loop)
    try:
        return loop.run_until_complete(coro_factory())
    finally:
        asyncio.set_event_loop(None)
        loop.close()


def join_all(threads: list[threading.Thread], timeout: float = 120.0) -> None:
    for thread in threads:
        thread.join(timeout)
        if thread.is_alive():
            raise AssertionError(f"{thread.name} did not finish within {timeout}s")


class TestFreeThreading:
    def test_extension_does_not_force_the_gil_back_on(self) -> None:
        if not hasattr(sys, "_is_gil_enabled"):
            pytest.skip("interpreter predates sys._is_gil_enabled")
        if not rsloop.build_info()["free_threaded"]:
            pytest.skip("requires a free-threaded build of the extension")
        # Importing a module declared `gil_used = true` makes CPython re-enable
        # the GIL for the whole process, silently undoing free-threading.
        assert not sys._is_gil_enabled()

    def test_parallel_loops_echo_over_fast_streams(self) -> None:
        self._run_parallel_echo(asyncio.start_server, asyncio.open_connection)

    def test_parallel_loops_echo_over_stdlib_stream_reader_protocol(self) -> None:
        # The generic fast path writes into `StreamReader._buffer` through a raw
        # pointer; running it on several loops at once is the regression test
        # for that resize-and-copy being atomic.
        self._run_parallel_echo(stdlib_start_server, stdlib_open_connection)

    def _run_parallel_echo(self, start_server, open_connection) -> None:
        results: dict[int, list[bytes]] = {}
        failures: list[BaseException] = []
        barrier = threading.Barrier(LOOP_THREADS)

        async def exercise(thread_index: int) -> list[bytes]:
            server = await start_server(echo_once, "127.0.0.1", 0)
            port = server.sockets[0].getsockname()[1]
            try:
                reader, writer = await open_connection("127.0.0.1", port)
                try:
                    received = []
                    for round_index in range(ROUND_TRIPS):
                        expected = payload_for(thread_index, round_index)
                        received.append(await round_trip(reader, writer, expected))
                    return received
                finally:
                    writer.close()
                    try:
                        await writer.wait_closed()
                    except (ConnectionResetError, BrokenPipeError):
                        pass
            finally:
                server.close()
                await server.wait_closed()

        def worker(thread_index: int) -> None:
            try:
                # Line the threads up so the loops overlap instead of running
                # back to back and quietly re-serializing the test.
                barrier.wait(timeout=60)
                results[thread_index] = run_in_own_loop(lambda: exercise(thread_index))
            except BaseException as exc:  # noqa: BLE001 - reported below
                failures.append(exc)
                barrier.abort()

        threads = [
            threading.Thread(target=worker, args=(index,), name=f"rsloop-ft-{index}")
            for index in range(LOOP_THREADS)
        ]
        for thread in threads:
            thread.start()
        join_all(threads)

        if failures:
            raise failures[0]

        assert len(results) == LOOP_THREADS
        for thread_index in range(LOOP_THREADS):
            expected = [
                payload_for(thread_index, round_index)
                for round_index in range(ROUND_TRIPS)
            ]
            assert results[thread_index] == expected

    def test_call_soon_threadsafe_fan_in_from_many_threads(self) -> None:
        callbacks_per_thread = 500
        producer_threads = 8
        loop = rsloop.new_event_loop()
        seen: list[tuple[int, int]] = []
        done = threading.Event()

        def record(thread_index: int, sequence: int) -> None:
            seen.append((thread_index, sequence))
            if len(seen) == producer_threads * callbacks_per_thread:
                done.set()

        def produce(thread_index: int) -> None:
            for sequence in range(callbacks_per_thread):
                loop.call_soon_threadsafe(record, thread_index, sequence)

        async def main() -> None:
            threads = [
                threading.Thread(target=produce, args=(index,))
                for index in range(producer_threads)
            ]
            for thread in threads:
                thread.start()
            await asyncio.get_running_loop().run_in_executor(None, done.wait, 60)
            join_all(threads)

        asyncio.set_event_loop(loop)
        try:
            loop.run_until_complete(main())
        finally:
            asyncio.set_event_loop(None)
            loop.close()

        assert done.is_set(), "not every scheduled callback ran"
        assert len(seen) == producer_threads * callbacks_per_thread
        # Callbacks all run on the loop thread, so ordering within one producer
        # must be preserved even though the producers interleaved.
        for thread_index in range(producer_threads):
            sequences = [sequence for index, sequence in seen if index == thread_index]
            assert sequences == list(range(callbacks_per_thread))

    def test_parallel_loops_share_one_getaddrinfo_cache(self) -> None:
        # getaddrinfo and the TLS material caches are process-global; hammer
        # them from several loops so a torn cache init would show up.
        failures: list[BaseException] = []
        barrier = threading.Barrier(LOOP_THREADS)

        async def exercise() -> None:
            loop = asyncio.get_running_loop()
            for _ in range(16):
                infos = await loop.getaddrinfo("127.0.0.1", 0, type=socket.SOCK_STREAM)
                if not infos:
                    raise AssertionError("getaddrinfo returned nothing")

        def worker() -> None:
            try:
                barrier.wait(timeout=60)
                run_in_own_loop(exercise)
            except BaseException as exc:  # noqa: BLE001 - reported below
                failures.append(exc)
                barrier.abort()

        threads = [
            threading.Thread(target=worker, name=f"rsloop-dns-{index}")
            for index in range(LOOP_THREADS)
        ]
        for thread in threads:
            thread.start()
        join_all(threads)

        if failures:
            raise failures[0]
