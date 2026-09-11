from __future__ import annotations

import asyncio
from typing import Dict

import rsloop
from fastapi import FastAPI

from _smoke import run_uvicorn_app


app = FastAPI()


@app.get("/")
async def index() -> Dict[str, str]:
    loop = asyncio.get_running_loop()
    return {
        "ok": "fastapi-rsloop",
        "loop": f"{type(loop).__module__}.{type(loop).__name__}",
    }


async def main() -> None:
    await run_uvicorn_app(app, b"fastapi-rsloop", name="fastapi")


if __name__ == "__main__":
    rsloop.run(main())
