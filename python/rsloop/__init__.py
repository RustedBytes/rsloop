from __future__ import annotations

import asyncio as __asyncio
import builtins as __builtins
import collections as __collections
import contextlib as __contextlib
import itertools as __itertools
import math as __math
import os as __os
import socket as __socket
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
__ORIG_CREATE_CONNECTION = Loop.create_connection
__USE_FAST_STREAMS = __os.environ.get("RSLOOP_USE_FAST_STREAMS", "1") != "0"


if __USE_FAST_STREAMS and __asyncio.open_connection is __ORIG_OPEN_CONNECTION:
    __asyncio.open_connection = __open_connection
if __USE_FAST_STREAMS and __asyncio.start_server is __ORIG_START_SERVER:
    __asyncio.start_server = __start_server


def __interleave_addrinfos(addrinfos, first_address_family_count=1):
    grouped = __collections.OrderedDict()
    for addrinfo in addrinfos:
        grouped.setdefault(addrinfo[0], []).append(addrinfo)

    lists = list(grouped.values())
    reordered = []
    if first_address_family_count > 1 and lists:
        reordered.extend(lists[0][: first_address_family_count - 1])
        del lists[0][: first_address_family_count - 1]

    for addrinfo in __itertools.zip_longest(*lists):
        reordered.extend(item for item in addrinfo if item is not None)
    return reordered


def __flatten_connection_exceptions(exceptions):
    return [exc for group in exceptions for exc in group]


def __raise_connection_error(exceptions, *, all_errors):
    if all_errors and hasattr(__builtins, "ExceptionGroup"):
        raise __builtins.ExceptionGroup("create_connection failed", exceptions)
    if len(exceptions) == 1:
        raise exceptions[0]

    model = str(exceptions[0])
    if all(str(exc) == model for exc in exceptions):
        raise exceptions[0]

    raise OSError("Multiple exceptions: " + ", ".join(str(exc) for exc in exceptions))


def __bind_error(address, exc):
    detail = exc.strerror.lower() if exc.strerror else str(exc)
    return OSError(
        exc.errno,
        f"error while attempting to bind on address {address!r}: {detail}",
    )


def __prepare_stream_socket(addrinfo, local_addrinfos):
    family, socktype, proto, _, address = addrinfo
    sock = __socket.socket(family=family, type=socktype, proto=proto)
    sock.setblocking(False)
    attempt_exceptions = []
    if local_addrinfos is None:
        return sock, address, attempt_exceptions

    for local_family, _, _, _, local_address in local_addrinfos:
        if local_family != family:
            continue
        try:
            sock.bind(local_address)
            return sock, address, attempt_exceptions
        except OSError as exc:
            attempt_exceptions.append(__bind_error(local_address, exc))

    sock.close()
    if attempt_exceptions:
        return None, None, attempt_exceptions
    return (
        None,
        None,
        [OSError(f"no matching local address with family={family} found")],
    )


def __consume_connection_attempts(done, pending, exceptions):
    for task in done:
        sock, attempt_exceptions = pending.pop(task)
        try:
            task.result()
        except OSError as exc:
            attempt_exceptions.append(exc)
            exceptions.append(attempt_exceptions)
            sock.close()
            continue
        except BaseException:
            sock.close()
            raise
        return sock
    return None


async def __connect_with_happy_eyeballs(
    loop,
    addrinfos,
    local_addrinfos,
    delay,
):
    exceptions = []
    pending = {}
    if not __math.isfinite(delay) or delay <= 0:
        delay = 0.0

    for index, addrinfo in enumerate(addrinfos):
        sock, address, attempt_exceptions = __prepare_stream_socket(
            addrinfo, local_addrinfos
        )
        if sock is None:
            exceptions.append(attempt_exceptions)
        else:
            pending[__asyncio.create_task(loop.sock_connect(sock, address))] = (
                sock,
                attempt_exceptions,
            )

        if not pending:
            continue
        if index + 1 >= len(addrinfos):
            continue

        done, _ = await __asyncio.wait(
            tuple(pending),
            timeout=delay,
            return_when=__asyncio.FIRST_COMPLETED,
        )
        winner = __consume_connection_attempts(done, pending, exceptions)
        if winner is not None:
            return winner, pending, exceptions

    while pending:
        done, _ = await __asyncio.wait(
            tuple(pending),
            return_when=__asyncio.FIRST_COMPLETED,
        )
        winner = __consume_connection_attempts(done, pending, exceptions)
        if winner is not None:
            return winner, pending, exceptions

    return None, pending, exceptions


