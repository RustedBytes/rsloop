from __future__ import annotations

import asyncio

import rsloop
from rsloop_rust_example import sleep_and_tag, race_sum


async def main() -> None:
    loop = asyncio.get_running_loop()
    print("loop:", type(loop).__name__)

    tagged, total = await asyncio.gather(
        sleep_and_tag("hello from python", 500),
        race_sum([1, 2, 3, 4]),
    )
    print(tagged)
    print("sum:", total)


if __name__ == "__main__":
    rsloop.run(main())
