from __future__ import annotations

import asyncio

from aiohttp import ClientSession
from aiohttp import web
import rsloop

from _smoke import reserve_port


async def index(request: web.Request) -> web.Response:
    loop = asyncio.get_running_loop()
    return web.json_response(
        {
            "ok": "aiohttp-rsloop",
            "loop": f"{type(loop).__module__}.{type(loop).__name__}",
        }
    )


async def main() -> None:
    port = reserve_port()
    app = web.Application()
    app.router.add_get("/", index)
    runner = web.AppRunner(app)

    await runner.setup()
    try:
        site = web.TCPSite(runner, "127.0.0.1", port)
        await site.start()

        async with ClientSession() as session:
            async with session.get(f"http://127.0.0.1:{port}/") as response:
                body = await response.text()

        assert "aiohttp-rsloop" in body, body
        assert "rsloop" in body, body
        print("aiohttp ok")
    finally:
        await runner.cleanup()


if __name__ == "__main__":
    rsloop.run(main())
