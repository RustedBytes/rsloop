"""Cancellation must release a pending read without discarding unread bytes."""

from __future__ import annotations

import asyncio
import sys
from typing import Any, cast

import pytest
import rsloop
import rsloop._loop as loop_module

FastReader = cast(Any, loop_module).PyFastStreamReader


@pytest.fixture(params=["asyncio", "rsloop", "uvloop"])
def loop_factory(request):
    if request.param == "uvloop":
        return pytest.importorskip("uvloop").new_event_loop
    return (asyncio if request.param == "asyncio" else rsloop).new_event_loop


def run(factory, main):
    # Runner/asyncio.run(loop_factory=...) are not available on Python 3.10.
    loop = factory()
    try:
        return loop.run_until_complete(asyncio.wait_for(main(), 5))
    finally:
        loop.run_until_complete(loop.shutdown_asyncgens())
        loop.close()


def new_reader(native) -> Any:
    loop = asyncio.get_running_loop()
    return FastReader(64, loop) if native else asyncio.StreamReader(limit=64)


def read(reader, method):
    if method == "read":
        return reader.read(8)
    if method == "readexactly":
        return reader.readexactly(8)
    if method == "readuntil":
        return reader.readuntil(b"\n")
    if method == "readline":
        return reader.readline()
    return reader.read()


async def await_read(reader, method):
    # Native read methods return Futures, stdlib methods return coroutines.
    # Wrap both so task.cancel() exercises actual task cancellation equally.
    return await read(reader, method)


@pytest.mark.parametrize(
    "native", [False, True], ids=["stdlib-reader", "native-reader"]
)
@pytest.mark.parametrize(
    "method", ["read", "readexactly", "readuntil", "readline", "readall"]
)
@pytest.mark.parametrize("finish", ["data", "eof", "error"])
def test_cancel_then_feed_before_cleanup(loop_factory, native, method, finish):
    async def main():
        reader = new_reader(native)
        if method in ("readexactly", "readuntil", "readline"):
            reader.feed_data(b"ab")
        task = asyncio.create_task(await_read(reader, method))
        await asyncio.sleep(0)
        assert not task.done()
        if method in ("readexactly", "readuntil", "readline"):
            reader.feed_data(b"cd")
        task.cancel()
        # No event-loop turn: feed must notice the cancelled Future itself.
        if finish == "error":
            reader.set_exception(ConnectionResetError("reset"))
        else:
            reader.feed_data(b"efgh\nTAIL")
            if finish == "eof":
                reader.feed_eof()
        with pytest.raises(asyncio.CancelledError):
            await task
        assert reader._waiter is None
        if finish == "error":
            with pytest.raises(ConnectionResetError, match="reset"):
                await reader.read(1)
            return
        prefix = b"abcd" if method in ("readexactly", "readuntil", "readline") else b""
        expected = prefix + b"efgh\nTAIL"
        assert await reader.readexactly(len(expected)) == expected
        reader.feed_eof()
        assert await reader.read() == b""
        assert reader.at_eof()

    run(loop_factory, main)


@pytest.mark.parametrize(
    "native", [False, True], ids=["stdlib-reader", "native-reader"]
)
@pytest.mark.parametrize(
    "method", ["read", "readexactly", "readuntil", "readline", "readall"]
)
def test_cancel_retry_and_concurrent_waiter(loop_factory, native, method):
    async def main():
        reader = new_reader(native)
        for _ in range(3):
            task = asyncio.create_task(await_read(reader, method))
            await asyncio.sleep(0)
            with pytest.raises((RuntimeError, ValueError), match="another coroutine"):
                await read(reader, method)
            task.cancel()
            with pytest.raises(asyncio.CancelledError):
                await task
            assert reader._waiter is None
        retry = asyncio.create_task(await_read(reader, method))
        await asyncio.sleep(0)
        assert not retry.done()
        reader.feed_data(b"1234567\n")
        reader.feed_eof()
        assert await retry == b"1234567\n"
        assert await reader.read() == b""

    run(loop_factory, main)


