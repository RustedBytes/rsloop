from __future__ import annotations

import asyncio
import gc
import os
import shlex
import socket
import subprocess
import sys
import unittest

import rsloop


async def run_in_thread(func, /, *args):
    to_thread = getattr(asyncio, "to_thread", None)
    if to_thread is not None:
        return await to_thread(func, *args)

    loop = asyncio.get_running_loop()
    return await loop.run_in_executor(None, func, *args)


class RunTests(unittest.TestCase):
    def test_set_event_loop_accepts_rsloop_loop(self) -> None:
        loop = rsloop.new_event_loop()
        try:
            asyncio.set_event_loop(loop)
            self.assertIs(asyncio.get_event_loop(), loop)
        finally:
            asyncio.set_event_loop(None)
            loop.close()

    def test_run_executes_coroutine(self) -> None:
        async def main() -> str:
            return "ok"

        self.assertEqual(rsloop.run(main()), "ok")

    @unittest.skipUnless(
        os.name == "posix" and os.path.isdir("/proc/self/fd"),
        "requires /proc/self/fd",
    )
    def test_create_connection_does_not_leak_file_descriptors(self) -> None:
        def fd_count() -> int:
            return len(os.listdir("/proc/self/fd"))

        async def main() -> None:
            loop = asyncio.get_running_loop()

            class ServerProtocol(asyncio.Protocol):
                def connection_made(self, transport: asyncio.BaseTransport) -> None:
                    transport.close()

            server = await loop.create_server(ServerProtocol, "127.0.0.1", 0)
            try:
                port = server.sockets[0].getsockname()[1]

                for _ in range(64):
                    closed = loop.create_future()

                    class ClientProtocol(asyncio.Protocol):
                        def connection_made(
                            self, transport: asyncio.BaseTransport
                        ) -> None:
                            transport.close()

                        def connection_lost(self, exc: Exception | None) -> None:
                            if not closed.done():
                                closed.set_result(None)

                    await loop.create_connection(ClientProtocol, "127.0.0.1", port)
                    await asyncio.wait_for(closed, 1.0)
                    await asyncio.sleep(0)
            finally:
                server.close()
                await server.wait_closed()

        before = fd_count()
        rsloop.run(main())
        gc.collect()
        after = fd_count()
        self.assertLessEqual(after, before + 4)

    def test_connect_pipe_round_trip(self) -> None:
        async def main() -> tuple[str, str]:
            loop = asyncio.get_running_loop()
            read_done: asyncio.Future[str] = loop.create_future()
            write_done: asyncio.Future[None] = loop.create_future()

            class ReadProtocol(asyncio.Protocol):
                def __init__(self) -> None:
                    self.parts: list[bytes] = []

                def data_received(self, data: bytes) -> None:
                    self.parts.append(data)

                def connection_lost(self, exc: Exception | None) -> None:
                    if not read_done.done():
                        read_done.set_result(b"".join(self.parts).decode())

            class WriteProtocol(asyncio.Protocol):
                def connection_made(self, transport: asyncio.BaseTransport) -> None:
                    transport.write(b"pipe-write-demo")
                    transport.close()

                def connection_lost(self, exc: Exception | None) -> None:
                    if not write_done.done():
                        write_done.set_result(None)

            r_fd, w_fd = os.pipe()
            with os.fdopen(r_fd, "rb", buffering=0) as rfile, os.fdopen(
                w_fd, "wb", buffering=0
            ) as wfile:
                read_transport, _ = await loop.connect_read_pipe(ReadProtocol, rfile)
                wfile.write(b"pipe-read-demo")
                wfile.flush()
                wfile.close()
                read_value = await asyncio.wait_for(read_done, 1.0)
                read_transport.close()

            r_fd2, w_fd2 = os.pipe()
            with os.fdopen(r_fd2, "rb", buffering=0) as rfile2, os.fdopen(
                w_fd2, "wb", buffering=0
            ) as wfile2:
                write_transport, _ = await loop.connect_write_pipe(
                    WriteProtocol, wfile2
                )
                write_value = rfile2.read(len(b"pipe-write-demo")).decode()
                await asyncio.wait_for(write_done, 1.0)
                write_transport.close()

            return read_value, write_value

        self.assertEqual(rsloop.run(main()), ("pipe-read-demo", "pipe-write-demo"))

    def test_write_pipe_transport_reports_write_buffer_flow_control(self) -> None:
        async def main() -> dict[str, object]:
            loop = asyncio.get_running_loop()
            done: asyncio.Future[dict[str, object]] = loop.create_future()
            payload = b"x" * (256 * 1024)

            def drain_pipe(fd: int, expected: int) -> int:
                total = 0
                while total < expected:
                    chunk = os.read(fd, min(65536, expected - total))
                    if not chunk:
                        break
                    total += len(chunk)
                return total

            class WriteProtocol(asyncio.Protocol):
                def __init__(self) -> None:
                    self.events: list[str] = []

                def connection_made(self, transport: asyncio.BaseTransport) -> None:
                    self.transport = transport
                    self.default_limits = transport.get_write_buffer_limits()
                    transport.set_write_buffer_limits(high=1, low=0)
                    self.updated_limits = transport.get_write_buffer_limits()
                    self.invalid_limits_raised = False
                    try:
                        transport.set_write_buffer_limits(high=1, low=2)
                    except ValueError:
                        self.invalid_limits_raised = True
                    transport.write(payload)
                    self.size_after_write = transport.get_write_buffer_size()

                def pause_writing(self) -> None:
                    self.events.append("pause")

                def resume_writing(self) -> None:
                    self.events.append("resume")
                    if not done.done():
                        done.set_result(
                            {
                                "default_limits": self.default_limits,
                                "updated_limits": self.updated_limits,
                                "invalid_limits_raised": self.invalid_limits_raised,
                                "size_after_write": self.size_after_write,
                                "size_after_resume": self.transport.get_write_buffer_size(),
                                "events": list(self.events),
                            }
                        )
                    self.transport.close()

            r_fd, w_fd = os.pipe()
            with os.fdopen(r_fd, "rb", buffering=0) as rfile, os.fdopen(
                w_fd, "wb", buffering=0
            ) as wfile:
                read_task = asyncio.create_task(
                    run_in_thread(drain_pipe, rfile.fileno(), len(payload))
                )
                transport, _ = await loop.connect_write_pipe(WriteProtocol, wfile)
                try:
                    result = await asyncio.wait_for(done, 3.0)
                    self.assertEqual(
                        await asyncio.wait_for(read_task, 3.0), len(payload)
                    )
                    return result
                finally:
                    transport.close()

        self.assertEqual(
            rsloop.run(main()),
            {
                "default_limits": (16384, 65536),
                "updated_limits": (0, 1),
                "invalid_limits_raised": True,
                "size_after_write": 256 * 1024,
                "size_after_resume": 0,
                "events": ["pause", "resume"],
            },
        )

    def test_subprocess_exec_round_trip(self) -> None:
        async def main() -> dict[str, object]:
            loop = asyncio.get_running_loop()
            done: asyncio.Future[dict[str, object]] = loop.create_future()

            class ProcessProtocol(asyncio.SubprocessProtocol):
                def __init__(self) -> None:
                    self.stdout = bytearray()
                    self.stderr = bytearray()

                def connection_made(self, transport: asyncio.BaseTransport) -> None:
                    self.transport = transport

                def pipe_data_received(self, fd: int, data: bytes) -> None:
                    if fd == 1:
                        self.stdout.extend(data)
                    elif fd == 2:
                        self.stderr.extend(data)

                def connection_lost(self, exc: Exception | None) -> None:
                    if not done.done():
                        done.set_result(
                            {
                                "stdout": self.stdout.decode(),
                                "stderr": self.stderr.decode(),
                                "returncode": self.transport.get_returncode(),
                            }
                        )

            if os.name == "nt":
                program = sys.executable
                args = (
                    "-c",
                    "import sys; data = sys.stdin.buffer.read(); "
                    "sys.stdout.buffer.write(data.upper()); "
                    "sys.stderr.write('stderr-ok')",
                )
            else:
                program = "/bin/sh"
                args = ("-c", "tr '[:lower:]' '[:upper:]'; printf stderr-ok >&2")

            transport, _ = await loop.subprocess_exec(
                ProcessProtocol,
                program,
                *args,
                stdin=asyncio.subprocess.PIPE,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            stdin_transport = transport.get_pipe_transport(0)
            stdin_transport.write(b"hello subprocess")
            stdin_transport.close()
            return await asyncio.wait_for(done, 3.0)

        self.assertEqual(
            rsloop.run(main()),
            {
                "stdout": "HELLO SUBPROCESS",
                "stderr": "stderr-ok",
                "returncode": 0,
            },
        )

    def test_subprocess_shell_round_trip(self) -> None:
        async def main() -> dict[str, object]:
            result: dict[str, object] | None = None
            for _ in range(10):
                proc = await asyncio.create_subprocess_shell(
                    "echo shell-ok",
                    stdout=asyncio.subprocess.PIPE,
                    stderr=asyncio.subprocess.PIPE,
                )
                stdout, stderr = await asyncio.wait_for(proc.communicate(), 3.0)
                result = {
                    "stdout": stdout.decode().strip(),
                    "stderr": stderr.decode().strip(),
                    "returncode": proc.returncode,
                }
            assert result is not None
            return result

        self.assertEqual(
            rsloop.run(main()),
            {
                "stdout": "shell-ok",
                "stderr": "",
                "returncode": 0,
            },
        )

    def test_subprocess_exec_text_mode_round_trip(self) -> None:
        async def main() -> dict[str, object]:
            script = (
                "import sys; "
                "data = sys.stdin.read().rstrip('\\n'); "
                "sys.stdout.write('out:' + data + '\\r\\nsecond\\n'); "
                "sys.stderr.write('err:' + data + '\\r')"
            )
            proc = await asyncio.create_subprocess_exec(
                sys.executable,
                "-c",
                script,
                stdin=asyncio.subprocess.PIPE,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
                text=True,
                encoding="utf-8",
            )
            assert proc.stdin is not None
            assert proc.stdout is not None
            assert proc.stderr is not None
            proc.stdin.write("cafe\n")
            await proc.stdin.drain()
            proc.stdin.close()
            first_line = await asyncio.wait_for(proc.stdout.readline(), 3.0)
            rest = await asyncio.wait_for(proc.stdout.read(), 3.0)
            stderr = await asyncio.wait_for(proc.stderr.read(), 3.0)
            return {
                "stdin_type": type(proc.stdin).__name__,
                "stdout_type": type(proc.stdout).__name__,
                "stderr_type": type(proc.stderr).__name__,
                "first_line": first_line,
                "rest": rest,
                "stderr": stderr,
                "returncode": await asyncio.wait_for(proc.wait(), 3.0),
            }

        self.assertEqual(
            rsloop.run(main()),
            {
                "stdin_type": "_TextStreamWriter",
                "stdout_type": "_TextStreamReader",
                "stderr_type": "_TextStreamReader",
                "first_line": "out:cafe\n",
                "rest": "second\n",
                "stderr": "err:cafe\n",
                "returncode": 0,
            },
        )

    def test_subprocess_shell_text_mode_round_trip(self) -> None:
        async def main() -> dict[str, object]:
            script = (
                "import sys; "
                "sys.stdout.write('shell-out\\r\\n'); "
                "sys.stderr.write('shell-err\\r')"
            )
            if os.name == "nt":
                cmd = subprocess.list2cmdline([sys.executable, "-c", script])
            else:
                cmd = shlex.join([sys.executable, "-c", script])

            proc = await asyncio.create_subprocess_shell(
                cmd,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
                text=True,
                encoding="utf-8",
            )
            stdout, stderr = await asyncio.wait_for(proc.communicate(), 3.0)
            return {
                "stdout": stdout,
                "stderr": stderr,
                "stdout_value_type": type(stdout).__name__,
                "stderr_value_type": type(stderr).__name__,
                "returncode": proc.returncode,
            }

        self.assertEqual(
            rsloop.run(main()),
            {
                "stdout": "shell-out\n",
                "stderr": "shell-err\n",
                "stdout_value_type": "str",
                "stderr_value_type": "str",
                "returncode": 0,
            },
        )

    def test_getaddrinfo_accepts_type_keyword(self) -> None:
        async def main() -> list[tuple[object, ...]]:
            loop = asyncio.get_running_loop()
            return await loop.getaddrinfo(
                "localhost",
                80,
                type=socket.SOCK_STREAM,
            )

        addrinfos = rsloop.run(main())
        self.assertTrue(addrinfos)
        self.assertTrue(
            all(addrinfo[1] == socket.SOCK_STREAM for addrinfo in addrinfos),
        )


if __name__ == "__main__":
    unittest.main()
