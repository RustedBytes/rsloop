"""Regressions for immediate socket I/O and cancellable local readiness waits."""

import array
import asyncio
import socket
import threading
import unittest

import rsloop


class LocalSocketTests(unittest.TestCase):
    def setUp(self):
        self.loop = rsloop.new_event_loop()
        self.addCleanup(self.loop.close)

    def pair(self):
        a, b = socket.socketpair()
        for sock in (a, b):
            sock.setblocking(False)
            self.addCleanup(sock.close)
        return a, b

    def run_async(self, coro):
        return self.loop.run_until_complete(asyncio.wait_for(coro, 10))

    def test_wait_created_before_run_and_immediate_completion(self):
        a, b = self.pair()
        pending = self.loop.sock_recv(a, 4)
        self.assertFalse(pending.done())
        self.loop.call_soon(b.send, b"test")
        self.assertEqual(self.run_async(pending), b"test")
        b.send(b"next")
        ready = self.loop.sock_recv(a, 4)
        self.assertTrue(ready.done())
        self.assertEqual(ready.result(), b"next")
        b.close()
        self.assertEqual(self.run_async(self.loop.sock_recv(a, 1)), b"")

    def test_cancelled_receive_does_not_consume_packet_or_retain_export(self):
        a, b = self.pair()

        async def exercise():
            buffer = bytearray(4)
            pending = self.loop.sock_recv_into(a, buffer)
            self.assertFalse(pending.done())
            pending.cancel()
            buffer.extend(b"resize")
            b.send(b"next")
            self.assertEqual(await self.loop.sock_recv(a, 4), b"next")
            await asyncio.sleep(0)
            self.assertEqual(buffer, bytearray(4) + b"resize")
            self.assertTrue(pending.cancelled())

        self.run_async(exercise())

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
                    self.assertGreater(count, 0)
                    received.extend(buffer[:count])

            reader = asyncio.create_task(receive())
            await self.loop.sock_sendall(a, payload)
            await reader
            self.assertEqual(received, payload)

        self.run_async(exercise())

    def test_cancelled_partial_send_releases_mutable_buffer(self):
        a, b = self.pair()
        a.setsockopt(socket.SOL_SOCKET, socket.SO_SNDBUF, 4096)

        async def exercise():
            payload = bytearray(b"x" * 1024 * 1024)
            pending = self.loop.sock_sendall(a, payload)
            self.assertFalse(pending.done())
            pending.cancel()
            payload.clear()
            await asyncio.sleep(0)
            while True:
                try:
                    b.recv(65536)
                except BlockingIOError:
                    break
            await asyncio.sleep(0)
            await asyncio.sleep(0)
            with self.assertRaises(BlockingIOError):
                b.recv(1)
            await self.loop.sock_sendall(a, b"after")
            self.assertEqual(await self.loop.sock_recv(b, 5), b"after")

        self.run_async(exercise())

    def test_typed_send_buffer_uses_byte_length(self):
        a, b = self.pair()
        payload = array.array("I", [1, 256, 65536, 0xFFFFFFFF])

        async def exercise():
            await self.loop.sock_sendall(a, payload)
            self.assertEqual(await self.loop.sock_recv(b, 100), payload.tobytes())

        self.run_async(exercise())

    def test_accept_is_nonblocking_and_overrides_run_on_owner_thread(self):
        owner = threading.get_ident()
        calls = []

        class Listener(socket.socket):
            def accept(self):
                calls.append(threading.get_ident())
                return super().accept()

        listener = Listener()
        self.addCleanup(listener.close)
        listener.bind(("127.0.0.1", 0))
        listener.listen()
        listener.setblocking(False)
        client = socket.socket()
        self.addCleanup(client.close)
        client.setblocking(False)

        async def exercise():
            pending = self.loop.sock_accept(listener)
            await self.loop.sock_connect(client, listener.getsockname())
            accepted, _ = await pending
            try:
                self.assertFalse(accepted.getblocking())
                await self.loop.sock_sendall(client, b"x")
                self.assertEqual(await self.loop.sock_recv(accepted, 1), b"x")
            finally:
                accepted.close()
            self.assertGreaterEqual(len(calls), 2)
            self.assertEqual(set(calls), {owner})

        self.run_async(exercise())

    def test_blocking_socket_is_rejected_without_entering_recv(self):
        a, _ = self.pair()
        a.setblocking(True)
        with self.assertRaisesRegex(ValueError, "non-blocking"):
            self.loop.sock_recv(a, 1)

    def test_closed_socket_and_empty_operations(self):
        a, b = self.pair()
        self.assertIsNone(self.run_async(self.loop.sock_sendall(a, b"")))
        self.assertEqual(self.run_async(self.loop.sock_recv(a, 0)), b"")
        self.assertEqual(self.run_async(self.loop.sock_recv_into(a, bytearray())), 0)
        a.close()
        with self.assertRaises(OSError):
            self.run_async(self.loop.sock_recv(a, 1))
        self.assertGreaterEqual(b.fileno(), 0)
