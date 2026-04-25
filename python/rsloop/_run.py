from __future__ import annotations

import asyncio as __asyncio
import signal as __signal
import sys as __sys
import threading as __threading
import typing as __typing

from ._loop_compat import Loop
from ._loop_compat import cancel_all_tasks as __cancel_all_tasks

_T = __typing.TypeVar("_T")
_PREVIOUS_EVENT_LOOP_POLICY: __typing.Optional[__asyncio.AbstractEventLoopPolicy] = None


def _noop() -> None:
    pass


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


class _SigintHandler:
    def __init__(self, loop: Loop, main_task: __asyncio.Task[__typing.Any]) -> None:
        self._loop = loop
        self._main_task = main_task
        self.interrupt_count = 0

    def __call__(
        self,
        signum: int,
        frame: __typing.Optional[__typing.Any],
    ) -> None:
        self.interrupt_count += 1
        if self.interrupt_count == 1 and not self._main_task.done():
            self._main_task.cancel()
            self._loop.call_soon_threadsafe(_noop)
            return
        raise KeyboardInterrupt()


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
            main_task = loop.create_task(wrapper())
            sigint_handler = None
            if (
                __threading.current_thread() is __threading.main_thread()
                and __signal.getsignal(__signal.SIGINT) is __signal.default_int_handler
            ):
                sigint_handler = _SigintHandler(loop, main_task)
                try:
                    __signal.signal(__signal.SIGINT, sigint_handler)
                except ValueError:
                    sigint_handler = None
            try:
                return loop.run_until_complete(main_task)
            except __asyncio.CancelledError:
                if sigint_handler is not None and sigint_handler.interrupt_count > 0:
                    uncancel = getattr(main_task, "uncancel", None)
                    if uncancel is None or uncancel() == 0:
                        raise KeyboardInterrupt()
                raise
            finally:
                if (
                    sigint_handler is not None
                    and __signal.getsignal(__signal.SIGINT) is sigint_handler
                ):
                    __signal.signal(__signal.SIGINT, __signal.default_int_handler)
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
