from __future__ import annotations

import asyncio
from typing import Any

import rsloop
from typing_extensions import assert_type


async def result() -> int:
    return 1


def check_public_api() -> None:
    loop = rsloop.new_event_loop()
    assert_type(loop, rsloop.Loop)
    assert_type(loop.create_future(), asyncio.Future[Any])
    assert_type(loop.create_task(result()), asyncio.Task[int])

    policy = rsloop.EventLoopPolicy()
    assert_type(policy.new_event_loop(), rsloop.Loop)
    assert_type(rsloop.run(result()), int)
    assert_type(rsloop.build_info(), dict[str, str | bool])
    assert_type(rsloop.transport_stats(), dict[str, int | bool])
    assert_type(rsloop.reset_transport_stats(), None)
