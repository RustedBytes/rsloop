from __future__ import annotations

import asyncio

import rsloop
from starlette.applications import Starlette
from starlette.requests import Request
from starlette.responses import JSONResponse
from starlette.routing import Route

from _smoke import run_uvicorn_app


async def index(request: Request) -> JSONResponse:
    loop = asyncio.get_running_loop()
    return JSONResponse(
        {
            "ok": "starlette-rsloop",
            "loop": f"{type(loop).__module__}.{type(loop).__name__}",
        }
    )


app = Starlette(routes=[Route("/", index)])


async def main() -> None:
    await run_uvicorn_app(app, b"starlette-rsloop", name="starlette")


if __name__ == "__main__":
    rsloop.run(main())
