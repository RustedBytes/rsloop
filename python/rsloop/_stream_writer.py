"""Cold-path orchestration for native stream writer shutdown."""

import asyncio


async def drain_closing(protocol):
    # StreamWriter.drain() yields on a closing transport so its queued
    # connection_lost callback can make the drain fail instead of busy-looping.
    await asyncio.sleep(0)
    await protocol._drain_helper()
