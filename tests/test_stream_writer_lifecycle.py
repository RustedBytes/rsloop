"""Stream writer contracts under cancellation, backpressure and shutdown."""

import asyncio
import gc
import socket
import weakref
from typing import Any, cast

import pytest
import rsloop


@pytest.fixture(params=["asyncio", "rsloop", "uvloop"])
def loop(request):
    if request.param == "uvloop":
        factory = pytest.importorskip("uvloop").new_event_loop
    elif request.param == "rsloop":
        factory = rsloop.new_event_loop
    else:
        factory = asyncio.new_event_loop
    event_loop = factory()
    errors = []
    event_loop.set_exception_handler(lambda _, context: errors.append(context))
    yield event_loop
    pending = asyncio.all_tasks(event_loop)
    for task in pending:
        task.cancel()
    if pending:
        event_loop.run_until_complete(asyncio.gather(*pending, return_exceptions=True))
    event_loop.run_until_complete(event_loop.shutdown_asyncgens())
    event_loop.close()
    assert not pending, "test leaked tasks"
    assert not errors, errors


def run(loop, coro):
    return loop.run_until_complete(asyncio.wait_for(coro, 20))


@pytest.mark.parametrize("drain_api", ["writer", "protocol"])
@pytest.mark.parametrize("cancel_mode", ["cancel", "timeout"])
def test_cancelled_drains_release_waiters_while_still_paused(
    loop, cancel_mode, drain_api
):
    async def main():
        _, writer, peer = await connect(loop)
        protocol = cast(Any, writer.transport.get_protocol())
        drain_call = writer.drain if drain_api == "writer" else protocol._drain_helper
        paused = False
        try:
            protocol.pause_writing()
            paused = True

            async def cancelled_drain():
                entered = asyncio.Event()

                async def drain():
                    entered.set()
                    await drain_call()

                task = asyncio.create_task(drain())
                await entered.wait()
                assert not task.done()
                # Inspect only ownership, without relying on either protocol's container.
                reference = weakref.ref(cast(Any, task)._fut_waiter)
                if cancel_mode == "cancel":
                    task.cancel()
                    with pytest.raises(asyncio.CancelledError):
                        await task
                else:
                    with pytest.raises(TimeoutError):
                        await asyncio.wait_for(task, 0)
                return reference

            references = [await cancelled_drain() for _ in range(32)]
            checkpoint = asyncio.Event()
            loop.call_soon(checkpoint.set)
            await checkpoint.wait()
            gc.collect()
            assert all(reference() is None for reference in references), (
                "cancelled drains retained until the stalled peer resumes"
            )
            # Cancellation must not unpause the protocol or poison the next drain.
            entered = asyncio.Event()

            async def live_drain():
                entered.set()
                await drain_call()

            live = asyncio.create_task(live_drain())
            await entered.wait()
            assert not live.done()
            protocol.resume_writing()
            paused = False
            await live
            await drain_call()

        finally:
            if paused:
                protocol.resume_writing()
            peer.close()
            writer.close()
            await asyncio.wait_for(writer.wait_closed(), 5)

    run(loop, main())


async def connect(loop):
    listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    listener.bind(("127.0.0.1", 0))
    listener.listen()
    listener.setblocking(False)
    try:
        reader, writer = await asyncio.open_connection(*listener.getsockname())
        if isinstance(loop, rsloop.Loop):
            assert type(writer).__name__ == "PyFastStreamWriter"
        peer, _ = await loop.sock_accept(listener)
        peer.setblocking(False)
        return reader, writer, peer
    finally:
        listener.close()


def test_drain_yields_when_transport_is_closing(loop):
    async def main():
        _, writer, peer = await connect(loop)
        try:
            writer.close()
            assert writer.is_closing()
            # A closing drain must let connection_lost run, even when unpaused.
            with pytest.raises(ConnectionResetError):
                await writer.drain()
        finally:
            peer.close()
            writer.close()
            await asyncio.wait_for(writer.wait_closed(), 5)

    run(loop, main())


