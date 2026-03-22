import asyncio
import os
import sys
import unittest

import rsloop


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
            proc = await asyncio.create_subprocess_shell(
                "echo shell-ok",
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
            stdout, stderr = await proc.communicate()
            return {
                "stdout": stdout.decode().strip(),
                "stderr": stderr.decode().strip(),
                "returncode": proc.returncode,
            }

        self.assertEqual(
            rsloop.run(main()),
            {
                "stdout": "shell-ok",
                "stderr": "",
                "returncode": 0,
            },
        )


if __name__ == "__main__":
    unittest.main()
