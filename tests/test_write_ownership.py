"""Queued transport writes must own a stable snapshot until delivery."""

import asyncio
import gc
import socket

import pytest
import rsloop


@pytest.mark.parametrize(
    "kind", ["bytes", "subclass", "bytearray", "memoryview", "strided"]
)
@pytest.mark.parametrize("finish", ["close", "write_eof"])
def test_queued_write_owns_original_contents(kind, finish):
    async def main():
        loop = asyncio.get_running_loop()
        sender, receiver = socket.socketpair()
        sender.setblocking(False)
        receiver.setblocking(False)
        sender.setsockopt(socket.SOL_SOCKET, socket.SO_SNDBUF, 4096)
        transport = None
        expected = bytes(range(256)) * 1024
        backing = bytearray(expected)
        finalized = []

        class FinalizingBytes(bytes):
            def __del__(self):
                finalized.append(transport.get_write_buffer_size())

        if kind == "subclass":
            payload = FinalizingBytes(backing)
        elif kind == "bytes":
            payload = bytes(backing)
        elif kind == "bytearray":
            payload = backing
        elif kind == "memoryview":
            payload = memoryview(backing)
        else:
            backing = bytearray(value for byte in expected for value in (byte, 0))
            payload = memoryview(backing)[::2]
        try:
            transport, _ = await loop.create_connection(asyncio.Protocol, sock=sender)
            transport.write(payload)
            transport.write(b"tail")
            assert transport.get_write_buffer_size() > 0
            del payload
            backing[:] = b"z" * len(backing)
            gc.collect()
            if kind == "subclass":
                assert finalized
            getattr(transport, finish)()
            received = bytearray()
            while chunk := await asyncio.wait_for(loop.sock_recv(receiver, 65536), 5):
                received.extend(chunk)
            assert received == expected + b"tail"
        finally:
            if transport is not None:
                transport.close()
            sender.close()
            receiver.close()

    rsloop.run(asyncio.wait_for(main(), 15))
