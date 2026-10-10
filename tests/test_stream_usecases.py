"""Application-level stream contracts, compared with independent event loops.

Use loopback TCP and explicit peer acknowledgements. Write boundaries are not
assumed to be TCP packet boundaries; both fragmentation and coalescing are valid.
"""

from __future__ import annotations

import asyncio
import contextlib
import json
import struct

import pytest
import rsloop


@pytest.fixture(params=["asyncio", "rsloop", "uvloop"])
def event_loop(request):
    factory = (
        pytest.importorskip("uvloop").new_event_loop
        if request.param == "uvloop"
        else (asyncio if request.param == "asyncio" else rsloop).new_event_loop
    )
    loop = factory()
    errors = []
    loop.set_exception_handler(lambda _loop, context: errors.append(context))
    try:
        yield loop
    finally:
        pending = asyncio.all_tasks(loop)
        for task in pending:
            task.cancel()
        if pending:
            loop.run_until_complete(asyncio.gather(*pending, return_exceptions=True))
        loop.run_until_complete(loop.shutdown_asyncgens())
        loop.close()
        assert not pending, "test leaked background tasks"
        assert not errors, errors


@contextlib.asynccontextmanager
async def tcp_pair(*, limit=64):
    loop = asyncio.get_running_loop()
    accepted = loop.create_future()

    def accept(reader, writer):
        accepted.set_result((reader, writer))

    server = await asyncio.start_server(accept, "127.0.0.1", 0, limit=limit)
    writers = []
    try:
        client = await asyncio.open_connection(
            "127.0.0.1", server.sockets[0].getsockname()[1], limit=limit
        )
        writers.append(client[1])
        peer = await accepted
        writers.append(peer[1])
        if isinstance(loop, rsloop.Loop):
            assert type(client[0]).__name__ == "PyFastStreamReader"
            assert type(peer[0]).__name__ == "PyFastStreamReader"
        yield client, peer
    finally:
        for writer in writers:
            writer.close()
        await asyncio.gather(*(writer.wait_closed() for writer in writers))
        server.close()
        await server.wait_closed()


@contextlib.asynccontextmanager
async def background(coro):
    task = asyncio.create_task(coro)
    try:
        yield task
        if not task.cancelled():
            await task  # Peer assertions and exceptions must reach pytest.
    finally:
        if not task.done():
            task.cancel()
        with contextlib.suppress(asyncio.CancelledError):
            await task


@pytest.mark.parametrize("chunk_size", [1, 7, 4096])
@pytest.mark.parametrize("final_newline", [False, True])
def test_json_lines_async_iteration(event_loop, chunk_size, final_newline):
    records = [{"message": "Привіт 🌍"}, {"count": 0}, {"finished": True}]
    lines = [json.dumps(record, ensure_ascii=False).encode() for record in records]

    async def main():
        async with tcp_pair() as ((reader, writer), (peer_reader, peer_writer)):

            async def publish():
                for index, line in enumerate(lines):
                    if index < len(lines) - 1 or final_newline:
                        line += b"\n"
                    for offset in range(0, len(line), chunk_size):
                        peer_writer.write(line[offset : offset + chunk_size])
                        await peer_writer.drain()
                    if index == len(lines) - 1:
                        peer_writer.write_eof()
                    assert await peer_reader.readexactly(1) == b"!"

            async with background(publish()):
                received = []
                async for line in reader:
                    received.append(json.loads(line))
                    writer.write(b"!")
                    await writer.drain()
                assert received == records
                assert reader.at_eof()
                # Exhausted iterators must remain exhausted.
                with pytest.raises(StopAsyncIteration):
                    await anext(reader)

    event_loop.run_until_complete(asyncio.wait_for(main(), 10))


@pytest.mark.parametrize("truncated", [False, True])
def test_pipelined_length_prefixed_messages(event_loop, truncated):
    # Larger than the read watermark: a pending body must not deadlock the peer.
    payloads = [b"", bytes(range(256)) * 257, b"last message"]
    frames = b"".join(struct.pack("!I", len(body)) + body for body in payloads)
    missing = 4 if truncated else 0

    async def main():
        async with tcp_pair() as ((reader, writer), (peer_reader, peer_writer)):

            async def publish():
                data = frames[:-missing] if missing else frames
                for offset in range(0, len(data), 1019):
                    peer_writer.write(data[offset : offset + 1019])
                    await peer_writer.drain()
                peer_writer.write_eof()
                # Half-close must preserve the reverse direction.
                assert await peer_reader.readexactly(2) == b"OK"

            async with background(publish()):
                for index, body in enumerate(payloads):
                    (size,) = struct.unpack("!I", await reader.readexactly(4))
                    assert size == len(body)
                    if truncated and index == len(payloads) - 1:
                        with pytest.raises(asyncio.IncompleteReadError) as caught:
                            await reader.readexactly(size)
                        assert caught.value.expected == size
                        assert caught.value.partial == body[:-missing]
                    else:
                        assert await reader.readexactly(size) == body
                assert await reader.read() == b""
                writer.write(b"OK")
                await writer.drain()

    event_loop.run_until_complete(asyncio.wait_for(main(), 10))


@pytest.mark.parametrize("method", ["readline", "readuntil", "__anext__"])
def test_oversized_line_then_next_request(event_loop, method):
    async def main():
        async with tcp_pair(limit=8) as ((reader, writer), (_, peer_writer)):
            peer_writer.write(b"x" * 9 + b"\nOK\n")
            peer_writer.write_eof()
            if method != "readuntil":
                with pytest.raises(ValueError):
                    await getattr(reader, method)()
            else:
                with pytest.raises(asyncio.LimitOverrunError) as caught:
                    await reader.readuntil(b"\n")
                assert caught.value.consumed == 9
                # readuntil retains the rejected record, unlike readline.
                assert await reader.readexactly(10) == b"x" * 9 + b"\n"
            assert await reader.readline() == b"OK\n"
            assert await reader.readline() == b""

    event_loop.run_until_complete(asyncio.wait_for(main(), 10))


@pytest.mark.parametrize("size", [-1, -64])
def test_invalid_frame_length_leaves_stream_reusable(event_loop, size):
    async def main():
        async with tcp_pair() as ((reader, _), (_, peer_writer)):
            peer_writer.write(b"next")
            peer_writer.write_eof()
            # Validate while creating the awaitable as well as awaiting it:
            # asyncio reports ValueError inside the coroutine, not at creation.
            pending = reader.readexactly(size)
            with pytest.raises(ValueError, match="less than zero"):
                await pending
            assert await reader.readexactly(4) == b"next"
            assert await reader.read() == b""

    event_loop.run_until_complete(asyncio.wait_for(main(), 10))


def test_cancel_iteration_then_resume_same_connection(event_loop):
    async def main():
        async with tcp_pair() as ((reader, _), (_, peer_writer)):
            started = asyncio.Event()

            async def next_line():
                started.set()
                return await anext(reader)

            async with background(next_line()) as pending:
                await started.wait()
                assert not pending.done()
                pending.cancel()
                with pytest.raises(asyncio.CancelledError):
                    await pending
            peer_writer.write(b"after cancellation\n")
            peer_writer.write_eof()
            assert await anext(reader) == b"after cancellation\n"
            with pytest.raises(StopAsyncIteration):
                await anext(reader)

    event_loop.run_until_complete(asyncio.wait_for(main(), 10))
