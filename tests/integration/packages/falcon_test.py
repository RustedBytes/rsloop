from __future__ import annotations

import asyncio
from typing import Any

import falcon.asgi
import rsloop

from _smoke import run_uvicorn_app


class Resource:
    async def on_get(self, req: Any, resp: Any) -> None:
        loop = asyncio.get_running_loop()
        resp.media = {
            "ok": "falcon-rsloop",
            "loop": f"{type(loop).__module__}.{type(loop).__name__}",
        }


app = falcon.asgi.App()
app.add_route("/", Resource())


async def main() -> None:
    await run_uvicorn_app(
        app,
        b"falcon-rsloop",
        name="falcon",
        lifespan="off",
    )


if __name__ == "__main__":
    rsloop.run(main())
