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
                if transport is not None:
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


@pytest.mark.parametrize(
    ("segments", "size", "send_buffer"),
    [(1, 65536, 4096), (4, 65536, 4096), (20, 65536, 4096), (20, 1024, 262144)],
)
@pytest.mark.parametrize("finish", ["close", "write_eof"])
def test_writelines_partial_batch_preserves_order(segments, size, send_buffer, finish):
    async def main():
        loop = asyncio.get_running_loop()
        sender, receiver = socket.socketpair()
        sender.setblocking(False)
        receiver.setblocking(False)
        sender.setsockopt(socket.SOL_SOCKET, socket.SO_SNDBUF, send_buffer)
        transport = None
        # The tiny first segment puts a partial write inside a later iovec.
        pieces = [b"header"] + [bytes([index]) * size for index in range(segments)]
        batch = b"".join(pieces)
        try:
            transport, _ = await loop.create_connection(asyncio.Protocol, sock=sender)
            batches = 0
            while True:
                transport.writelines(iter([b"", *pieces, b""]))
                batches += 1
                if send_buffer != 4096 or transport.get_write_buffer_size() > 0:
                    break
                # SO_SNDBUF alone does not bound the peer's receive window.
                # Windows can accept/drain a whole batch before this check;
                # fill until backpressure is actually observed, without
                # yielding to the receiver. Preserve every byte for validation.
                assert batches * len(batch) < 32 * 1024 * 1024, (
                    "could not establish write backpressure within 32 MiB"
                )
            expected = batch * batches + b"tail"
            transport.write(b"tail")
            del pieces
            gc.collect()
            getattr(transport, finish)()
            received = bytearray()
            while chunk := await loop.sock_recv(receiver, 65536):
                received.extend(chunk)
            assert received == expected
        finally:
            if transport is not None:
                transport.close()
            sender.close()
            receiver.close()

    rsloop.run(asyncio.wait_for(main(), 15))


@pytest.mark.parametrize("size", [32768, 131072])
def test_writelines_snapshots_each_yield_and_validates_before_sending(size):
    async def main():
        loop = asyncio.get_running_loop()
        sender, receiver = socket.socketpair()
        sender.setblocking(False)
        receiver.setblocking(False)
        transport = None
        data = bytearray(b"a" * size)

        def invalid():
            yield b"must not be sent" * 4096
            raise ValueError("broken iterator")

        def changing():
            yield memoryview(data)
            data[:] = b"b" * len(data)
            yield data
            data[:] = b"c" * len(data)

        try:
            transport, _ = await loop.create_connection(asyncio.Protocol, sock=sender)
            with pytest.raises(ValueError, match="broken iterator"):
                transport.writelines(invalid())
            assert transport.get_write_buffer_size() == 0
            with pytest.raises(BlockingIOError):
                receiver.recv(1)
            transport.writelines(changing())
            transport.write_eof()
            received = bytearray()
            while chunk := await loop.sock_recv(receiver, 65536):
                received.extend(chunk)
            assert received == b"a" * size + b"b" * size
        finally:
            if transport is not None:
                transport.close()
            sender.close()
            receiver.close()

    rsloop.run(asyncio.wait_for(main(), 15))
