"""AnyIO on rsloop.

AnyIO's asyncio backend drives the running loop directly rather than through
`asyncio`'s high-level helpers, so it reaches loop APIs the other package tests
here do not: `subprocess_exec` with the full `Popen` keyword set, socket
creation, `run_in_executor`, and cancellation. Issue #68 was a keyword rejection
on that subprocess path that only AnyIO's test suite noticed, so each surface
below is checked explicitly instead of relying on one end-to-end request.
"""

from __future__ import annotations

import asyncio
import socket
import sys
from typing import Any

import anyio
import anyio.to_thread
import rsloop
from _smoke import reserve_port
from anyio.abc import SocketStream

CHILD_ARGS = (sys.executable, "-c", "print('anyio-subprocess-ok')")


async def check_loop_is_rsloop() -> None:
    loop = asyncio.get_running_loop()
    name = f"{type(loop).__module__}.{type(loop).__name__}"
    assert "rsloop" in name, name


async def check_run_process() -> None:
    """`anyio.run_process` forwards every documented `Popen` keyword.

    Regression cover for issue #68: `startupinfo` and `creationflags` are passed
    unconditionally by AnyIO, so rejecting them at their defaults broke this.
    """
    result = await anyio.run_process(list(CHILD_ARGS))
    assert result.returncode == 0, result.returncode
    assert b"anyio-subprocess-ok" in result.stdout, result.stdout


async def check_open_process() -> None:
    async with await anyio.open_process(list(CHILD_ARGS)) as process:
        assert process.stdout is not None
        chunks = bytearray()
        async for chunk in process.stdout:
            chunks.extend(chunk)
        await process.wait()
    assert process.returncode == 0, process.returncode
    assert b"anyio-subprocess-ok" in chunks, bytes(chunks)


async def check_tcp_round_trip() -> None:
    port = reserve_port()

    async def serve(listener: Any) -> None:
        async def handle(stream: SocketStream) -> None:
            async with stream:
                await stream.send(await stream.receive())

        await listener.serve(handle)

    listener = await anyio.create_tcp_listener(local_host="127.0.0.1", local_port=port)
    async with anyio.create_task_group() as task_group:
        task_group.start_soon(serve, listener)
        async with await anyio.connect_tcp("127.0.0.1", port) as stream:
            await stream.send(b"anyio-stream-ok")
            assert await stream.receive() == b"anyio-stream-ok"
        task_group.cancel_scope.cancel()


async def check_concurrent_send_backpressure() -> None:
    """A full Windows send buffer must not block the event-loop thread."""
    server = socket.create_server(("127.0.0.1", 0))

    async def send_forever(stream: SocketStream) -> None:
        while True:
            await stream.send(b"\0" * 4096)

    try:
        host, port = server.getsockname()[:2]
        async with (
            await anyio.connect_tcp(host, port) as stream,
            anyio.create_task_group() as task_group,
        ):
            task_group.start_soon(send_forever, stream)
            await anyio.wait_all_tasks_blocked()
            try:
                await stream.send(b"concurrent")
            except anyio.BusyResourceError:
                pass
            else:
                raise AssertionError("concurrent send did not raise BusyResourceError")
            task_group.cancel_scope.cancel()
    finally:
        server.close()


async def check_task_group_and_cancellation() -> None:
    finished: list[int] = []

    async def child(index: int) -> None:
        await anyio.sleep(0.01)
        finished.append(index)

    async with anyio.create_task_group() as task_group:
        for index in range(8):
            task_group.start_soon(child, index)
    assert sorted(finished) == list(range(8)), finished

    # A cancel scope that fires has to unwind cleanly on rsloop's timers.
    with anyio.move_on_after(0.05) as scope:
        await anyio.sleep(30)
    assert scope.cancelled_caught


async def check_to_thread() -> None:
    assert (
        await anyio.to_thread.run_sync(lambda: "anyio-thread-ok") == "anyio-thread-ok"
    )


async def main() -> None:
    for check in (
        check_loop_is_rsloop,
        check_run_process,
        check_open_process,
        check_tcp_round_trip,
        check_concurrent_send_backpressure,
        check_task_group_and_cancellation,
        check_to_thread,
    ):
        with anyio.fail_after(30):
            await check()

    print("anyio ok")


if __name__ == "__main__":
    rsloop.run(main())
