"""Flow-control callbacks may close a transport before the write call returns."""

import asyncio
import socket
import sys
from typing import cast

import pytest
import rsloop


@pytest.mark.skipif(
    sys.platform == "win32" or not hasattr(socket, "AF_UNIX"),
    reason="requires Unix transports",
)
@pytest.mark.parametrize("method", ["write", "writelines"])
@pytest.mark.parametrize("callback", ["pause", "resume"])
@pytest.mark.parametrize("finish", ["close", "write_eof"])
def test_flow_control_callback_preserves_unsent_data(callback, finish, method):
    payload = bytes(range(256)) * (1024 if method == "writelines" else 272)

    async def main():
        loop = asyncio.get_running_loop()
        sender, receiver = socket.socketpair()
        sender.setsockopt(socket.SOL_SOCKET, socket.SO_SNDBUF, 4096)
        sender.setblocking(False)
        receiver.setblocking(False)
        finished = loop.create_future()
        transport = None

        class Sender(asyncio.Protocol):
            def connection_made(self, transport):
                transport = cast(asyncio.Transport, transport)
                self.transport = transport
                loop.call_soon(self.send)

            def send(self):
                transport = self.transport
                if callback == "resume":
                    transport.set_write_buffer_limits(high=65536, low=65536)
                    if method == "writelines":
                        transport.writelines(
                            payload[offset : offset + 16384]
                            for offset in range(0, len(payload), 16384)
                        )
                    else:
                        for offset in range(0, len(payload), 4096):
                            transport.write(payload[offset : offset + 4096])
                else:
                    transport.set_write_buffer_limits(high=0, low=0)
                    if method == "writelines":
                        transport.writelines([payload[:8192], payload[8192:]])
                    else:
                        transport.write(payload)

            def pause_writing(self):
                if callback == "pause":
                    self.finish()

            def resume_writing(self):
                if callback == "resume":
                    self.finish()

            def finish(self):
                if not finished.done():
                    # Capture progress as the callback re-enters shutdown.
                    # Large batches can already be draining on resume.
                    remaining = self.transport.get_write_buffer_size()
                    finished.set_result(remaining)
                    getattr(self.transport, finish)()

        async def receive_all():
            data = bytearray()
            while chunk := await asyncio.wait_for(loop.sock_recv(receiver, 65536), 5):
                data.extend(chunk)
            return data

        try:
            transport, _ = await loop.create_unix_connection(Sender, sock=sender)
            if method == "writelines" and callback == "resume":
                # A large batch must make progress before it crosses low water.
                receiving = asyncio.create_task(receive_all())
                try:
                    await asyncio.wait_for(finished, 5)
                    assert await receiving == payload
                finally:
                    receiving.cancel()
                    await asyncio.gather(receiving, return_exceptions=True)
            else:
                assert await asyncio.wait_for(finished, 5) > 0
                assert await receive_all() == payload
        finally:
            if transport is not None:
                transport.close()
            sender.close()
            receiver.close()

    rsloop.run(main())
