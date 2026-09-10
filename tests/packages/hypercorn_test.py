from __future__ import annotations

import asyncio
from typing import Any, cast

import rsloop
from _smoke import reserve_port, wait_for_http
from hypercorn.asyncio import serve
from hypercorn.config import Config


async def app(scope: dict[str, Any], receive: Any, send: Any) -> None:
    assert scope["type"] == "http"
    loop_name = f"{type(asyncio.get_running_loop()).__module__}"
    await send(
        {
            "type": "http.response.start",
            "status": 200,
            "headers": [(b"content-type", b"text/plain")],
        }
    )
    await send(
        {
            "type": "http.response.body",
            "body": f"hypercorn-{loop_name}".encode(),
        }
    )


async def main() -> None:
    port = reserve_port()
    shutdown = asyncio.Event()
    config = Config()
    config.bind = [f"127.0.0.1:{port}"]
    config.accesslog = None
    config.errorlog = None

    task = asyncio.create_task(
        serve(cast(Any, app), config, shutdown_trigger=shutdown.wait)
    )
    try:
        response = await wait_for_http(port)
        assert b"hypercorn-rsloop" in response, response
        print("hypercorn ok")
    finally:
        shutdown.set()
        await task


if __name__ == "__main__":
    rsloop.run(main())
