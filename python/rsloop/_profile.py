from __future__ import annotations

import contextlib as __contextlib
import typing as __typing

from ._loop import profiler_running as __profiler_running
from ._loop import start_profiler as __start_profiler
from ._loop import stop_profiler as __stop_profiler


def profiler_running() -> bool:
    return __profiler_running()


def start_profiler() -> None:
    """Start a Tracy profiling session."""
    __start_profiler()


def stop_profiler() -> None:
    """Stop the active Tracy profiling session."""
    __stop_profiler()


@__contextlib.contextmanager
def profile() -> __typing.Iterator[None]:
    """Context manager wrapper around ``start_profiler()`` / ``stop_profiler()``."""
    start_profiler()
    try:
        yield None
    finally:
        stop_profiler()
