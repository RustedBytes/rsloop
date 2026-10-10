"""Real event loops and bounded teardown for downstream library contracts."""

import asyncio

import pytest
import rsloop


@pytest.fixture(params=["asyncio", "rsloop", "uvloop"])
def loop(request):
    if request.param == "uvloop":
        factory = pytest.importorskip("uvloop").new_event_loop
    elif request.param == "rsloop":
        factory = rsloop.new_event_loop
    else:
        factory = asyncio.new_event_loop
    event_loop = factory()
    errors = []
    event_loop.set_exception_handler(lambda _, context: errors.append(context))
    yield event_loop
    try:
        pending = asyncio.all_tasks(event_loop)
        for task in pending:
            task.cancel()
        if pending:
            event_loop.run_until_complete(
                asyncio.wait_for(asyncio.gather(*pending, return_exceptions=True), 5)
            )
        event_loop.run_until_complete(event_loop.shutdown_asyncgens())
        event_loop.run_until_complete(event_loop.shutdown_default_executor())
        assert not pending, "test leaked tasks"
        assert not errors, errors
    finally:
        event_loop.close()
