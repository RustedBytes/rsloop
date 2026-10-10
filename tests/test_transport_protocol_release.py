"""Closed transports must release protocols that keep their transport alive."""

import asyncio
import gc
import sys
import weakref
from typing import cast

import pytest
import rsloop


@pytest.mark.parametrize("close_method", ["close", "abort", "peer"])
@pytest.mark.parametrize("raise_on_lost", [False, True])
def test_closed_transport_releases_protocol(close_method, raise_on_lost):
    async def main():
        loop = asyncio.get_running_loop()
        loop.set_exception_handler(lambda *_: None)
        peers = []
        accepted = asyncio.Queue()

        class Peer(asyncio.Protocol):
            def connection_made(self, transport):
                peers.append(transport)
                accepted.put_nowait(transport)

        class Client(asyncio.Protocol):
            def __init__(self):
                self.lost = loop.create_future()

            def connection_made(self, transport):
                self.transport = transport

            def connection_lost(self, exc):
                assert self.transport.get_protocol() is self
                self.lost.set_result(None)
                if raise_on_lost:
                    raise RuntimeError("intentional connection_lost failure")

        server = await loop.create_server(Peer, "127.0.0.1", 0)
        refs = []
        transports = []
        try:
            for _ in range(20):
                transport, protocol = await loop.create_connection(
                    Client, "127.0.0.1", server.sockets[0].getsockname()[1]
                )
                peer = await asyncio.wait_for(accepted.get(), 5)
                refs.append(weakref.ref(protocol))
                transports.append(transport)
                if close_method == "peer":
                    peer.close()
                else:
                    getattr(transport, close_method)()
                await asyncio.wait_for(protocol.lost, 5)
                del protocol, transport
            await asyncio.sleep(0)
            gc.collect()
            assert all(ref() is None for ref in refs)
            assert all(transport.get_protocol() is None for transport in transports)
        finally:
            for transport in peers:
                transport.close()
            server.close()
            await server.wait_closed()

    loop = rsloop.new_event_loop()
    try:
        loop.run_until_complete(main())
    finally:
        loop.close()


def test_closed_stream_reader_releases_fast_path_references():
    async def main():
        loop = asyncio.get_running_loop()
        peers = []

        class Peer(asyncio.Protocol):
            def connection_made(self, transport):
                peers.append(transport)
                cast(asyncio.Transport, transport).write(b"reply")

        server = await loop.create_server(Peer, "127.0.0.1", 0)
        try:
            reader = asyncio.StreamReader()
            protocol = asyncio.StreamReaderProtocol(reader)
            transport, _ = await loop.create_connection(
                lambda protocol=protocol: protocol,
                "127.0.0.1",
                server.sockets[0].getsockname()[1],
            )
            assert await asyncio.wait_for(reader.readexactly(5), 5) == b"reply"
            reader_ref, protocol_ref = weakref.ref(reader), weakref.ref(protocol)
            writer = asyncio.StreamWriter(transport, protocol, reader, loop)
            writer.close()
            await asyncio.wait_for(writer.wait_closed(), 5)
            del reader, protocol, writer, _
            await asyncio.sleep(0)
            gc.collect()
            assert reader_ref() is None
            assert protocol_ref() is None
            assert transport.get_protocol() is None
        finally:
            for peer in peers:
                peer.close()
            server.close()
            await server.wait_closed()

    loop = rsloop.new_event_loop()
    try:
        loop.run_until_complete(main())
    finally:
        loop.close()


@pytest.mark.parametrize("raise_on_lost", [False, True])
def test_exited_subprocess_releases_protocol(raise_on_lost):
    async def main():
        loop = asyncio.get_running_loop()
        loop.set_exception_handler(lambda *_: None)

        class Client(asyncio.SubprocessProtocol):
            def __init__(self):
                self.lost = loop.create_future()

            def connection_made(self, transport):
                self.transport = transport

            def connection_lost(self, exc):
                self.lost.set_result(None)
                if raise_on_lost:
                    raise RuntimeError("intentional connection_lost failure")

        transport, protocol = await loop.subprocess_exec(
            Client, sys.executable, "-c", "pass"
        )
        try:
            ref = weakref.ref(protocol)
            await asyncio.wait_for(protocol.lost, 5)
            del protocol
            await asyncio.sleep(0)
            gc.collect()
            assert ref() is None
        finally:
            transport.close()

    loop = rsloop.new_event_loop()
    try:
        loop.run_until_complete(main())
    finally:
        loop.close()
