"""BufferedProtocol delivery must release exports before Python callbacks."""

import array
import asyncio
import concurrent.futures
import unittest

import rsloop


def transfer(seed):
    payload = bytes((value + seed) % 256 for value in range(256)) * 1024
    loop = rsloop.new_event_loop()

    async def exercise():
        done = loop.create_future()

        class Sender(asyncio.Protocol):
            def connection_made(self, transport):
                transport.writelines(
                    (
                        payload[:17],
                        bytearray(payload[17:123]),
                        memoryview(payload[123:]),
                    )
                )
                transport.close()

        class Receiver(asyncio.BufferedProtocol):
            def __init__(self):
                self.received = bytearray()
                self.buffer = array.array("I", [0] * 101)

            def get_buffer(self, sizehint):
                return self.buffer

            def buffer_updated(self, count):
                self.received.extend(self.buffer.tobytes()[:count])
                # Both resizing the old exporter and replacing it are legal:
                # delivery must not retain its export across this callback.
                self.buffer.append(0)
                self.buffer = array.array("I", [0] * 101)

            def connection_lost(self, exc):
                if not done.done():
                    if exc:
                        done.set_exception(exc)
                    else:
                        done.set_result(bytes(self.received))

        server = await loop.create_server(Sender, "127.0.0.1", 0)
        transport = None
        try:
            transport, _ = await loop.create_connection(
                Receiver, *server.sockets[0].getsockname()
            )
            return await asyncio.wait_for(done, 10)
        finally:
            if transport is not None:
                transport.close()
            server.close()
            await server.wait_closed()

    try:
        received = loop.run_until_complete(exercise())
        if received != payload:
            raise AssertionError("buffered delivery corrupted or lost payload")
    finally:
        loop.close()


class BufferedDeliveryTests(unittest.TestCase):
    def test_typed_buffer_can_resize_and_replace_after_delivery(self):
        transfer(0)

    def test_independent_loop_threads_preserve_typed_buffers(self):
        with concurrent.futures.ThreadPoolExecutor(max_workers=4) as executor:
            for result in executor.map(transfer, range(4)):
                self.assertIsNone(result)
