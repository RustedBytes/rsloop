# Examples

This page shows practical ways to use `rsloop`.

All examples use normal Python `asyncio` patterns. The difference is that `rsloop` provides the event loop implementation.

## Run one coroutine

This is the simplest starting point:

```python
import rsloop


async def main() -> None:
    print("hello from rsloop")


rsloop.run(main())
```

Use this style when your program has one main async entry point.

## Create tasks inside the loop

This looks almost the same as standard `asyncio`:

```python
import asyncio
import rsloop


async def worker(name: str) -> str:
    await asyncio.sleep(0.1)
    return f"{name} done"


async def main() -> None:
    task1 = asyncio.create_task(worker("first"))
    task2 = asyncio.create_task(worker("second"))
    results = await asyncio.gather(task1, task2)
    print(results)


rsloop.run(main())
```

If your app already uses `asyncio.create_task(...)`, `await`, and `gather(...)`, moving to `rsloop` is usually straightforward.

## Create a loop manually

Manual loop creation is useful when you want explicit setup and cleanup:

```python
import asyncio
import rsloop


async def main() -> None:
    print("running on", type(asyncio.get_running_loop()).__name__)


loop = rsloop.new_event_loop()
asyncio.set_event_loop(loop)
try:
    loop.run_until_complete(main())
finally:
    asyncio.set_event_loop(None)
    loop.close()
```

Use this style if your program manages the loop lifecycle itself.

## Schedule callbacks

`rsloop.Loop` supports the familiar callback APIs:

```python
import rsloop


loop = rsloop.new_event_loop()


def say(message: str) -> None:
    print(message)
    loop.stop()


loop.call_later(0.5, say, "timer fired")
loop.run_forever()
loop.close()
```

This is useful when reading older `asyncio` code that still uses callbacks instead of only coroutines.

## Work with raw sockets

You can use socket helpers directly on the running loop:

```python
import asyncio
import socket
import rsloop


async def main() -> None:
    loop = asyncio.get_running_loop()
    left, right = socket.socketpair()
    left.setblocking(False)
    right.setblocking(False)

    try:
        await loop.sock_sendall(left, b"ping")
        data = await loop.sock_recv(right, 4)
        print(data.decode())
    finally:
        left.close()
        right.close()


rsloop.run(main())
```

Use this style when you need lower-level control than streams provide.

## Start a TCP server

`rsloop` supports the normal protocol and transport style:

```python
import asyncio
import rsloop


class EchoProtocol(asyncio.Protocol):
    def connection_made(self, transport: asyncio.BaseTransport) -> None:
        self.transport = transport

    def data_received(self, data: bytes) -> None:
        self.transport.write(data.upper())


async def main() -> None:
    loop = asyncio.get_running_loop()
    server = await loop.create_server(EchoProtocol, "127.0.0.1", 9000)
    try:
        print("serving on 127.0.0.1:9000")
        await asyncio.sleep(60)
    finally:
        server.close()
        await server.wait_closed()


rsloop.run(main())
```

This is a good fit when your project already uses `asyncio.Protocol`.

## Use streams

Because `rsloop` can patch stream helpers, high-level stream code can stay familiar:

```python
import asyncio
import rsloop


async def handle_client(reader: asyncio.StreamReader, writer: asyncio.StreamWriter) -> None:
    data = await reader.read(100)
    writer.write(data.upper())
    await writer.drain()
    writer.close()
    await writer.wait_closed()


async def main() -> None:
    server = await asyncio.start_server(handle_client, "127.0.0.1", 9001)
    try:
        reader, writer = await asyncio.open_connection("127.0.0.1", 9001)
        writer.write(b"hello")
        await writer.drain()
        print((await reader.read(100)).decode())
        writer.close()
        await writer.wait_closed()
    finally:
        server.close()
        await server.wait_closed()


rsloop.run(main())
```

This is often the easiest path for developers who already use high-level `asyncio` streams.

## Run blocking work in an executor

Not everything has to be async:

```python
import asyncio
import rsloop


def blocking_sum() -> int:
    return sum(range(100000))


async def main() -> None:
    loop = asyncio.get_running_loop()
    result = await loop.run_in_executor(None, blocking_sum)
    print(result)


rsloop.run(main())
```

Use this when you must call blocking Python code without freezing the event loop.

## Run a subprocess

`rsloop` also supports subprocess workflows:

```python
import asyncio
import rsloop
import sys


async def main() -> None:
    proc = await asyncio.create_subprocess_exec(
        sys.executable,
        "-c",
        "print('hello from child')",
        stdout=asyncio.subprocess.PIPE,
    )
    stdout, _ = await proc.communicate()
    print(stdout.decode().strip())


rsloop.run(main())
```

This is useful for scripts, tooling, and service code that needs to call external programs.

## Where to find bigger examples

For fuller examples, read the repository files in `examples/`:

- `01_basics.py`
- `02_fd_and_sockets.py`
- `03_streams.py`
- `04_unix_and_accepted_socket.py`
- `05_pipes_signals_subprocesses.py`
