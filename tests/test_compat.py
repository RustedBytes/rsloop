from __future__ import annotations

import asyncio
import errno
import os
import signal
import socket
import tempfile
import time
import threading
import unittest
import warnings
import builtins
from unittest import mock

import rsloop

EXCEPTION_GROUP = getattr(builtins, "ExceptionGroup", None)


class CompatibilityTests(unittest.TestCase):
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
            raise AssertionError("run_in_executor(None, ...) should fail after shutdown")

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
            raise AssertionError("getaddrinfo should fail after default executor shutdown")

        self.assertEqual(
            rsloop.run(main()),
            "Executor shutdown has been called",
        )

    def test_create_task_passes_kwargs_to_task_factory(self) -> None:
        async def main() -> tuple[dict[str, object], str]:
            loop = asyncio.get_running_loop()
            captured = {}

            async def coro():
                return "ok"

            def factory(loop, coro, **kwargs):
                if "custom_flag" in kwargs:
                    captured.update(kwargs)
                forwarded = dict(kwargs)
                forwarded.pop("custom_flag", None)
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
                await server.wait_closed()

        self.assertEqual(rsloop.run(main()), 1)

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
