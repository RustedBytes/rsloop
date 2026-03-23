from __future__ import annotations

import asyncio
import errno
import socket
import time
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
        async def main() -> float:
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
                return time.monotonic() - started
            finally:
                server.close()
                await server.wait_closed()

        self.assertLess(rsloop.run(main()), 0.15)

    def test_shutdown_default_executor_timeout_warns_and_falls_back_to_nowait(
        self,
    ) -> None:
        async def main() -> tuple[list[bool], list[str]]:
            loop = asyncio.get_running_loop()
            calls = []

            class DummyExecutor:
                def shutdown(self, wait):
                    calls.append(wait)
                    if wait:
                        time.sleep(0.2)

            loop.set_default_executor(DummyExecutor())
            with warnings.catch_warnings(record=True) as caught:
                warnings.simplefilter("always")
                await loop.shutdown_default_executor(timeout=0.01)
            return calls, [str(item.message) for item in caught]

        calls, messages = rsloop.run(main())
        self.assertEqual(calls, [True, False])
        self.assertTrue(
            any("within 0.01 seconds" in message for message in messages),
            messages,
        )

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
