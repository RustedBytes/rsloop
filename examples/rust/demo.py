from __future__ import annotations

import asyncio

import rsloop
import rsloop_rust_example


async def main() -> None:
    loop = asyncio.get_running_loop()
    print("loop:", type(loop).__name__)

    tagged, total = await asyncio.gather(
        rsloop_rust_example.sleep_and_tag("hello from python", 50),
        rsloop_rust_example.race_sum([1, 2, 3, 4]),
    )
    print(tagged)
    print("sum:", total)


if __name__ == "__main__":
    rsloop.run(main())
