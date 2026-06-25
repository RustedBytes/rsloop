from __future__ import annotations

import asyncio

import rsloop
from hypercorn.asyncio import serve
from hypercorn.config import Config
from quart import Quart
from quart import jsonify

from _smoke import reserve_port
from _smoke import wait_for_http


app = Quart(__name__)


@app.get("/")
async def index() -> object:
    loop = asyncio.get_running_loop()
    return jsonify(
        {
            "ok": "quart-rsloop",
            "loop": f"{type(loop).__module__}.{type(loop).__name__}",
        }
    )


async def main() -> None:
    port = reserve_port()
    shutdown = asyncio.Event()
    config = Config()
    config.bind = [f"127.0.0.1:{port}"]
    config.accesslog = None
    config.errorlog = None

    task = asyncio.create_task(serve(app, config, shutdown_trigger=shutdown.wait))
    try:
        response = await wait_for_http(port)
        assert b"quart-rsloop" in response, response
        assert b"rsloop" in response, response
        print("quart ok")
    finally:
        shutdown.set()
        await task


if __name__ == "__main__":
    rsloop.run(main())