@pytest.mark.skipif(
    sys.version_info < (3, 11), reason="asyncio.timeout requires Python 3.11"
)
@pytest.mark.parametrize(
    "native", [False, True], ids=["stdlib-reader", "native-reader"]
)
@pytest.mark.parametrize(
    "method", ["read", "readexactly", "readuntil", "readline", "readall"]
)
def test_timeout_zero_then_read(loop_factory, native, method):
    async def main():
        reader = new_reader(native)
        with pytest.raises(TimeoutError):
            async with asyncio.timeout(0):
                await read(reader, method)
        # Retry before any data arrives, as redis-py/hiredis does.
        pending = asyncio.create_task(await_read(reader, method))
        asyncio.get_running_loop().call_soon(reader.feed_data, b"1234567\n")
        asyncio.get_running_loop().call_soon(reader.feed_eof)
        assert await pending == b"1234567\n"

    run(loop_factory, main)


@pytest.mark.parametrize(
    "method", ["read", "readexactly", "readuntil", "readline", "readall"]
)
def test_old_cancel_callback_cannot_clear_new_waiter(loop_factory, method):
    async def main():
        reader = new_reader(True)
        old = read(reader, method)
        old.cancel()
        # Native reads return Futures eagerly. Install a new waiter before the
        # old Future's queued done callback can execute.
        current = read(reader, method)
        await asyncio.sleep(0)
        assert reader._waiter is current
        with pytest.raises(ValueError, match="another coroutine"):
            read(reader, method)
        reader.feed_data(b"1234567\n")
        reader.feed_eof()
        assert await current == b"1234567\n"

    run(loop_factory, main)


@pytest.mark.parametrize("method", ["readexactly", "readuntil"])
def test_cancelled_partial_then_eof(loop_factory, method):
    async def main():
        for native in (False, True):
            reader = new_reader(native)
            reader.feed_data(b"ab")
            pending = asyncio.create_task(await_read(reader, method))
            await asyncio.sleep(0)
            reader.feed_data(b"cd")
            pending.cancel()
            with pytest.raises(asyncio.CancelledError):
                await pending
            reader.feed_eof()
            with pytest.raises(asyncio.IncompleteReadError) as caught:
                await read(reader, method)
            assert caught.value.partial == b"abcd"
            assert caught.value.expected == (8 if method == "readexactly" else None)
            assert await reader.read() == b""

    run(loop_factory, main)


@pytest.mark.skipif(
    sys.version_info < (3, 11), reason="asyncio.timeout requires Python 3.11"
)
def test_timeout_over_tcp(loop_factory):
    async def main():
        release = asyncio.Event()
        finished = asyncio.get_running_loop().create_future()

        async def serve(reader, writer):
            try:
                await release.wait()
                writer.write(b"hello")
                await writer.drain()
            finally:
                writer.close()
                await writer.wait_closed()
                finished.set_result(None)

        server = await asyncio.start_server(serve, "127.0.0.1", 0)
        try:
            reader, writer = await asyncio.open_connection(
                "127.0.0.1", server.sockets[0].getsockname()[1]
            )
            try:
                with pytest.raises(TimeoutError):
                    async with asyncio.timeout(0):
                        await reader.read(100)
                release.set()
                assert await reader.read(100) == b"hello"
                assert await reader.read(100) == b""
            finally:
                release.set()
                writer.close()
                await writer.wait_closed()
                await finished
        finally:
            server.close()
            await server.wait_closed()

    run(loop_factory, main)


def test_cancelled_exact_restores_flow_control(loop_factory):
    class Transport:
        paused = False

        def pause_reading(self):
            self.paused = True

        def resume_reading(self):
            self.paused = False

    async def main():
        reader = new_reader(True)
        transport = Transport()
        reader.set_transport_public(transport)
        pending = reader.readexactly(512)
        reader.feed_data(b"a" * 256)
        assert not transport.paused
        pending.cancel()
        await asyncio.sleep(0)
        assert reader._waiter is None
        assert transport.paused
        retry = reader.readexactly(512)
        assert not transport.paused
        reader.feed_data(b"b" * 256)
        assert await retry == b"a" * 256 + b"b" * 256

    run(loop_factory, main)


def test_waiter_callback_does_not_keep_reader_alive(loop_factory):
    import gc
    import weakref

    async def main():
        reader = new_reader(True)
        reference = weakref.ref(reader)
        pending = reader.readexactly(8)
        del reader
        gc.collect()
        assert reference() is None
        pending.cancel()
        await asyncio.sleep(0)

    run(loop_factory, main)
