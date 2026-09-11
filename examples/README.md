# rsloop Examples

The examples are ordered from event-loop fundamentals to integrations. Run all
commands from the repository root unless a section says otherwise.

## Setup

Build the local extension before running an example:

```bash
uv run --with maturin maturin develop --release
```

## Guided feature tour

| Example | Demonstrates |
| --- | --- |
| [`01_basics.py`](./01_basics.py) | `run_forever`, callbacks, futures, threadsafe scheduling, tasks, and executors |
| [`02_fd_and_sockets.py`](./02_fd_and_sockets.py) | File-descriptor readiness and low-level asynchronous socket methods |
| [`03_streams.py`](./03_streams.py) | TCP servers and clients built with asyncio protocols and transports |
| [`04_unix_and_accepted_socket.py`](./04_unix_and_accepted_socket.py) | Unix-domain streams and `connect_accepted_socket` |
| [`05_pipes_signals_subprocesses.py`](./05_pipes_signals_subprocesses.py) | Pipes, Unix signals, and low- and high-level subprocess APIs |

Run any example directly:

```bash
uv run python examples/01_basics.py
uv run python examples/02_fd_and_sockets.py
uv run python examples/03_streams.py
uv run python examples/04_unix_and_accepted_socket.py
uv run python examples/05_pipes_signals_subprocesses.py
```

Unix-domain sockets and POSIX signal handlers report that they are skipped on
Windows. The other demonstrations in those files still run.

## Web services

### FastAPI with Uvicorn

[`fastapi_service.py`](./fastapi_service.py) runs one FastAPI application on
stdlib asyncio, uvloop, winloop, or rsloop. In rsloop mode it enables rsloop's
fast streams before constructing the event loop.

```bash
uv run --with fastapi --with uvicorn python examples/fastapi_service.py --event-loop asyncio --no-access-log
uv run --with fastapi --with uvicorn --with uvloop python examples/fastapi_service.py --event-loop uvloop --no-access-log
uv run --with fastapi --with uvicorn python examples/fastapi_service.py --event-loop rsloop --no-access-log
```

On Windows, winloop is another available comparison:

```bash
uv run --with fastapi --with uvicorn --with winloop python examples/fastapi_service.py --event-loop winloop --no-access-log
```

The service exposes:

- `/` and `/health` for service and loop information
- `/sleep?delay=0.05` for timer scheduling
- `/fanout?tasks=500&delay=0` for concurrent task scheduling
- `/stream-loopback?roundtrips=100&payload_size=256` for stream I/O

`std-async` is accepted as an alias for `asyncio`.

### Granian

[`granian_service.py`](./granian_service.py) registers rsloop through Granian's
custom event-loop builder and creates the selected loop in each worker.

```bash
uv run --with granian python examples/granian_service.py --event-loop rsloop
uv run --with granian python examples/granian_service.py --event-loop asyncio
uv run --with granian --with uvloop python examples/granian_service.py --event-loop uvloop
```

The service exposes `/`, `/health`, and `/loop`. Its `/benchmark` endpoint
returns the fixed 10 KiB response used by
[`benches/compare_granian.py`](../benches/compare_granian.py).

## WebSocket clients and server

Start the Picows echo server, then run either client in another terminal:

```bash
uv run --with picows python examples/picows_server.py
uv run --with picows python examples/picows_test.py
uv run --with websockets python examples/wsbench_websockets.py
```

All three scripts use `127.0.0.1:9001` by default.

## Rust extension

The [`rust`](./rust/) directory contains a separate PyO3 extension that turns
Rust futures into Python awaitables using `rsloop::rust_async::future_into_py`.
See its [README](./rust/README.md) for build and run instructions.
