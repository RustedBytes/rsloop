import asyncio
import os
import signal

import rsloop


class ReadPipeProtocol(asyncio.Protocol):
    def __init__(self, done: asyncio.Future[str]) -> None:
        self.done = done
        self.parts: list[bytes] = []

    def data_received(self, data: bytes) -> None:
        self.parts.append(data)

    def connection_lost(self, exc: Exception | None) -> None:
        if not self.done.done():
            self.done.set_result(b"".join(self.parts).decode())


class WritePipeProtocol(asyncio.Protocol):
    def __init__(self, done: asyncio.Future[None], payload: bytes) -> None:
        self.done = done
        self.payload = payload

    def connection_made(self, transport: asyncio.BaseTransport) -> None:
        self.transport = transport
        transport.write(self.payload)
        transport.close()

    def connection_lost(self, exc: Exception | None) -> None:
        if not self.done.done():
            self.done.set_result(None)


class ProcessProtocol(asyncio.SubprocessProtocol):
    def __init__(self, done: asyncio.Future[dict[str, object]]) -> None:
        self.done = done
        self.stdout = bytearray()
        self.stderr = bytearray()
        self.events: list[tuple[str, object]] = []

    def connection_made(self, transport: asyncio.BaseTransport) -> None:
        self.transport = transport
        self.events.append(("pid", transport.get_pid()))

    def pipe_data_received(self, fd: int, data: bytes) -> None:
        self.events.append(("data", fd, len(data)))
        if fd == 1:
            self.stdout.extend(data)
        elif fd == 2:
            self.stderr.extend(data)

    def process_exited(self) -> None:
        self.events.append(("returncode", self.transport.get_returncode()))

    def connection_lost(self, exc: Exception | None) -> None:
        if not self.done.done():
            self.done.set_result(
                {
                    "stdout": self.stdout.decode(),
                    "stderr": self.stderr.decode(),
                    "events": self.events,
                }
            )


async def demo_signal_handlers() -> None:
    loop = asyncio.get_running_loop()
    done: asyncio.Future[str] = loop.create_future()

    def on_usr1(label: str) -> None:
        if not done.done():
            done.set_result(label)

    loop.add_signal_handler(signal.SIGUSR1, on_usr1, "usr1")
    await asyncio.sleep(0.05)
    os.kill(os.getpid(), signal.SIGUSR1)
    print("signal handler:", await asyncio.wait_for(done, 1.0))
    print("signal removed:", loop.remove_signal_handler(signal.SIGUSR1))


async def demo_pipes() -> None:
    loop = asyncio.get_running_loop()

    r_fd, w_fd = os.pipe()
    read_done: asyncio.Future[str] = loop.create_future()
    with os.fdopen(r_fd, "rb", buffering=0) as rfile, os.fdopen(
        w_fd, "wb", buffering=0
    ) as wfile:
        transport, _ = await loop.connect_read_pipe(
            lambda: ReadPipeProtocol(read_done), rfile
        )
        wfile.write(b"pipe-read-demo")
        wfile.flush()
        wfile.close()
        print("connect_read_pipe:", await asyncio.wait_for(read_done, 1.0))
        transport.close()

    payload = b"pipe-write-demo"
    r_fd2, w_fd2 = os.pipe()
    write_done: asyncio.Future[None] = loop.create_future()
    with os.fdopen(r_fd2, "rb", buffering=0) as rfile2, os.fdopen(
        w_fd2, "wb", buffering=0
    ) as wfile2:
        transport, _ = await loop.connect_write_pipe(
            lambda: WritePipeProtocol(write_done, payload),
            wfile2,
        )
        print("connect_write_pipe:", rfile2.read(len(payload)).decode())
        await asyncio.wait_for(write_done, 1.0)
        transport.close()


async def demo_subprocesses() -> None:
    loop = asyncio.get_running_loop()

    exec_done: asyncio.Future[dict[str, object]] = loop.create_future()
    transport, _ = await loop.subprocess_exec(
        lambda: ProcessProtocol(exec_done),
        "/bin/sh",
        "-c",
        "cat; printf shell-stderr >&2",
        stdin=asyncio.subprocess.PIPE,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
    )
    stdin_transport = transport.get_pipe_transport(0)
    stdin_transport.write(b"subprocess-exec-demo")
    stdin_transport.close()
    print("subprocess_exec:", await asyncio.wait_for(exec_done, 2.0))

    shell_done: asyncio.Future[dict[str, object]] = loop.create_future()
    _, _ = await loop.subprocess_shell(
        lambda: ProcessProtocol(shell_done),
        "printf subprocess-shell-demo",
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
    )
    print("subprocess_shell:", await asyncio.wait_for(shell_done, 2.0))


async def demo_high_level_subprocesses() -> None:
    proc = await asyncio.create_subprocess_exec(
        "/bin/sh",
        "-c",
        "cat; printf merged-stderr >&2",
        stdin=asyncio.subprocess.PIPE,
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.STDOUT,
    )
    proc.stdin.write(b"create-subprocess-exec-demo")
    proc.stdin.write_eof()
    await proc.stdin.wait_closed()
    stdout, stderr = await proc.communicate()
    print(
        "create_subprocess_exec:",
        {
            "stdout": stdout.decode(),
            "stderr": stderr,
            "returncode": proc.returncode,
        },
    )

    shell_proc = await asyncio.create_subprocess_shell(
        "printf create-subprocess-shell-demo",
        stdout=asyncio.subprocess.PIPE,
        stderr=asyncio.subprocess.PIPE,
    )
    shell_stdout, shell_stderr = await shell_proc.communicate()
    print(
        "create_subprocess_shell:",
        {
            "stdout": shell_stdout.decode(),
            "stderr": shell_stderr.decode(),
            "returncode": shell_proc.returncode,
        },
    )


async def main() -> None:
    await demo_signal_handlers()
    await demo_pipes()
    await demo_subprocesses()
    await demo_high_level_subprocesses()


if __name__ == "__main__":
    rsloop.run(main())
