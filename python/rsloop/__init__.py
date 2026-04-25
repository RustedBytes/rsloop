from __future__ import annotations

from ._bootstrap import bootstrap as __bootstrap

__bootstrap()

from ._loop_compat import Loop
from ._loop_compat import __version__
from ._profile import profile
from ._profile import profiler_running
from ._profile import start_profiler
from ._profile import stop_profiler
from ._run import EventLoopPolicy
from ._run import install
from ._run import new_event_loop
from ._run import run
from ._run import uninstall

__all__: tuple[str, ...] = (
    "EventLoopPolicy",
    "Loop",
    "__version__",
    "install",
    "new_event_loop",
    "profile",
    "profiler_running",
    "run",
    "start_profiler",
    "stop_profiler",
    "uninstall",
)
