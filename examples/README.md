# Service Examples

## Granian with rsloop

[`granian_service.py`](./granian_service.py) registers `rsloop` as Granian's
`auto` event-loop builder. Granian calls that builder in every worker, following
its [documented Python customization API](https://github.com/emmett-framework/granian#asyncio-event-loop-initialization).

Build rsloop in release mode, then start the ASGI service:

```bash
uv run --with maturin maturin develop --release
uv run --with granian python examples/granian_service.py --event-loop rsloop
```

Verify the loop selected inside the worker:

```bash
curl http://127.0.0.1:8000/loop
```

For comparison, the same application can run on stdlib asyncio or uvloop:

```bash
uv run --with granian python examples/granian_service.py --event-loop asyncio
uv run --with granian --with uvloop python examples/granian_service.py --event-loop uvloop
```

The service exposes `/`, `/health`, and `/loop` for inspection. Its
`/benchmark` endpoint returns a fixed 10 KiB response used by
[`benches/compare_granian.py`](../benches/compare_granian.py).

## FastAPI with Uvicorn

This example runs the same FastAPI service on three event loops:

- stdlib `asyncio`
- `uvloop`
- `rsloop`

When started with `--event-loop rsloop`, the example explicitly enables
`rsloop` fast streams before importing the loop implementation.

## Prerequisites

If the Rust extension is not already built locally:

```bash
uv run --with maturin maturin develop --release
```

The example uses temporary dependencies from `uv`, so nothing needs to be
added to the project package metadata.

## Run

From the repository root:

```bash
uv run --with fastapi --with uvicorn python examples/fastapi_service.py --event-loop asyncio --no-access-log
uv run --with fastapi --with uvicorn --with uvloop python examples/fastapi_service.py --event-loop uvloop --no-access-log
uv run --with fastapi --with uvicorn python examples/fastapi_service.py --event-loop rsloop --no-access-log
```

Service entrypoint: [`examples/fastapi_service.py`](./fastapi_service.py)

`std-async` is also accepted as an alias for stdlib `asyncio`:

```bash
uv run --with fastapi --with uvicorn --with uvloop python examples/fastapi_service.py --event-loop std-async
```

## Endpoints

- `/` returns the selected loop and basic service info
- `/health` returns a simple readiness payload
- `/sleep?delay=0.05` exercises timer scheduling
- `/fanout?tasks=500&delay=0` exercises concurrent task scheduling
- `/stream-loopback?roundtrips=100&payload_size=256` exercises `asyncio.start_server()` and `asyncio.open_connection()`, which use `rsloop` fast streams in `rsloop` mode

Example:

```bash
curl http://127.0.0.1:8000/
curl "http://127.0.0.1:8000/fanout?tasks=1000&delay=0"
curl "http://127.0.0.1:8000/stream-loopback?roundtrips=200&payload_size=512"
```
