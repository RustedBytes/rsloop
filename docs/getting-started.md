# Getting Started

This page covers the parts most Python users will touch first.

## Install

From PyPI:

```bash
pip install rsloop
```

With `uv`:

```bash
uv add rsloop
```

## The public API

The package exports a small public surface:

- `rsloop.Loop`
- `rsloop.new_event_loop()`
- `rsloop.run(...)`
- `rsloop.profile()`
- `rsloop.start_profiler()`
- `rsloop.stop_profiler()`
- `rsloop.profiler_running()`

For most programs, `rsloop.run(...)` is enough.

## Simplest way to use it

```python
import rsloop


async def main() -> str:
    return "done"


result = rsloop.run(main())
print(result)
```

This is similar to `asyncio.run(...)`, but it creates and uses an `rsloop` loop.

## Manual loop creation

Use manual loop creation when you need more control:

```python
import asyncio
import rsloop


loop = rsloop.new_event_loop()
asyncio.set_event_loop(loop)
try:
    loop.run_until_complete(asyncio.sleep(0))
finally:
    asyncio.set_event_loop(None)
    loop.close()
```

## What still feels like normal asyncio?

A lot of the programming model stays the same. For example:

- `async def` coroutines
- `await`
- `asyncio.create_task(...)`
- protocols and transports
- socket helpers such as `sock_recv(...)`
- servers and connections
- subprocess helpers

The big difference is the implementation of the event loop itself.

## Import-time behavior

Importing `rsloop` does a little setup work:

- it boots the native extension
- it patches `asyncio.set_event_loop(...)` for compatibility, especially on older Python versions
- it can patch `asyncio.open_connection(...)` and `asyncio.start_server(...)` to use `rsloop`'s fast stream path

That fast stream behavior is controlled by `RSLOOP_USE_FAST_STREAMS`.

Disable it like this:

```bash
export RSLOOP_USE_FAST_STREAMS=0
```

## Useful examples

The `examples/` directory is the best hands-on tour of the project:

- `examples/01_basics.py`: loop lifecycle, callbacks, tasks, executors
- `examples/02_fd_and_sockets.py`: file descriptor watchers and socket helpers
- `examples/03_streams.py`: TCP protocols, connections, and servers
- `examples/04_unix_and_accepted_socket.py`: Unix sockets and accepted sockets
- `examples/05_pipes_signals_subprocesses.py`: pipes, signals, and subprocesses

If you are new to lower-level `asyncio` features, start with `01_basics.py` and `03_streams.py`.
