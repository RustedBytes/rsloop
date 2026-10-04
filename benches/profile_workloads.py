"""Small correctness-checked probes for runtime paths outside TCP benchmarks."""

from __future__ import annotations

import asyncio
import contextvars
import os
import signal
import socket
import sys
import tempfile
import threading
from pathlib import Path


async def threadsafe(iterations: int) -> None:
    loop = asyncio.get_running_loop()
    done = loop.create_future()
    seen = []
    value = contextvars.ContextVar("profile_value", default=-1)

    def receive(i):
        seen.append((i, value.get()))
        if len(seen) == iterations:
            done.set_result(None)

    def producer():
        for i in range(iterations):
            value.set(i)
            loop.call_soon_threadsafe(receive, i)

    worker = threading.Thread(target=producer, name="profile-producer")
    worker.start()
    try:
        await done
    finally:
        worker.join()
    assert seen == [(i, i) for i in range(iterations)]


async def executor_dns(iterations: int) -> None:
    loop = asyncio.get_running_loop()
    for i in range(iterations):
        assert await loop.run_in_executor(None, sum, range(i + 1)) == i * (i + 1) // 2
        addresses = await loop.getaddrinfo("localhost", 80, type=socket.SOCK_STREAM)
        assert addresses
        host, port = await loop.getnameinfo(
            ("127.0.0.1", 80), socket.NI_NUMERICHOST | socket.NI_NUMERICSERV
        )
        assert (host, port) == ("127.0.0.1", "80")


async def udp(iterations: int) -> None:
    loop = asyncio.get_running_loop()
    with (
        socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as left,
        socket.socket(socket.AF_INET, socket.SOCK_DGRAM) as right,
    ):
        left.bind(("127.0.0.1", 0))
        right.bind(("127.0.0.1", 0))
        left.setblocking(False)
        right.setblocking(False)
        for i in range(iterations):
            payload = i.to_bytes(8, "little")
            await loop.sock_sendto(left, payload, right.getsockname())
            data, addr = await loop.sock_recvfrom(right, 64)
            assert data == payload
            await loop.sock_sendto(right, data, addr)
            buffer = bytearray(64)
            size, _ = await loop.sock_recvfrom_into(left, buffer)
            assert buffer[:size] == payload


async def socket_ops(iterations: int) -> None:
    loop = asyncio.get_running_loop()
    with socket.socket() as listener, socket.socket() as client:
        listener.bind(("127.0.0.1", 0))
        listener.listen()
        listener.setblocking(False)
        client.setblocking(False)
        connecting = asyncio.ensure_future(
            loop.sock_connect(client, listener.getsockname())
        )
        server, _ = await loop.sock_accept(listener)
        await connecting
        with server:
            server.setblocking(False)
            for _ in range(iterations):
                await loop.sock_sendall(client, b"socket probe")
                data = await loop.sock_recv(server, 64)
                assert data == b"socket probe"
                await loop.sock_sendall(server, data)
                buffer = bytearray(64)
                size = await loop.sock_recv_into(client, buffer)
                assert buffer[:size] == data


async def subprocess_pipes(iterations: int) -> None:
    for _ in range(iterations):
        process = await asyncio.create_subprocess_exec(
            sys.executable,
            "-c",
            "import sys; sys.stdout.buffer.write(sys.stdin.buffer.read()); sys.stderr.write('err')",
            stdin=asyncio.subprocess.PIPE,
            stdout=asyncio.subprocess.PIPE,
            stderr=asyncio.subprocess.PIPE,
        )
        try:
            out, err = await process.communicate(b"pipe probe" * 1024)
            assert out == b"pipe probe" * 1024 and err == b"err"
            assert process.returncode == 0
        finally:
            if process.returncode is None:
                process.kill()
                await process.wait()


async def unix_streams(iterations: int) -> None:
    async def echo(reader, writer):
        try:
            while data := await reader.read(4096):
                writer.write(data)
                await writer.drain()
        finally:
            writer.close()
            await writer.wait_closed()

    with tempfile.TemporaryDirectory(prefix="rsloop-profile-") as directory:
        path = str(Path(directory) / "sock")
        server = await asyncio.start_unix_server(echo, path)
        try:
            reader, writer = await asyncio.open_unix_connection(path)
            try:
                for _ in range(iterations):
                    writer.write(b"unix")
                    await writer.drain()
                    assert await reader.readexactly(4) == b"unix"
            finally:
                writer.close()
                await writer.wait_closed()
        finally:
            server.close()
            await server.wait_closed()


async def signals(iterations: int) -> None:
    loop = asyncio.get_running_loop()
    received = asyncio.Event()
    # Registration is dispatched to rsloop's runtime thread. SIGWINCH is
    # harmless before installation; retry delivery until that thread is ready.
    loop.add_signal_handler(signal.SIGWINCH, received.set)
    try:
        for _ in range(iterations):
            received.clear()
            while not received.is_set():
                os.kill(os.getpid(), signal.SIGWINCH)
                try:
                    await asyncio.wait_for(received.wait(), 0.05)
                except asyncio.TimeoutError:
                    pass
    finally:
        loop.remove_signal_handler(signal.SIGWINCH)


async def cancellation(iterations: int) -> None:
    async def waiting():
        await asyncio.sleep(60)

    for _ in range(iterations):
        task = asyncio.create_task(waiting())
        await asyncio.sleep(0)
        task.cancel()
        try:
            await task
        except asyncio.CancelledError:
            pass
        else:
            raise AssertionError("cancellation was lost")


RUNNERS = {
    "threadsafe": threadsafe,
    "executor_dns": executor_dns,
    "udp": udp,
    "socket_ops": socket_ops,
    "subprocess_pipes": subprocess_pipes,
    "unix_streams": unix_streams,
    "signals": signals,
    "cancellation": cancellation,
}
