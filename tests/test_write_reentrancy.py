"""Flow-control callbacks may close a transport before the write call returns."""

import asyncio
import socket
import sys

import pytest
import rsloop


@pytest.mark.skipif(
    sys.platform == "win32" or not hasattr(socket, "AF_UNIX"),
    reason="requires Unix transports",
)
@pytest.mark.parametrize("callback", ["pause", "resume"])
@pytest.mark.parametrize("finish", ["close", "write_eof"])
def test_flow_control_callback_preserves_unsent_data(callback, finish):
    payload = bytes(range(256)) * 272  # 68 KiB; crosses the staging high water.

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
                self.transport = transport
                loop.call_soon(self.send)

            def send(self):
                transport = self.transport
                if callback == "resume":
                    transport.set_write_buffer_limits(high=65536, low=65536)
                    for offset in range(0, len(payload), 4096):
                        transport.write(payload[offset : offset + 4096])
                else:
                    transport.set_write_buffer_limits(high=0, low=0)
                    transport.write(payload)

            def pause_writing(self):
                if callback == "pause":
                    self.finish()

            def resume_writing(self):
                if callback == "resume":
                    self.finish()

            def finish(self):
                if not finished.done():
                    # The peer hasn't read anything. Its small send buffer
                    # forces the callback to run with an unsent suffix.
                    remaining = self.transport.get_write_buffer_size()
                    finished.set_result(remaining)
                    getattr(self.transport, finish)()

        try:
            transport, _ = await loop.create_unix_connection(Sender, sock=sender)
            assert await asyncio.wait_for(finished, 5) > 0
            data = bytearray()
            while chunk := await asyncio.wait_for(loop.sock_recv(receiver, 65536), 5):
                data.extend(chunk)
            assert data == payload
        finally:
            if transport is not None:
                transport.close()
            sender.close()
            receiver.close()

    rsloop.run(main())
