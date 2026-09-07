"""Regressions for immediate socket I/O and cancellable local readiness waits."""

import array
import asyncio
import socket
import threading

import pytest
import rsloop


class TestLocalSocket:
    @pytest.fixture(autouse=True)
    def setup_loop(self, request):
        self.request = request
        self.loop = rsloop.new_event_loop()
        request.addfinalizer(self.loop.close)

    def pair(self):
        a, b = socket.socketpair()
        for sock in (a, b):
            sock.setblocking(False)
            self.request.addfinalizer(sock.close)
        return a, b

    def run_async(self, coro):
        return self.loop.run_until_complete(asyncio.wait_for(coro, 10))

    def test_wait_created_before_run_and_immediate_completion(self):
        a, b = self.pair()
        pending = self.loop.sock_recv(a, 4)
        assert not pending.done()
        self.loop.call_soon(b.send, b"test")
        assert self.run_async(pending) == b"test"
        b.send(b"next")
        ready = self.loop.sock_recv(a, 4)
        assert ready.done()
        assert ready.result() == b"next"
        b.close()
        assert self.run_async(self.loop.sock_recv(a, 1)) == b""

    def test_cancelled_receive_does_not_consume_packet_or_retain_export(self):
        a, b = self.pair()

        async def exercise():
            buffer = bytearray(4)
            pending = self.loop.sock_recv_into(a, buffer)
            assert not pending.done()
            pending.cancel()
            buffer.extend(b"resize")
            b.send(b"next")
            assert await self.loop.sock_recv(a, 4) == b"next"
            await asyncio.sleep(0)
            assert buffer == bytearray(4) + b"resize"
            assert pending.cancelled()

        self.run_async(exercise())

    def test_busy_socket_loop_services_timers_and_threadsafe_callbacks(self):
        a, b = self.pair()
        start = threading.Event()
        remote = self.loop.create_future()
        timer = self.loop.create_future()

        def schedule_from_thread():
            if start.wait(5):
                self.loop.call_soon_threadsafe(remote.set_result, "remote")

        worker = threading.Thread(target=schedule_from_thread)
        worker.start()

        async def exercise():
            async def echo():
                while data := await self.loop.sock_recv(b, 1):
                    await self.loop.sock_sendall(b, data)

            peer = asyncio.create_task(echo())
            self.loop.call_later(0.005, timer.set_result, "timer")
            count = 0
            try:
                while count < 10 or not (remote.done() and timer.done()):
                    await self.loop.sock_sendall(a, b"x")
                    assert await self.loop.sock_recv(a, 1) == b"x"
                    count += 1
                    if count == 10:
                        start.set()
                assert remote.result() == "remote"
                assert timer.result() == "timer"
            finally:
                peer.cancel()
                await asyncio.gather(peer, return_exceptions=True)

        try:
            self.run_async(exercise())
        finally:
            start.set()
            worker.join(timeout=5)
        assert not worker.is_alive()

    def test_partial_send_and_receive_into_preserve_all_bytes(self):
        a, b = self.pair()
        a.setsockopt(socket.SOL_SOCKET, socket.SO_SNDBUF, 4096)
        payload = bytes(range(256)) * 4096

        async def exercise():
            received = bytearray()

            async def receive():
                buffer = bytearray(3137)
                while len(received) < len(payload):
                    count = await self.loop.sock_recv_into(b, buffer)
                    assert count > 0
                    received.extend(buffer[:count])

            reader = asyncio.create_task(receive())
            await self.loop.sock_sendall(a, payload)
            await reader
            assert received == payload

        self.run_async(exercise())

    def test_cancelled_partial_send_releases_mutable_buffer(self):
        a, b = self.pair()

        class PartialSender:
            # Windows socketpair uses TCP: even a small SO_SNDBUF does not
            # guarantee a 1 MiB send blocks. Force one real partial send and
            # then a readiness wait, independent of kernel buffer capacity.
            sent = 0
            attempts = 0
            blocked = True

            def gettimeout(self):
                return a.gettimeout()

            def fileno(self):
                return a.fileno()

            def send(self, data):
                self.attempts += 1
                if self.blocked and self.sent:
                    raise BlockingIOError
                count = a.send(memoryview(data)[:17] if self.blocked else data)
                self.sent += count
                return count

        sender = PartialSender()

        async def exercise():
            payload = bytearray(b"x" * 1024 * 1024)
            pending = self.loop.sock_sendall(sender, payload)
            assert not pending.done()
            assert 0 < sender.sent < len(payload)
            assert sender.attempts == 2
            pending.cancel()
            payload.clear()
            sender.blocked = False
            await asyncio.sleep(0)
            # Await delivery of exactly the accepted prefix; a TCP peer may
            # not observe it in a single nonblocking recv immediately.
            received = bytearray()
            while len(received) < sender.sent:
                received.extend(
                    await self.loop.sock_recv(b, sender.sent - len(received))
                )
            assert received == b"x" * sender.sent
            await asyncio.sleep(0)
            await asyncio.sleep(0)
            assert sender.sent <= 17
            assert sender.attempts == 2
            with pytest.raises(BlockingIOError):
                b.recv(1)
            await self.loop.sock_sendall(a, b"after")
            assert await self.loop.sock_recv(b, 5) == b"after"

        self.run_async(exercise())

    def test_typed_send_buffer_uses_byte_length(self):
        a, b = self.pair()
        payload = array.array("I", [1, 256, 65536, 0xFFFFFFFF])

        async def exercise():
            await self.loop.sock_sendall(a, payload)
            assert await self.loop.sock_recv(b, 100) == payload.tobytes()

        self.run_async(exercise())

    def test_accept_is_nonblocking_and_overrides_run_on_owner_thread(self):
        owner = threading.get_ident()
        calls = []

        class Listener(socket.socket):
            def accept(self):
                calls.append(threading.get_ident())
                return super().accept()

        listener = Listener()
        self.request.addfinalizer(listener.close)
        listener.bind(("127.0.0.1", 0))
        listener.listen()
        listener.setblocking(False)
        client = socket.socket()
        self.request.addfinalizer(client.close)
        client.setblocking(False)

        async def exercise():
            pending = self.loop.sock_accept(listener)
            await self.loop.sock_connect(client, listener.getsockname())
            accepted, _ = await pending
            try:
                assert not accepted.getblocking()
                await self.loop.sock_sendall(client, b"x")
                assert await self.loop.sock_recv(accepted, 1) == b"x"
            finally:
                accepted.close()
            assert len(calls) >= 2
            assert set(calls) == {owner}

        self.run_async(exercise())

    def test_blocking_socket_is_rejected_without_entering_recv(self):
        a, _ = self.pair()
        a.setblocking(True)
        with pytest.raises(ValueError, match="non-blocking"):
            self.loop.sock_recv(a, 1)

    def test_closed_socket_and_empty_operations(self):
        a, b = self.pair()
        assert self.run_async(self.loop.sock_sendall(a, b"")) is None
        assert self.run_async(self.loop.sock_recv(a, 0)) == b""
        assert self.run_async(self.loop.sock_recv_into(a, bytearray())) == 0
        a.close()
        with pytest.raises(OSError):
            self.run_async(self.loop.sock_recv(a, 1))
        assert b.fileno() >= 0
