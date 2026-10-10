"""Documented websockets recv cancellation contracts over real TCP."""

import asyncio

import pytest
from websockets.asyncio.client import connect
from websockets.asyncio.server import serve
from websockets.exceptions import ConcurrencyError, ConnectionClosedOK

pytestmark = pytest.mark.ecosystem


@pytest.mark.parametrize("compression", [None, "deflate"])
@pytest.mark.parametrize("interruption", ["cancel", "timeout"])
def test_fragmented_receive_survives_cancellation(loop, compression, interruption):
    async def main():
        first_fragment = asyncio.Event()
        finish = asyncio.Event()
        failures = []
        fragments = ["Привіт ", "🌍 " * 8192, "кінець"]
        messages = [b"one", b"two", b"three"]

        async def handler(websocket):
            async def fragmented():
                yield fragments[0]
                first_fragment.set()
                await finish.wait()
                for fragment in fragments[1:]:
                    yield fragment

            try:
                await websocket.send(fragmented())
                for message in messages:
                    await websocket.send(message)
                await websocket.wait_closed()
            except Exception as error:
                failures.append(error)
                raise

        async with serve(
            handler, "127.0.0.1", 0, compression=compression, close_timeout=1
        ) as server:
            port = server.sockets[0].getsockname()[1]
            async with connect(
                f"ws://127.0.0.1:{port}",
                compression=compression,
                max_queue=1,
                close_timeout=1,
            ) as client:
                pending = asyncio.create_task(client.recv())
                try:
                    await first_fragment.wait()
                    # The ping/pong round trip lets the incomplete frame stream
                    # reach the client before cancellation, without a timed sleep.
                    pong = await client.ping(b"fragment-arrived")
                    await pong
                    assert not pending.done()
                    if interruption == "cancel":
                        pending.cancel()
                        with pytest.raises(asyncio.CancelledError):
                            await pending
                    else:
                        with pytest.raises(asyncio.TimeoutError):
                            await asyncio.wait_for(pending, 0)
                    finish.set()
                    assert await client.recv() == "".join(fragments)
                    assert [await client.recv() for _ in messages] == messages
                finally:
                    finish.set()
                    pending.cancel()
                    await asyncio.gather(pending, return_exceptions=True)
        assert not failures, failures

    loop.run_until_complete(asyncio.wait_for(main(), 20))


def test_concurrent_receive_rejected_and_close_wakes_reader(loop):
    async def main():
        async def handler(websocket):
            await websocket.wait_closed()

        async with serve(handler, "127.0.0.1", 0, close_timeout=1) as server:
            port = server.sockets[0].getsockname()[1]
            async with connect(f"ws://127.0.0.1:{port}", close_timeout=1) as client:
                entered = asyncio.Event()

                async def receive():
                    entered.set()
                    return await client.recv()

                pending = asyncio.create_task(receive())
                try:
                    await entered.wait()
                    with pytest.raises(ConcurrencyError):
                        await client.recv()
                    await client.close(code=1000, reason="finished")
                    with pytest.raises(ConnectionClosedOK):
                        await pending
                finally:
                    pending.cancel()
                    await asyncio.gather(pending, return_exceptions=True)

    loop.run_until_complete(asyncio.wait_for(main(), 20))
