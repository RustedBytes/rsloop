from __future__ import annotations

import asyncio
import errno
import gc
import inspect
import os
import signal
import socket
import struct
import sys
import tempfile
import time
import threading
import unittest
import warnings
import weakref
import builtins
import pathlib
from unittest import mock

import rsloop

EXCEPTION_GROUP = getattr(builtins, "ExceptionGroup", None)


class CompatibilityTests(unittest.TestCase):
    def test_create_connection_refused_does_not_hang(self) -> None:
        async def main() -> None:
            # Close a bound socket to obtain a port with no listener. On some
            # macOS versions a connection to a bound-but-not-listening socket
            # remains in progress instead of being refused.
            probe = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            probe.bind(("127.0.0.1", 0))
            port = probe.getsockname()[1]
            probe.close()

            loop = asyncio.get_running_loop()
            with self.assertRaises(ConnectionRefusedError):
                await asyncio.wait_for(
                    loop.create_connection(
                        asyncio.Protocol,
                        host="127.0.0.1",
                        port=port,
                    ),
                    timeout=3.0,
                )

        rsloop.run(main())

    @unittest.skipUnless(EXCEPTION_GROUP is not None, "requires ExceptionGroup")
    def test_create_connection_all_errors_returns_exception_group(self) -> None:
        async def main() -> int:
            loop = asyncio.get_running_loop()

            def fake_getaddrinfo(*args, **kwargs):
                return [
                    (socket.AF_INET6, socket.SOCK_STREAM, 0, "", ("::1", 41001, 0, 0)),
                    (socket.AF_INET, socket.SOCK_STREAM, 0, "", ("127.0.0.1", 41002)),
                ]

            async def fake_sock_connect(self, sock, address):
                raise OSError(errno.ECONNREFUSED, f"connect failed: {address!r}")

            with mock.patch("socket.getaddrinfo", new=fake_getaddrinfo):
                with mock.patch.object(
                    rsloop.Loop, "sock_connect", new=fake_sock_connect
                ):
                    with self.assertRaises(EXCEPTION_GROUP) as ctx:
                        await loop.create_connection(
                            asyncio.Protocol,
                            "compat.test",
                            443,
                            all_errors=True,
                        )
            self.assertTrue(
                all(isinstance(exc, OSError) for exc in ctx.exception.exceptions)
            )
            return len(ctx.exception.exceptions)

        self.assertEqual(rsloop.run(main()), 2)

    def test_create_connection_interleave_reorders_attempts(self) -> None:
        async def main() -> list[int]:
            loop = asyncio.get_running_loop()
            calls = []

            def fake_getaddrinfo(*args, **kwargs):
                return [
                    (socket.AF_INET6, socket.SOCK_STREAM, 0, "", ("::1", 42001, 0, 0)),
                    (socket.AF_INET6, socket.SOCK_STREAM, 0, "", ("::1", 42002, 0, 0)),
                    (socket.AF_INET, socket.SOCK_STREAM, 0, "", ("127.0.0.1", 42003)),
                    (socket.AF_INET, socket.SOCK_STREAM, 0, "", ("127.0.0.1", 42004)),
                ]

            async def fake_sock_connect(self, sock, address):
                calls.append(address[1])
                raise OSError(errno.ECONNREFUSED, "boom")

            with mock.patch("socket.getaddrinfo", new=fake_getaddrinfo):
                with mock.patch.object(
                    rsloop.Loop, "sock_connect", new=fake_sock_connect
                ):
                    with self.assertRaises(OSError):
                        await loop.create_connection(
                            asyncio.Protocol,
                            "compat.test",
                            80,
                            interleave=1,
                        )
            return calls

        self.assertEqual(rsloop.run(main()), [42001, 42003, 42002, 42004])

    def test_create_connection_happy_eyeballs_staggers_attempts(self) -> None:
        async def main() -> tuple[float, int]:
            loop = asyncio.get_running_loop()
            done = loop.create_future()

            class ServerProtocol(asyncio.Protocol):
                def connection_made(self, transport):
                    transport.close()

            class ClientProtocol(asyncio.Protocol):
                def connection_made(self, transport):
                    transport.close()

                def connection_lost(self, exc):
                    if not done.done():
                        done.set_result(None)

            server = await loop.create_server(ServerProtocol, "127.0.0.1", 0)
            try:
                port = server.sockets[0].getsockname()[1]
                slow_port = port + 1
                orig_sock_connect = rsloop.Loop.sock_connect

                def fake_getaddrinfo(*args, **kwargs):
                    return [
                        (
                            socket.AF_INET,
                            socket.SOCK_STREAM,
                            0,
                            "",
                            ("127.0.0.1", slow_port),
                        ),
                        (
                            socket.AF_INET,
                            socket.SOCK_STREAM,
                            0,
                            "",
                            ("127.0.0.1", port),
                        ),
                    ]

                async def fake_sock_connect(self, sock, address):
                    if address[1] == slow_port:
                        await asyncio.sleep(0.2)
                        raise OSError(errno.ECONNREFUSED, "slow fail")
                    return await orig_sock_connect(self, sock, address)

                started = time.monotonic()
                with mock.patch("socket.getaddrinfo", new=fake_getaddrinfo):
                    with mock.patch.object(
                        rsloop.Loop, "sock_connect", new=fake_sock_connect
                    ):
                        transport, _ = await loop.create_connection(
                            ClientProtocol,
                            "compat.test",
                            80,
                            happy_eyeballs_delay=0.01,
                        )
                        await asyncio.wait_for(done, 1.0)
                        transport.close()
                        await asyncio.sleep(0)
                        socket_fileno = transport.get_extra_info("socket").fileno()
                return time.monotonic() - started, socket_fileno
            finally:
                server.close()
                await server.wait_closed()

        elapsed, socket_fileno = rsloop.run(main())
        self.assertLess(elapsed, 0.15)
        self.assertEqual(socket_fileno, -1)

    def test_eof_after_data_reports_connection_lost(self) -> None:
        async def main() -> tuple[bytes, list[str]]:
            loop = asyncio.get_running_loop()
            done = loop.create_future()
            events = []
            received = bytearray()

            class ServerProtocol(asyncio.Protocol):
                def connection_made(self, transport):
                    transport.write(b"response-before-eof")
                    transport.close()

            class ClientProtocol(asyncio.Protocol):
                def data_received(self, data):
                    events.append("data")
                    received.extend(data)

                def eof_received(self):
                    events.append("eof")
                    return False

                def connection_lost(self, exc):
                    events.append("lost")
                    if not done.done():
                        done.set_result(None)

            server = await loop.create_server(ServerProtocol, "127.0.0.1", 0)
            try:
                port = server.sockets[0].getsockname()[1]
                await loop.create_connection(ClientProtocol, "127.0.0.1", port)
                await asyncio.wait_for(done, 1.0)
                return bytes(received), events
            finally:
                server.close()
                await server.wait_closed()

        received, events = rsloop.run(main())
        self.assertEqual(received, b"response-before-eof")
        self.assertEqual(events, ["data", "eof", "lost"])

    def test_close_flushes_coalesced_server_writes(self) -> None:
        first = b"a" * 128
        second = b"b" * 128

        async def main() -> bytes:
            async def handle(
                _reader: asyncio.StreamReader, writer: asyncio.StreamWriter
            ) -> None:
                writer.write(first)
                writer.write(second)
                writer.close()
                await writer.wait_closed()

            server = await asyncio.start_server(handle, "127.0.0.1", 0)
            try:
                host, port = server.sockets[0].getsockname()[:2]
                reader, writer = await asyncio.open_connection(host, port)
                try:
                    return await reader.read()
                finally:
                    writer.close()
                    await writer.wait_closed()
            finally:
                server.close()
                await server.wait_closed()

        self.assertEqual(rsloop.run(main()), first + second)

    @unittest.skipUnless(sys.platform == "win32", "requires Winsock")
    def test_write_reports_reset_while_reading_is_paused(self) -> None:
        async def main() -> BaseException | None:
            loop = asyncio.get_running_loop()
            connection_lost = loop.create_future()

            class ClientProtocol(asyncio.Protocol):
                def connection_made(self, transport):
                    transport.set_write_buffer_limits(0)
                    transport.pause_reading()

                def connection_lost(self, exc):
                    if not connection_lost.done():
                        connection_lost.set_result(exc)

            server_sock = socket.create_server(("127.0.0.1", 0))
            try:
                transport, _ = await loop.create_connection(
                    ClientProtocol, *server_sock.getsockname()[:2]
                )
                peer_sock, _ = server_sock.accept()
                peer_sock.setsockopt(
                    socket.SOL_SOCKET,
                    socket.SO_LINGER,
                    struct.pack("ii", 1, 0),
                )
                peer_sock.close()
                try:
                    for _ in range(1000):
                        await asyncio.sleep(0)
                        transport.write(b"foo")
                        if connection_lost.done():
                            break

                    return await asyncio.wait_for(connection_lost, 3.0)
                finally:
                    transport.close()
            finally:
                server_sock.close()

        self.assertIsNotNone(rsloop.run(main()))

    def test_readexactly_larger_than_flow_control_window(self) -> None:
        # Regression test: a readexactly() waiting for more than 2 * limit
        # bytes (default limit 64 KiB) used to deadlock because the reader
        # paused the transport once 2 * limit bytes were buffered while the
        # waiter still needed more data to complete. The stall also cascaded
        # into the peer's drain(), so the whole round trip is bounded by a
        # timeout instead of individual awaits.
        payload = b"x" * (1024 * 1024)

        async def echo_roundtrip() -> bytes:
            async def handle_echo(
                reader: asyncio.StreamReader, writer: asyncio.StreamWriter
            ) -> None:
                data = await reader.readexactly(len(payload))
                writer.write(data)
                await writer.drain()
                writer.close()

            server = await asyncio.start_server(handle_echo, "127.0.0.1", 0)
            try:
                host, port = server.sockets[0].getsockname()[:2]
                reader, writer = await asyncio.open_connection(host, port)
                try:
                    writer.write(payload)
                    await writer.drain()
                    return await reader.readexactly(len(payload))
                finally:
                    writer.close()
                    await writer.wait_closed()
            finally:
                server.close()
                await server.wait_closed()

        async def main() -> bytes:
            return await asyncio.wait_for(echo_roundtrip(), 15.0)

        self.assertEqual(rsloop.run(main()), payload)

    def test_add_reader_stale_fire_after_future_done(self) -> None:
        # Regression test for the anyio raw-socket wait pattern:
        # add_reader(sock, fut.set_result, None), complete the future on the
        # first fire, then remove_reader from a done callback — possibly
        # after the socket has already been closed (fileno() == -1). Stale
        # readiness fires must not call set_result on the done future again,
        # and the watcher must not spin on the closed descriptor.
        async def main() -> tuple[list, bool]:
            loop = asyncio.get_running_loop()
            errors: list = []
            loop.set_exception_handler(lambda _loop, context: errors.append(context))

            a, b = socket.socketpair()
            a.setblocking(False)
            b.setblocking(False)
            try:
                fut = loop.create_future()
                loop.add_reader(a, fut.set_result, None)
                b.send(b"x")  # make `a` readable; the datagram stays unread
                await asyncio.wait_for(fut, 5.0)

                # Close the socket before removing the reader, like anyio's
                # aclose() does; removal must still find the registration.
                a.close()
                removed = loop.remove_reader(a)

                # Give any stale readiness fire time to (incorrectly) run.
                await asyncio.sleep(0.2)
                return errors, removed
            finally:
                if a.fileno() != -1:
                    a.close()
                b.close()

        errors, removed = rsloop.run(main())
        self.assertEqual(errors, [])
        self.assertTrue(removed)

    def test_create_server_sock_listens_bound_socket(self) -> None:
        async def main() -> bytes:
            loop = asyncio.get_running_loop()
            sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
            sock.bind(("127.0.0.1", 0))
            port = sock.getsockname()[1]

            class ServerProtocol(asyncio.Protocol):
                def connection_made(self, transport):
                    transport.write(b"bound-socket-server")
                    transport.close()

            server = await loop.create_server(ServerProtocol, sock=sock)
            try:
                reader, writer = await asyncio.open_connection("127.0.0.1", port)
                try:
                    return await asyncio.wait_for(reader.read(), 1.0)
                finally:
                    writer.close()
                    await writer.wait_closed()
            finally:
                server.close()
                await server.wait_closed()

        self.assertEqual(rsloop.run(main()), b"bound-socket-server")

    def test_external_socket_read_wakes_without_waiting_for_timer(self) -> None:
        ready = threading.Event()
        server = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
        server.bind(("127.0.0.1", 0))
        server.listen(1)
        port = server.getsockname()[1]

        def serve_once() -> None:
            ready.set()
            conn, _ = server.accept()
            with conn:
                conn.sendall(b"external-socket-data")
            server.close()

        thread = threading.Thread(target=serve_once)
        thread.start()
        ready.wait(1.0)

        async def main() -> tuple[bytes, float]:
            loop = asyncio.get_running_loop()
            done = loop.create_future()
            received = bytearray()

            class ClientProtocol(asyncio.Protocol):
                def data_received(self, data):
                    received.extend(data)
                    if not done.done():
                        done.set_result(None)

            started = time.monotonic()
            await loop.create_connection(ClientProtocol, "127.0.0.1", port)
            await asyncio.wait_for(done, 2.0)
            return bytes(received), time.monotonic() - started

        try:
            received, elapsed = rsloop.run(main())
        finally:
            thread.join(1.0)
            server.close()

        self.assertEqual(received, b"external-socket-data")
        self.assertLess(elapsed, 0.5)

    def test_call_later_raises_for_nan(self) -> None:
        # Regression for upstream issue #48: math.inf / oversized delays used to
        # panic in Duration::from_secs_f64. They must now schedule without firing
        # prematurely (inf, huge), fire ASAP (negative), and reject NaN.
        async def main() -> None:
            loop = asyncio.get_running_loop()

            with self.assertRaises(ValueError):
                loop.call_later(float("nan"), lambda: None)
            with self.assertRaises(ValueError):
                loop.call_at(float("nan"), lambda: None)
            with self.assertRaises(ValueError):
                await asyncio.sleep(float("nan"))

        rsloop.run(main())

    def test_sleep_forever(self) -> None:
        # Regression for upstream issue #48: asyncio.sleep(math.inf) (anyio's
        # sleep_forever) should be valid and be canceallable.

        async def main() -> None:
            task = asyncio.create_task(asyncio.sleep(float("inf")))
            await asyncio.sleep(0.05)
            task.cancel()
            with self.assertRaises(asyncio.CancelledError):
                await task

        rsloop.run(main())

    def test_shutdown_default_executor_timeout_warns_and_falls_back_to_nowait(
        self,
    ) -> None:
        async def main() -> tuple[list[bool], list[str]]:
            loop = asyncio.get_running_loop()
            calls = []
            messages = []

            class DummyExecutor:
                def shutdown(self, wait):
                    calls.append(wait)
                    if wait:
                        time.sleep(0.2)

            loop.set_default_executor(DummyExecutor())

            def capture_warning(message, category=None, stacklevel=1, source=None):
                messages.append(str(message))
                return None

            with mock.patch.object(warnings, "warn", side_effect=capture_warning):
                await loop.shutdown_default_executor(timeout=0.01)
            return calls, messages

        calls, messages = rsloop.run(main())
        self.assertEqual(calls, [True, False])
        self.assertTrue(
            any("within 0.01 seconds" in message for message in messages),
            messages,
        )

    def test_shutdown_default_executor_blocks_later_default_submissions(self) -> None:
        async def main() -> str:
            loop = asyncio.get_running_loop()

            class DummyExecutor:
                def submit(self, func, *args):
                    raise AssertionError("submit should not be called after shutdown")

                def shutdown(self, wait):
                    return None

            loop.set_default_executor(DummyExecutor())
            await loop.shutdown_default_executor()

            try:
                await loop.run_in_executor(None, lambda: 1)
            except RuntimeError as exc:
                return str(exc)
            raise AssertionError(
                "run_in_executor(None, ...) should fail after shutdown"
            )

        self.assertEqual(
            rsloop.run(main()),
            "Executor shutdown has been called",
        )

    def test_close_shuts_down_default_executor_without_waiting(self) -> None:
        calls = []

        class DummyExecutor:
            def shutdown(self, wait):
                calls.append(wait)

        loop = rsloop.new_event_loop()
        loop.set_default_executor(DummyExecutor())
        loop.close()
        loop.close()

        self.assertEqual(calls, [False])

    def test_shutdown_asyncgens_closes_active_generators(self) -> None:
        async def main() -> list[str]:
            loop = asyncio.get_running_loop()
            events = []

            async def gen():
                try:
                    yield "value"
                    await asyncio.sleep(10)
                finally:
                    events.append("closed")

            agen = gen()
            self.assertEqual(await agen.__anext__(), "value")
            await loop.shutdown_asyncgens()
            return events

        self.assertEqual(rsloop.run(main()), ["closed"])

    def test_shutdown_asyncgens_warns_on_new_iteration_after_shutdown(self) -> None:
        async def main() -> tuple[list[str], list[object]]:
            loop = asyncio.get_running_loop()
            messages = []
            sources = []

            async def gen():
                try:
                    yield "value"
                finally:
                    pass

            def capture_warning(message, category=None, stacklevel=1, source=None):
                messages.append(str(message))
                sources.append(source)
                return None

            with mock.patch.object(warnings, "warn", side_effect=capture_warning):
                await loop.shutdown_asyncgens()
                agen = gen()
                self.assertEqual(await agen.__anext__(), "value")
                await agen.aclose()

            return messages, sources

        messages, sources = rsloop.run(main())
        self.assertTrue(
            any("shutdown_asyncgens() call" in message for message in messages),
            messages,
        )
        self.assertEqual(len(sources), 1)
        self.assertIsInstance(sources[0], rsloop.Loop)

    def test_getaddrinfo_and_getnameinfo_use_default_executor(self) -> None:
        async def main() -> tuple[list[str], tuple[str, str]]:
            loop = asyncio.get_running_loop()
            calls = []

            class DummyExecutor:
                def submit(self, func, *args):
                    calls.append(func.__name__)
                    future = __import__("concurrent.futures").futures.Future()
                    try:
                        future.set_result(func(*args))
                    except BaseException as exc:
                        future.set_exception(exc)
                    return future

                def shutdown(self, wait):
                    return None

            loop.set_default_executor(DummyExecutor())
            addrinfos = await loop.getaddrinfo("localhost", 80, type=socket.SOCK_STREAM)
            host, service = await loop.getnameinfo(("127.0.0.1", 80))
            self.assertTrue(addrinfos)
            return calls, (host, service)

        calls, nameinfo = rsloop.run(main())
        self.assertEqual(calls, ["getaddrinfo", "getnameinfo"])
        self.assertEqual(nameinfo[1], "http")

    def test_getaddrinfo_honors_default_executor_shutdown(self) -> None:
        async def main() -> str:
            loop = asyncio.get_running_loop()

            class DummyExecutor:
                def submit(self, func, *args):
                    raise AssertionError("submit should not be called after shutdown")

                def shutdown(self, wait):
                    return None

            loop.set_default_executor(DummyExecutor())
            await loop.shutdown_default_executor()
            try:
                await loop.getaddrinfo("localhost", 80)
            except RuntimeError as exc:
                return str(exc)
            raise AssertionError(
                "getaddrinfo should fail after default executor shutdown"
            )

        self.assertEqual(
            rsloop.run(main()),
            "Executor shutdown has been called",
        )

    def test_create_task_passes_kwargs_to_task_factory(self) -> None:
        async def main() -> tuple[dict[str, object], str]:
            loop = asyncio.get_running_loop()
            captured = {}
            task_kwargs = {
                name
                for name, parameter in inspect.signature(
                    asyncio.Task
                ).parameters.items()
                if parameter.kind is inspect.Parameter.KEYWORD_ONLY
            }

            async def coro():
                return "ok"

            def factory(loop, coro, **kwargs):
                if "custom_flag" in kwargs:
                    captured.update(kwargs)
                forwarded = dict(kwargs)
                forwarded.pop("custom_flag", None)
                for key in tuple(forwarded):
                    if key not in task_kwargs:
                        forwarded.pop(key)
                return asyncio.Task(coro, loop=loop, **forwarded)

            loop.set_task_factory(factory)
            task = loop.create_task(
                coro(),
                name="demo",
                eager_start=False,
                custom_flag="seen",
            )
            return captured, await task

        captured, result = rsloop.run(main())
        self.assertEqual(result, "ok")
        self.assertEqual(captured["name"], "demo")
        self.assertEqual(captured["eager_start"], False)
        self.assertEqual(captured["custom_flag"], "seen")

    def test_create_task_accepts_eager_start_without_task_factory(self) -> None:
        async def main() -> tuple[bool, str]:
            loop = asyncio.get_running_loop()

            async def coro():
                await asyncio.sleep(0)
                return "done"

            task = loop.create_task(coro(), eager_start=False)
            pending_before = not task.done()
            return pending_before, await task

        self.assertEqual(rsloop.run(main()), (True, "done"))

    def test_create_task_rejects_unexpected_kwarg_without_task_factory(self) -> None:
        async def main() -> None:
            loop = asyncio.get_running_loop()

            async def coro():
                return "done"

            pending = coro()
            with self.assertRaisesRegex(
                TypeError,
                r"create_task\(\) got an unexpected keyword argument 'custom_flag'",
            ):
                loop.create_task(pending, custom_flag=True)
            pending.close()

        rsloop.run(main())

    def test_create_server_accepts_keep_alive(self) -> None:
        async def main() -> int:
            loop = asyncio.get_running_loop()
            server = await loop.create_server(
                asyncio.Protocol,
                "127.0.0.1",
                0,
                keep_alive=True,
            )
            try:
                sock = server.sockets[0]
                return sock.getsockopt(socket.SOL_SOCKET, socket.SO_KEEPALIVE)
            finally:
                server.close()
                await asyncio.sleep(0)

        expected = 8 if sys.platform == "darwin" else 1
        self.assertEqual(rsloop.run(main()), expected)

    def test_create_datagram_endpoint_round_trip(self) -> None:
        async def main() -> str:
            loop = asyncio.get_running_loop()
            done = loop.create_future()

            class ServerProtocol(asyncio.DatagramProtocol):
                def connection_made(self, transport):
                    self.transport = transport

                def datagram_received(self, data, addr):
                    self.transport.sendto(b"echo:" + data, addr)

            class ClientProtocol(asyncio.DatagramProtocol):
                def connection_made(self, transport):
                    self.transport = transport
                    transport.sendto(b"ping")

                def datagram_received(self, data, addr):
                    if not done.done():
                        done.set_result(data.decode())
                    self.transport.close()

            server_transport, _ = await loop.create_datagram_endpoint(
                ServerProtocol,
                local_addr=("127.0.0.1", 0),
            )
            try:
                port = server_transport.get_extra_info("sockname")[1]
                client_transport, _ = await loop.create_datagram_endpoint(
                    ClientProtocol,
                    remote_addr=("127.0.0.1", port),
                )
                try:
                    # remote_addr connects the socket, so peername is populated.
                    peername = client_transport.get_extra_info("peername")
                    self.assertEqual(peername[:2], ("127.0.0.1", port))
                    sock = client_transport.get_extra_info("socket")
                    self.assertEqual(tuple(sock.getpeername()), tuple(peername))
                    return await asyncio.wait_for(done, 1.0)
                finally:
                    client_transport.close()
            finally:
                server_transport.close()

        self.assertEqual(rsloop.run(main()), "echo:ping")

    def test_sendfile_fallback_writes_file_contents(self) -> None:
        async def main(path: str, expected: bytes) -> tuple[int, bytes]:
            loop = asyncio.get_running_loop()
            done = loop.create_future()
            client_done = loop.create_future()
            server_done = loop.create_future()

            class ServerProtocol(asyncio.Protocol):
                def connection_made(self, transport):
                    self.transport = transport

                def data_received(self, data):
                    if not done.done():
                        done.set_result(bytes(data))
                    self.transport.close()

                def connection_lost(self, exc):
                    if not server_done.done():
                        server_done.set_result(None)

            class ClientProtocol(asyncio.Protocol):
                def connection_made(self, transport):
                    self.transport = transport

                def connection_lost(self, exc):
                    if not client_done.done():
                        client_done.set_result(None)

            server = await loop.create_server(ServerProtocol, "127.0.0.1", 0)
            try:
                port = server.sockets[0].getsockname()[1]
                transport, _ = await loop.create_connection(
                    ClientProtocol,
                    "127.0.0.1",
                    port,
                )
                try:
                    with open(path, "rb") as f:
                        sent = await loop.sendfile(transport, f)
                    transport.close()
                    received = await asyncio.wait_for(done, 1.0)
                    await asyncio.wait_for(client_done, 1.0)
                    await asyncio.wait_for(server_done, 1.0)
                    return sent, received
                finally:
                    transport.close()
            finally:
                server.close()
                await asyncio.sleep(0)

        with tempfile.TemporaryDirectory() as tmpdir:
            path = pathlib.Path(tmpdir) / "payload.bin"
            payload = b"sendfile-payload"
            path.write_bytes(payload)
            self.assertEqual(
                rsloop.run(main(str(path), payload)), (len(payload), payload)
            )

    def test_sock_recvfrom_receives_datagram(self) -> None:
        async def main() -> tuple[bytes, tuple[str, int]]:
            loop = asyncio.get_running_loop()
            recv_sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
            send_sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
            try:
                recv_sock.bind(("127.0.0.1", 0))
                recv_sock.setblocking(False)
                addr = recv_sock.getsockname()
                send_sock.sendto(b"udp-data", addr)
                return await asyncio.wait_for(loop.sock_recvfrom(recv_sock, 1024), 1.0)
            finally:
                recv_sock.close()
                send_sock.close()

        data, addr = rsloop.run(main())
        self.assertEqual(data, b"udp-data")
        self.assertEqual(addr[0], "127.0.0.1")

    def test_sock_recvfrom_into_receives_datagram(self) -> None:
        async def main() -> tuple[int, bytes, tuple[str, int]]:
            loop = asyncio.get_running_loop()
            recv_sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
            send_sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
            try:
                recv_sock.bind(("127.0.0.1", 0))
                recv_sock.setblocking(False)
                addr = recv_sock.getsockname()
                send_sock.sendto(b"udp-into", addr)
                buf = bytearray(32)
                nbytes, peer = await asyncio.wait_for(
                    loop.sock_recvfrom_into(recv_sock, buf), 1.0
                )
                return nbytes, bytes(buf[:nbytes]), peer
            finally:
                recv_sock.close()
                send_sock.close()

        nbytes, data, addr = rsloop.run(main())
        self.assertEqual(nbytes, len(b"udp-into"))
        self.assertEqual(data, b"udp-into")
        self.assertEqual(addr[0], "127.0.0.1")

    def test_sock_sendto_sends_datagram(self) -> None:
        async def main() -> tuple[int, bytes]:
            loop = asyncio.get_running_loop()
            recv_sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
            send_sock = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
            try:
                recv_sock.bind(("127.0.0.1", 0))
                send_sock.setblocking(False)
                sent = await asyncio.wait_for(
                    loop.sock_sendto(send_sock, b"udp-sendto", recv_sock.getsockname()),
                    1.0,
                )
                data, _ = recv_sock.recvfrom(1024)
                return sent, data
            finally:
                recv_sock.close()
                send_sock.close()

        self.assertEqual(rsloop.run(main()), (len(b"udp-sendto"), b"udp-sendto"))

    def test_sock_sendfile_fallback_writes_file_contents(self) -> None:
        async def main(path: str) -> tuple[int, bytes]:
            loop = asyncio.get_running_loop()
            recv_done = loop.create_future()
            server_done = loop.create_future()

            class ServerProtocol(asyncio.Protocol):
                def connection_made(self, transport):
                    self.transport = transport

                def data_received(self, data):
                    if not recv_done.done():
                        recv_done.set_result(bytes(data))
                    self.transport.close()

                def connection_lost(self, exc):
                    if not server_done.done():
                        server_done.set_result(None)

            server = await loop.create_server(ServerProtocol, "127.0.0.1", 0)
            try:
                port = server.sockets[0].getsockname()[1]
                recv_sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
                recv_sock.bind(("127.0.0.1", 0))
                recv_sock.close()
                sock = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
                sock.setblocking(False)
                await loop.sock_connect(sock, ("127.0.0.1", port))
                try:
                    with open(path, "rb") as f:
                        sent = await loop.sock_sendfile(sock, f)
                    received = await asyncio.wait_for(recv_done, 1.0)
                    await asyncio.wait_for(server_done, 1.0)
                    return sent, received
                finally:
                    sock.close()
            finally:
                server.close()
                await asyncio.sleep(0)

        with tempfile.TemporaryDirectory() as tmpdir:
            path = pathlib.Path(tmpdir) / "payload.bin"
            payload = b"sock-sendfile"
            path.write_bytes(payload)
            self.assertEqual(rsloop.run(main(str(path))), (len(payload), payload))

    def test_slow_callback_duration_property(self) -> None:
        loop = rsloop.new_event_loop()
        try:
            self.assertEqual(loop.slow_callback_duration, 0.1)
            loop.slow_callback_duration = 0.25
            self.assertEqual(loop.slow_callback_duration, 0.25)
        finally:
            loop.close()

    @unittest.skipUnless(hasattr(socket, "AF_UNIX"), "unix sockets required")
    def test_create_unix_server_cleanup_socket_false_leaves_path(self) -> None:
        async def main(path: str) -> bool:
            loop = asyncio.get_running_loop()
            server = await loop.create_unix_server(
                asyncio.Protocol,
                path,
                cleanup_socket=False,
            )
            server.close()
            await server.wait_closed()
            return os.path.exists(path)

        with tempfile.TemporaryDirectory() as tmpdir:
            path = os.path.join(tmpdir, "sock")
            self.assertTrue(rsloop.run(main(path)))

    @unittest.skipUnless(hasattr(signal, "SIGUSR1"), "unix only")
    def test_add_signal_handler_rejects_non_main_thread(self) -> None:
        loop = rsloop.new_event_loop()
        try:
            errors = []

            def worker():
                try:
                    loop.add_signal_handler(signal.SIGUSR1, lambda: None)
                except BaseException as exc:
                    errors.append(exc)

            thread = threading.Thread(target=worker)
            thread.start()
            thread.join()

            self.assertEqual(len(errors), 1)
            self.assertIsInstance(errors[0], ValueError)
            self.assertIn("main thread", str(errors[0]))
        finally:
            loop.close()

    def test_ssl_shutdown_timeout_requires_ssl(self) -> None:
        async def main() -> tuple[str, str]:
            loop = asyncio.get_running_loop()
            create_connection_error = None
            create_server_error = None
            try:
                await loop.create_connection(
                    asyncio.Protocol,
                    "127.0.0.1",
                    80,
                    ssl_shutdown_timeout=0.1,
                )
            except ValueError as exc:
                create_connection_error = str(exc)

            try:
                await loop.create_server(
                    asyncio.Protocol,
                    "127.0.0.1",
                    0,
                    ssl_shutdown_timeout=0.1,
                )
            except ValueError as exc:
                create_server_error = str(exc)

            return create_connection_error, create_server_error

        self.assertEqual(
            rsloop.run(main()),
            (
                "ssl_shutdown_timeout is only meaningful with ssl",
                "ssl_shutdown_timeout is only meaningful with ssl",
            ),
        )

    def test_loop_allows_weak_refs(self) -> None:
        loop_ref: weakref.ReferenceType[asyncio.AbstractEventLoop] | None = None

        async def main():
            nonlocal loop_ref
            loop = asyncio.get_running_loop()
            num_refs = weakref.getweakrefcount(loop)
            loop_ref = weakref.ref(loop)
            self.assertEqual(weakref.getweakrefcount(loop), num_refs + 1)
            self.assertIs(loop_ref(), loop)

        rsloop.run(main())
        assert loop_ref is not None
        gc.collect()
        self.assertIsNone(loop_ref())

    def test_create_subprocess_accepts_explicit_popen_defaults(self) -> None:
        async def main():
            await asyncio.create_subprocess_exec(
                sys.executable,
                "-c",
                "import sys;sys.exit(0)",
                cwd=None,
                env=None,
                executable=None,
                umask=-1,
            )

        rsloop.run(main())

    def test_create_subprocess_exec_defaults_to_inherit(self) -> None:
        # High-level create_subprocess_exec leaves stdin/stdout/stderr at None,
        # which means "inherit the parent's fds" -> no pipe streams are created.
        async def main() -> None:
            proc = await asyncio.create_subprocess_exec(
                sys.executable,
                "-c",
                "import sys;sys.exit(0)",
            )
            await proc.wait()
            self.assertIsNone(proc.stdin)
            self.assertIsNone(proc.stdout)
            self.assertIsNone(proc.stderr)

        rsloop.run(main())

    def test_loop_subprocess_exec_defaults_to_pipe(self) -> None:
        # Low-level loop.subprocess_exec defaults omitted stdio to PIPE, so a
        # pipe transport is created for each of stdin/stdout/stderr.
        async def main() -> None:
            loop = asyncio.get_running_loop()
            transport, _ = await loop.subprocess_exec(
                asyncio.SubprocessProtocol,
                sys.executable,
                "-c",
                "import sys;sys.exit(0)",
            )
            try:
                self.assertIsNotNone(transport.get_pipe_transport(0))
                self.assertIsNotNone(transport.get_pipe_transport(1))
                self.assertIsNotNone(transport.get_pipe_transport(2))
            finally:
                transport.close()

    def test_set_write_buffer_limits_arguments_are_optional(self) -> None:
        # Regression test for issue #49: both arguments must be optional,
        # matching asyncio.WriteTransport.set_write_buffer_limits().
        async def main() -> None:
            loop = asyncio.get_running_loop()
            left, right = socket.socketpair()
            with left, right:
                transport, _ = await loop.create_connection(asyncio.Protocol, sock=left)
                try:
                    transport.set_write_buffer_limits(0)  # high only (the issue's call)
                    transport.set_write_buffer_limits(low=100)  # low only
                    transport.set_write_buffer_limits()  # no args -> defaults
                finally:
                    transport.close()

        rsloop.run(main())
