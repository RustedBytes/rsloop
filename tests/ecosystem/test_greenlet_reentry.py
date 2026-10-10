"""Exercise greenlet stack copying in a subprocess: regressions can segfault."""

import asyncio
import subprocess
import sys
from pathlib import Path
from typing import cast

import pytest
import rsloop

pytestmark = pytest.mark.ecosystem


def _exercise(depth, bridge):
    import greenlet

    class AsyncGreenlet(greenlet.greenlet):
        def __init__(self, fn, driver):
            super().__init__(fn, driver)
            self.driver = driver

    def await_only(awaitable):
        return cast(AsyncGreenlet, greenlet.getcurrent()).driver.switch(awaitable)

    async def spawn(fn):
        child = AsyncGreenlet(fn, greenlet.getcurrent())
        result = child.switch()
        while not child.dead:
            try:
                value = await result
            except BaseException as exc:  # noqa: BLE001 - forward cancellation into the greenlet
                result = child.throw(type(exc), exc, exc.__traceback__)
            else:
                result = child.switch(value)
        return result

    if bridge == "sqlalchemy":
        from sqlalchemy.util.concurrency import await_only, greenlet_spawn

        spawn = greenlet_spawn

    lock = asyncio.Lock()
    progress = [0, 0]

    def critical_section():
        await_only(lock.acquire())
        try:
            await_only(asyncio.sleep(0.001))
        finally:
            lock.release()

    async def worker(index):
        while True:
            await spawn(critical_section)
            progress[index] += 1
            await asyncio.sleep(0)

    async def start():
        return [asyncio.create_task(worker(i)) for i in range(2)]

    def deeper(n, fn):
        # map.__next__ deliberately adds C frames; a generator changes the repro.
        return fn() if n == 0 else next(map(lambda _: deeper(n - 1, fn), [None]))  # noqa: C417

    loop = rsloop.new_event_loop()
    asyncio.set_event_loop(loop)
    tasks = loop.run_until_complete(start())
    deeper(depth, lambda: loop.run_until_complete(asyncio.sleep(0.15)))
    assert min(progress) > 1, progress
    for task in tasks:
        task.cancel()
    loop.run_until_complete(
        asyncio.wait_for(asyncio.gather(*tasks, return_exceptions=True), 2)
    )
    assert all(task.cancelled() for task in tasks)
    loop.close()
    asyncio.set_event_loop(None)


@pytest.mark.parametrize("depth", [0, 2, 4, 8, 50, 100])
@pytest.mark.parametrize("bridge", ["minimal", "sqlalchemy"])
def test_greenlet_wakeup_after_deeper_loop_reentry(depth, bridge):
    pytest.importorskip("greenlet")
    if bridge == "sqlalchemy":
        pytest.importorskip("sqlalchemy")
    script = (
        f"import runpy; runpy.run_path({str(Path(__file__))!r})"
        f"['_exercise']({depth}, {bridge!r})"
    )
    result = subprocess.run(
        [sys.executable, "-X", "faulthandler", "-c", script],
        capture_output=True,
        text=True,
        timeout=10,
        check=False,
    )
    assert result.returncode == 0, result.stdout + result.stderr
