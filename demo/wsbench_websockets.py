"""
uv run --with websockets demo/wsbench_websockets.py
"""

import asyncio
import logging
import ssl

import rsloop
from websockets import connect


HOST = "127.0.0.1"
PORT = 9001
URL = f"ws://{HOST}:{PORT}/"
USE_RSLOOP = True


async def run_messages(
    url: str = URL,
    *,
    ssl_context: ssl.SSLContext | None = None,
    count: int = 10,
) -> list[tuple[str, str]]:
    replies = []
    async with connect(
        url,
        compression=None,
        ping_interval=None,
        ssl=ssl_context,
    ) as websocket:
        for idx in range(count):
            message = f"hello {idx}"
            await websocket.send(message)
            reply = await websocket.recv()
            replies.append((message, reply))
    return replies


async def main():
    for message, reply in await run_messages():
        print(f"sent: {message} received: {reply}")


if __name__ == "__main__":
    logging.basicConfig(level=logging.DEBUG)
    if USE_RSLOOP:
        rsloop.run(main(), debug=True)
    else:
        asyncio.run(main())
