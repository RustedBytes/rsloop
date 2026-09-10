from asyncio import DefaultEventLoopPolicy
from collections.abc import Callable, Coroutine
from typing import Any, TypeVar

from ._loop import PyLoop

_T = TypeVar("_T")

Loop = PyLoop

__version__: str

def build_info() -> dict[str, str | bool]: ...
def transport_stats() -> dict[str, int | bool]: ...
def reset_transport_stats() -> None: ...

class EventLoopPolicy(DefaultEventLoopPolicy):
    def new_event_loop(self) -> Loop: ...

def install() -> None: ...
def uninstall() -> None: ...
def new_event_loop() -> Loop: ...
def run(
    main: Coroutine[Any, Any, _T],
    *,
    loop_factory: Callable[[], Loop] = ...,
    debug: bool | None = ...,
) -> _T: ...
__all__: tuple[str, ...] = (
    "EventLoopPolicy",
    "Loop",
    "__version__",
    "build_info",
    "install",
    "new_event_loop",
    "reset_transport_stats",
    "run",
    "transport_stats",
    "uninstall",
)
