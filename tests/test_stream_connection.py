"""Result assembly must retain loop affinity after cross-thread completion."""

import asyncio
import contextvars
import threading

from rsloop._stream_connection import open_connection


def test_connection_result_assembly_preserves_loop_thread_and_context():
    marker = contextvars.ContextVar("connection_marker")

    async def main():
        loop = asyncio.get_running_loop()
        owner = threading.get_ident()
        result = object()
        connected = loop.create_future()
        marker.set(result)

        def finish(created):
            assert threading.get_ident() == owner
            assert asyncio.get_running_loop() is loop
            assert marker.get() is result
            assert created is result
            return result

        worker = threading.Thread(
            target=loop.call_soon_threadsafe, args=(connected.set_result, result)
        )
        worker.start()
        try:
            assert (
                await asyncio.wait_for(open_connection(connected, finish), 5) is result
            )
        finally:
            worker.join(timeout=5)
            assert not worker.is_alive()

    asyncio.run(main())