@pytest.mark.parametrize("finish", ["close", "write_eof"])
def test_slow_peer_concurrent_drains_cancellation_and_ordered_shutdown(loop, finish):
    async def main():
        reader, writer, peer = await connect(loop)
        tasks: list[asyncio.Task[Any]] = []
        try:
            writer.get_extra_info("socket").setsockopt(
                socket.SOL_SOCKET, socket.SO_SNDBUF, 4096
            )
            writer.transport.set_write_buffer_limits(high=1024, low=0)
            payload = bytes(range(256)) * 4096
            copies = 0
            while writer.transport.get_write_buffer_size() <= 1024:
                writer.write(payload)
                copies += 1
                assert copies * len(payload) <= 32 * 1024 * 1024
            started = [asyncio.Event() for _ in range(3)]

            async def drain(index):
                started[index].set()
                await writer.drain()

            tasks = [asyncio.create_task(drain(i)) for i in range(3)]
            for event in started:
                await event.wait()
            assert all(not task.done() for task in tasks)
            tasks[1].cancel()
            with pytest.raises(asyncio.CancelledError):
                await tasks[1]
            assert not tasks[0].done() and not tasks[2].done()

            async def receive():
                received = bytearray()
                while chunk := await loop.sock_recv(peer, 65536):
                    received.extend(chunk)
                return received

            receiver = asyncio.create_task(receive())
            tasks.append(receiver)
            await asyncio.gather(tasks[0], tasks[2])
            await writer.drain()
            writer.write(b"tail")
            getattr(writer, finish)()
            assert await receiver == payload * copies + b"tail"
            if finish == "write_eof":
                # Sending EOF must preserve the other direction of the stream.
                await loop.sock_sendall(peer, b"ack")
                peer.shutdown(socket.SHUT_WR)
                assert await reader.read() == b"ack"
        finally:
            for task in tasks:
                if not task.done():
                    task.cancel()
            await asyncio.gather(*tasks, return_exceptions=True)
            peer.close()
            writer.close()
            await asyncio.wait_for(writer.wait_closed(), 5)

    run(loop, main())


def test_reader_error_takes_precedence_over_closing_drain(loop):
    async def main():
        reader, writer, peer = await connect(loop)
        error = OSError("peer failed while closing")
        try:
            reader.set_exception(error)
            writer.close()
            with pytest.raises(OSError) as raised:
                await writer.drain()
            assert raised.value is error
        finally:
            peer.close()
            writer.close()
            await asyncio.wait_for(writer.wait_closed(), 5)

    run(loop, main())


@pytest.mark.parametrize("finish", ["close", "abort"])
def test_shutdown_wakes_all_paused_drains(loop, finish):
    async def main():
        _, writer, peer = await connect(loop)
        tasks = []
        try:
            writer.transport.get_protocol().pause_writing()
            started = [asyncio.Event() for _ in range(3)]

            async def drain(index):
                started[index].set()
                await writer.drain()

            tasks = [asyncio.create_task(drain(i)) for i in range(3)]
            for event in started:
                await event.wait()
            assert all(not task.done() for task in tasks)
            tasks[1].cancel()
            with pytest.raises(asyncio.CancelledError):
                await tasks[1]
            getattr(writer.transport, finish)()
            results = await asyncio.gather(tasks[0], tasks[2], return_exceptions=True)
            # Graceful connection_lost(None) releases existing drains; a new
            # drain observes the closed connection and must fail.
            assert results == [None, None]
            await asyncio.wait_for(writer.wait_closed(), 5)
            with pytest.raises(ConnectionResetError):
                await writer.drain()
        finally:
            for task in tasks:
                if not task.done():
                    task.cancel()
            await asyncio.gather(*tasks, return_exceptions=True)
            peer.close()
            writer.close()
            await asyncio.wait_for(writer.wait_closed(), 5)

    run(loop, main())
