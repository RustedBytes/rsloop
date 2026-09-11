from __future__ import annotations

import asyncio
from typing import Dict

import rsloop
from litestar import Litestar
from litestar import get

from _smoke import run_uvicorn_app


@get("/")
async def index() -> Dict[str, str]:
    loop = asyncio.get_running_loop()
    return {
        "ok": "litestar-rsloop",
        "loop": f"{type(loop).__module__}.{type(loop).__name__}",
    }


app = Litestar([index])


async def main() -> None:
    await run_uvicorn_app(app, b"litestar-rsloop", name="litestar")


if __name__ == "__main__":
    rsloop.run(main())
