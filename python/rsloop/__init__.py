from __future__ import annotations

import asyncio as __asyncio
import contextlib as __contextlib
import os as __os
import ssl as __ssl
import sys as __sys
import typing as __typing

from ._loop import PyLoop as Loop
from ._loop import __version__
from ._loop import open_connection as __open_connection
from ._loop import profiler_running as __profiler_running
from ._loop import start_profiler as __start_profiler
from ._loop import start_server as __start_server
from ._loop import stop_profiler as __stop_profiler

__all__: tuple[str, ...] = (
    "Loop",
    "__version__",
    "new_event_loop",
    "profile",
    "profiler_running",
    "run",
    "start_profiler",
    "stop_profiler",
)
_T = __typing.TypeVar("_T")
__ORIG_SET_EVENT_LOOP = __asyncio.set_event_loop


def __set_event_loop(loop: Loop | None) -> None:
    try:
        __ORIG_SET_EVENT_LOOP(loop)
        return
    except (AssertionError, TypeError):
        if loop is None or not isinstance(loop, Loop):
            raise

    # Python 3.8 rejects non-stdlib loop objects in set_event_loop() with a
    # hard isinstance() assertion. Mirror the stdlib policy bookkeeping so
    # get_event_loop() still returns the current rsloop instance.
    policy = __asyncio.get_event_loop_policy()
    local = getattr(policy, "_local", None)
    if local is None:
        raise
    local._set_called = True
    local._loop = loop


if __asyncio.set_event_loop is __ORIG_SET_EVENT_LOOP:
    __asyncio.set_event_loop = __set_event_loop
    __asyncio.events.set_event_loop = __set_event_loop


def new_event_loop() -> Loop:
    return Loop()


def profiler_running() -> bool:
    return __profiler_running()


def start_profiler(*, frequency: int = 999) -> None:
    __start_profiler(frequency=frequency)


def stop_profiler(
    path: str | __os.PathLike[str],
    *,
    format: str = "flamegraph",
) -> str:
    return __stop_profiler(__os.fspath(path), format=format)


@__contextlib.contextmanager
def profile(
    path: str | __os.PathLike[str],
    *,
    frequency: int = 999,
    format: str = "flamegraph",
) -> __typing.Iterator[str]:
    resolved_path = __os.fspath(path)
    start_profiler(frequency=frequency)
    try:
        yield resolved_path
    finally:
        stop_profiler(resolved_path, format=format)


__ORIG_OPEN_CONNECTION = __asyncio.open_connection
__ORIG_START_SERVER = __asyncio.start_server
__USE_FAST_STREAMS = __os.environ.get("RSLOOP_USE_FAST_STREAMS", "1") != "0"


if __USE_FAST_STREAMS and __asyncio.open_connection is __ORIG_OPEN_CONNECTION:
    __asyncio.open_connection = __open_connection
if __USE_FAST_STREAMS and __asyncio.start_server is __ORIG_START_SERVER:
    __asyncio.start_server = __start_server


def __install_ssl_tracking() -> None:
    context_cls = __ssl.SSLContext
    if getattr(context_cls, "_rsloop_tracking_installed", False):
        return

    def mark_default_verify_paths(context):
        context.__dict__["_rsloop_use_default_verify_paths"] = True
        return context

    orig_create_default_context = __ssl.create_default_context
    orig_load_cert_chain = context_cls.load_cert_chain
    orig_load_default_certs = context_cls.load_default_certs
    orig_set_default_verify_paths = context_cls.set_default_verify_paths

    def create_default_context(*args, **kwargs):
        return mark_default_verify_paths(orig_create_default_context(*args, **kwargs))

    def load_cert_chain(self, certfile, keyfile=None, password=None):
        result = orig_load_cert_chain(
            self,
            certfile,
            keyfile=keyfile,
            password=password,
        )
        if callable(password):
            password_value = password()
        else:
            password_value = password
        if isinstance(password_value, str):
            password_value = password_value.encode()
        if password_value is not None and not isinstance(password_value, bytes):
            password_value = bytes(password_value)
        self.__dict__["_rsloop_certfile"] = __os.fspath(certfile)
        self.__dict__["_rsloop_keyfile"] = (
            __os.fspath(keyfile) if keyfile is not None else __os.fspath(certfile)
        )
        self.__dict__["_rsloop_key_password"] = password_value
        return result

    def load_default_certs(self, *args, **kwargs):
        result = orig_load_default_certs(self, *args, **kwargs)
        mark_default_verify_paths(self)
        return result

    def set_default_verify_paths(self):
        result = orig_set_default_verify_paths(self)
        mark_default_verify_paths(self)
        return result

    __ssl.create_default_context = create_default_context
    context_cls.load_cert_chain = load_cert_chain
    context_cls.load_default_certs = load_default_certs
    context_cls.set_default_verify_paths = set_default_verify_paths
    context_cls._rsloop_tracking_installed = True


__install_ssl_tracking()


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
            __asyncio.set_event_loop(None)
            loop.close()
