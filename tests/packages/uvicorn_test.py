from __future__ import annotations

from typing import Any

import rsloop

from _smoke import run_uvicorn_app


async def app(scope: dict[str, Any], receive: Any, send: Any) -> None:
    if scope["type"] == "lifespan":
        while True:
            message = await receive()
            if message["type"] == "lifespan.startup":
                await send({"type": "lifespan.startup.complete"})
            elif message["type"] == "lifespan.shutdown":
                await send({"type": "lifespan.shutdown.complete"})
                return

    assert scope["type"] == "http"
    await send(
        {
            "type": "http.response.start",
            "status": 200,
            "headers": [(b"content-type", b"text/plain")],
        }
    )
    await send({"type": "http.response.body", "body": b"uvicorn-rsloop"})


async def main() -> None:
    await run_uvicorn_app(app, b"uvicorn-rsloop", name="uvicorn")


if __name__ == "__main__":
    rsloop.run(main())
