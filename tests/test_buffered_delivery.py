"""BufferedProtocol delivery must release exports before Python callbacks."""

import array
import asyncio
import concurrent.futures
from typing import cast

import pytest
import rsloop


def transfer(seed, finish="close", buffered=True, server_sender=True):
    payload = bytes((value + seed) % 256 for value in range(256)) * 1024
    loop = rsloop.new_event_loop()

    async def exercise():
        done = loop.create_future()

        class Sender(asyncio.Protocol):
            def connection_made(self, transport):
                transport = cast(asyncio.Transport, transport)
                transport.writelines(
                    (
                        payload[:17],
                        bytearray(payload[17:123]),
                        memoryview(payload[123:]),
                    )
                )
                getattr(transport, finish)()

        class PlainReceiver(asyncio.Protocol):
            def __init__(self):
                self.received = bytearray()

            def data_received(self, data):
                self.received.extend(data)

            def connection_lost(self, exc):
                if not done.done():
                    if exc:
                        done.set_exception(exc)
                    else:
                        done.set_result(bytes(self.received))

        class BufferedReceiver(PlainReceiver, asyncio.BufferedProtocol):
            def __init__(self):
                super().__init__()
                self.buffer = array.array("I", [0] * 101)

            def get_buffer(self, sizehint):
                return self.buffer

            def buffer_updated(self, nbytes):
                self.received.extend(self.buffer.tobytes()[:nbytes])
                # Both resizing the old exporter and replacing it are legal:
                # delivery must not retain its export across this callback.
                self.buffer.append(0)
                self.buffer = array.array("I", [0] * 101)

        receiver = BufferedReceiver if buffered else PlainReceiver
        server_factory, client_factory = (
            (Sender, receiver) if server_sender else (receiver, Sender)
        )
        server = await loop.create_server(server_factory, "127.0.0.1", 0)
        transport = None
        try:
            transport, _ = await loop.create_connection(
                client_factory, *server.sockets[0].getsockname()
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
            raise AssertionError(
                f"delivery corrupted or lost payload: {len(received)}/{len(payload)} bytes"
            )
    finally:
        loop.close()


class TestBufferedDelivery:
    @pytest.mark.parametrize("finish", ["close", "write_eof"])
    def test_typed_buffer_can_resize_and_replace_after_delivery(self, finish):
        transfer(0, finish)

    def test_independent_loop_threads_preserve_typed_buffers(self):
        with concurrent.futures.ThreadPoolExecutor(max_workers=4) as executor:
            for result in executor.map(transfer, range(4)):
                assert result is None

    @pytest.mark.parametrize("finish", ["close", "write_eof"])
    @pytest.mark.parametrize("server_sender", [True, False])
    def test_immediate_shutdown_preserves_plain_protocol_data(
        self, finish, server_sender
    ):
        # Covers graceful shutdown while Windows is still switching a shared
        # socket from completion reads to nonblocking readiness reads.
        transfer(0, finish, buffered=False, server_sender=server_sender)
