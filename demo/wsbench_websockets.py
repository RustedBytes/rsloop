"""
uv run --with websockets demo/wsbench_websockets.py
"""

import asyncio
import logging

import rsloop
from websockets import connect


HOST = "127.0.0.1"
PORT = 9001
URL = f"ws://{HOST}:{PORT}/"
USE_RSLOOP = True


async def main():
    async with connect(URL, compression=None, ping_interval=None) as websocket:
        for idx in range(10):
            message = f"hello {idx}"
            await websocket.send(message)
            reply = await websocket.recv()
            print(f"sent: {message} received: {reply}")


if __name__ == "__main__":
    logging.basicConfig(level=logging.DEBUG)
    if USE_RSLOOP:
        rsloop.run(main(), debug=True)
    else:
        asyncio.run(main())
