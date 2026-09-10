from __future__ import annotations

from ._bootstrap import bootstrap as __bootstrap

__bootstrap()

from ._loop_compat import (
    Loop,
    __version__,
    build_info,
    reset_transport_stats,
    transport_stats,
)
from ._run import EventLoopPolicy, install, new_event_loop, run, uninstall

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
