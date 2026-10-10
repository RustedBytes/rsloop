"""Extreme finite timeouts must not interrupt an otherwise successful shutdown."""

import asyncio
import math
import sys
import threading
from typing import Any, cast

import pytest
import rsloop


@pytest.mark.parametrize(
    "timeout",
    [math.nextafter(float(2**64), 0.0), float(2**64), sys.float_info.max],
)
def test_large_finite_executor_timeout_completes_without_panic(timeout):
    calls = []

    class Executor:
        def shutdown(self, wait):
            calls.append(wait)

    async def main():
        loop = asyncio.get_running_loop()
        loop.set_default_executor(cast(Any, Executor()))
        await asyncio.wait_for(
            cast(Any, loop.shutdown_default_executor)(timeout=timeout), 2
        )
        with pytest.raises(RuntimeError, match="Executor shutdown has been called"):
            await loop.run_in_executor(None, lambda: None)

    rsloop.run(main())
    assert calls == [True]


def test_oversized_executor_timeout_remains_cancellable():
    release = threading.Event()
    calls = []

    async def main():
        loop = asyncio.get_running_loop()
        started = asyncio.Event()
        finished = asyncio.Event()

        class Executor:
            def shutdown(self, wait):
                calls.append(wait)
                loop.call_soon_threadsafe(started.set)
                release.wait(2)
                loop.call_soon_threadsafe(finished.set)

        loop.set_default_executor(cast(Any, Executor()))
        pending = asyncio.ensure_future(
            cast(Any, loop.shutdown_default_executor)(timeout=sys.float_info.max)
        )
        try:
            await asyncio.wait_for(started.wait(), 2)
            pending.cancel()
            with pytest.raises(asyncio.CancelledError):
                await pending
        finally:
            release.set()
            await asyncio.wait_for(finished.wait(), 2)

    rsloop.run(main())
    assert calls == [True]
