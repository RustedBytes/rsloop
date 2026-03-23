# FastAPI Demo

This demo runs the same FastAPI service on three event loops:

- stdlib `asyncio`
- `uvloop`
- `rsloop`

When started with `--event-loop rsloop`, the demo explicitly enables
`rsloop` fast streams before importing the loop implementation.

## Prerequisites

If the Rust extension is not already built locally:

```bash
uv run --with maturin maturin develop --release
```

The demo itself uses temporary dependencies from `uv`, so nothing needs to be
added to the project package metadata.

## Run

From the repository root:

```bash
uv run --with fastapi --with uvicorn --with uvloop python demo/fastapi_service.py --event-loop asyncio
uv run --with fastapi --with uvicorn --with uvloop python demo/fastapi_service.py --event-loop uvloop
uv run --with fastapi --with uvicorn --with uvloop python demo/fastapi_service.py --event-loop rsloop
```

Service entrypoint: [`demo/fastapi_service.py`](./fastapi_service.py)

`std-async` is also accepted as an alias for stdlib `asyncio`:

```bash
uv run --with fastapi --with uvicorn --with uvloop python demo/fastapi_service.py --event-loop std-async
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