async def __loop_create_connection(
    self,
    protocol_factory,
    host=None,
    port=None,
    *,
    ssl=None,
    family=0,
    proto=0,
    flags=0,
    sock=None,
    local_addr=None,
    server_hostname=None,
    ssl_handshake_timeout=None,
    ssl_shutdown_timeout=None,
    happy_eyeballs_delay=None,
    interleave=None,
    all_errors=False,
):
    if server_hostname is not None and not ssl:
        raise ValueError("server_hostname is only meaningful with ssl")
    if server_hostname is None and ssl:
        if not host:
            raise ValueError(
                "You must set server_hostname when using ssl without a host"
            )
        server_hostname = host
    if ssl_handshake_timeout is not None and not ssl:
        raise ValueError("ssl_handshake_timeout is only meaningful with ssl")
    if ssl_shutdown_timeout is not None and not ssl:
        raise ValueError("ssl_shutdown_timeout is only meaningful with ssl")
    if happy_eyeballs_delay is not None and interleave is None:
        interleave = 1

    created_sock = None
    if host is not None or port is not None:
        if sock is not None:
            raise ValueError("host/port and sock can not be specified at the same time")

        addrinfos = await self.getaddrinfo(
            host,
            port,
            family=family,
            type=__socket.SOCK_STREAM,
            proto=proto,
            flags=flags,
        )
        if not addrinfos:
            raise OSError("getaddrinfo() returned empty list")

        if local_addr is not None:
            local_addrinfos = await self.getaddrinfo(
                *local_addr,
                family=family,
                type=__socket.SOCK_STREAM,
                proto=proto,
                flags=flags,
            )
            if not local_addrinfos:
                raise OSError("getaddrinfo() returned empty list")
        else:
            local_addrinfos = None

        if interleave:
            addrinfos = __interleave_addrinfos(addrinfos, interleave)

        if happy_eyeballs_delay is None:
            connection_exceptions = []
            for addrinfo in addrinfos:
                created_sock, address, attempt_exceptions = __prepare_stream_socket(
                    addrinfo, local_addrinfos
                )
                if created_sock is None:
                    connection_exceptions.append(attempt_exceptions)
                    continue
                try:
                    await self.sock_connect(created_sock, address)
                    break
                except OSError as exc:
                    attempt_exceptions.append(exc)
                    connection_exceptions.append(attempt_exceptions)
                    created_sock.close()
                    created_sock = None
                except BaseException:
                    created_sock.close()
                    raise
            if created_sock is None:
                __raise_connection_error(
                    __flatten_connection_exceptions(connection_exceptions),
                    all_errors=all_errors,
                )
        else:
            (
                created_sock,
                pending,
                connection_exceptions,
            ) = await __connect_with_happy_eyeballs(
                self,
                addrinfos,
                local_addrinfos,
                happy_eyeballs_delay,
            )
            for task, (pending_sock, _) in pending.items():
                task.cancel()
                pending_sock.close()
            if pending:
                await __asyncio.gather(*pending, return_exceptions=True)
            if created_sock is None:
                __raise_connection_error(
                    __flatten_connection_exceptions(connection_exceptions),
                    all_errors=all_errors,
                )

        sock = created_sock
    elif sock is None:
        raise ValueError("host and port was not specified and no sock specified")

    try:
        return await __ORIG_CREATE_CONNECTION(
            self,
            protocol_factory,
            ssl=ssl,
            family=family,
            proto=proto,
            flags=flags,
            sock=sock,
            server_hostname=server_hostname,
            ssl_handshake_timeout=ssl_handshake_timeout,
            ssl_shutdown_timeout=ssl_shutdown_timeout,
        )
    except BaseException:
        if created_sock is not None:
            created_sock.close()
        raise


if Loop.create_connection is __ORIG_CREATE_CONNECTION:
    Loop.create_connection = __loop_create_connection


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
