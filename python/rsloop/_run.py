from __future__ import annotations

import asyncio as __asyncio
import sys as __sys
import typing as __typing

from ._loop_compat import Loop
from ._loop_compat import cancel_all_tasks as __cancel_all_tasks

_T = __typing.TypeVar("_T")
_PREVIOUS_EVENT_LOOP_POLICY: __typing.Optional[__asyncio.AbstractEventLoopPolicy] = None


def new_event_loop() -> Loop:
    return Loop()


class EventLoopPolicy(__asyncio.DefaultEventLoopPolicy):
    """Event loop policy that creates rsloop loops by default."""

    def new_event_loop(self) -> Loop:
        return new_event_loop()


def install() -> None:
    """Install rsloop as asyncio's default event loop policy."""

    global _PREVIOUS_EVENT_LOOP_POLICY

    policy = __asyncio.get_event_loop_policy()
    if isinstance(policy, EventLoopPolicy):
        return

    _PREVIOUS_EVENT_LOOP_POLICY = policy
    __asyncio.set_event_loop_policy(EventLoopPolicy())


def uninstall() -> None:
    """Restore the event loop policy that was active before install()."""

    global _PREVIOUS_EVENT_LOOP_POLICY

    if _PREVIOUS_EVENT_LOOP_POLICY is None:
        return

    if isinstance(__asyncio.get_event_loop_policy(), EventLoopPolicy):
        __asyncio.set_event_loop_policy(_PREVIOUS_EVENT_LOOP_POLICY)
    _PREVIOUS_EVENT_LOOP_POLICY = None


if __typing.TYPE_CHECKING:

    def run(
        main: __typing.Coroutine[__typing.Any, __typing.Any, _T],
        *,
        loop_factory: __typing.Callable[[], Loop] = new_event_loop,
        debug: bool | None = None,
    ) -> _T: ...
else:

    def run(main, *, loop_factory=new_event_loop, debug=None, **run_kwargs):
        async def wrapper():
            loop = __asyncio._get_running_loop()
            if not isinstance(loop, Loop):
                raise TypeError("rsloop.run() uses a non-rsloop loop")
            return await main

        if __sys.version_info[:2] >= (3, 12):
            return __asyncio.run(
                wrapper(),
                loop_factory=loop_factory,
                debug=debug,
                **run_kwargs,
            )

        if __asyncio._get_running_loop() is not None:
            raise RuntimeError(
                "asyncio.run() cannot be called from a running event loop"
            )

        if not __asyncio.iscoroutine(main):
            raise ValueError(f"a coroutine was expected, got {main!r}")

        loop = loop_factory()
        try:
            __asyncio.set_event_loop(loop)
            if debug is not None:
                loop.set_debug(debug)
            return loop.run_until_complete(wrapper())
        finally:
            try:
                __cancel_all_tasks(loop)
                loop.run_until_complete(loop.shutdown_asyncgens())
                shutdown_default_executor = getattr(
                    loop,
                    "shutdown_default_executor",
                    None,
                )
                if shutdown_default_executor is not None:
                    loop.run_until_complete(shutdown_default_executor())
            finally:
                __asyncio.set_event_loop(None)
                loop.close()
