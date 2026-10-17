"""Native name resolution: same rows and errors as socket.getaddrinfo, without the GIL."""

import _socket
import asyncio
import os
import socket
import subprocess
import sys
import textwrap
from typing import Any, cast

import pytest
import rsloop

pytestmark = pytest.mark.skipif(
    sys.platform == "win32", reason="the native resolver is Unix-only"
)

STREAM = {"type": socket.SOCK_STREAM}


def _resolve(host, port, **kwargs):
    async def main():
        return await asyncio.get_running_loop().getaddrinfo(host, port, **kwargs)

    return rsloop.run(main())


def _outcome(call):
    try:
        return "ok", call()
    except Exception as error:  # noqa: BLE001 - compared below
        return type(error), error.args


@pytest.mark.parametrize(
    "host,port,kwargs",
    [
        ("localhost", 80, STREAM),
        ("localhost", "80", STREAM),
        ("localhost", b"80", STREAM),
        ("localhost", "http", STREAM),
        (b"localhost", 80, STREAM),
        ("localhost.", 80, STREAM),
        ("localhost", None, {}),
        ("localhost", 80, {}),
        ("localhost", 0, {"type": socket.SOCK_DGRAM}),
        ("localhost", 65535, {"family": socket.AF_INET, **STREAM}),
        ("localhost", 80, {"flags": socket.AI_CANONNAME, **STREAM}),
        (None, 80, {"flags": socket.AI_PASSIVE}),
        (None, 80, STREAM),
        ("::1%lo", 80, STREAM),
        ("localhost", 80, {"proto": socket.IPPROTO_UDP, **STREAM}),
        # Errors: gaierror codes and messages as socket.getaddrinfo raises them.
        ("localhost", "no-such-service-rsloop", STREAM),
        ("localhost", 80, {"flags": socket.AI_NUMERICHOST, **STREAM}),
        (None, None, {}),
        # Shapes the native path leaves to CPython's own conversion.
        ("a..b", 80, STREAM),
        ("x" * 64, 80, STREAM),
        ("localhost", -1, STREAM),
        ("localhost", 2**70, STREAM),
        ("localhost", 1.5, STREAM),
        ("localhost", True, STREAM),
        ("local\x00host", 80, STREAM),
    ],
)
def test_results_and_errors_match_socket_getaddrinfo(host, port, kwargs):
    if host == "::1%lo" and not socket.has_ipv6:
        pytest.skip("IPv6 is unavailable")
    expected = _outcome(lambda: socket.getaddrinfo(host, port, **kwargs))
    actual = _outcome(lambda: _resolve(host, port, **kwargs))
    assert actual == expected
    if expected[0] == "ok":
        # The enum members, not only equal ints.
        assert [tuple(map(type, row)) for row in actual[1]] == [
            tuple(map(type, row)) for row in expected[1]
        ]


def test_lookups_run_concurrently_and_keep_their_own_results():
    async def main():
        loop = asyncio.get_running_loop()
        ports = list(range(1000, 1064))
        results = await asyncio.gather(
            *(loop.getaddrinfo("localhost", port, **STREAM) for port in ports)
        )
        for port, rows in zip(ports, results, strict=True):
            assert rows == socket.getaddrinfo("localhost", port, **STREAM)

    rsloop.run(main())


def test_a_cancelled_lookup_leaves_the_loop_running():
    async def main():
        loop = asyncio.get_running_loop()
        # rsloop returns a Future (the asyncio stubs say coroutine).
        getaddrinfo = cast(Any, loop.getaddrinfo)
        lookups = [getaddrinfo("localhost", 80, **STREAM) for _ in range(32)]
        for lookup in lookups:
            lookup.cancel()
        # Completions of the cancelled lookups arrive while this one waits.
        assert await loop.getaddrinfo("localhost", 81, **STREAM)
        await asyncio.sleep(0.05)
        return [lookup.cancelled() for lookup in lookups]

    assert rsloop.run(main()) == [True] * 32


def test_a_lookup_started_before_run_completes_in_the_next_run():
    loop = rsloop.new_event_loop()
    try:
        lookup = loop.getaddrinfo("localhost", 80, **STREAM)
        rows = loop.run_until_complete(lookup)
        assert rows == socket.getaddrinfo("localhost", 80, **STREAM)
    finally:
        loop.close()


def _run_script(source):
    result = subprocess.run(
        [sys.executable, "-c", textwrap.dedent(source)],
        capture_output=True,
        text=True,
        timeout=60,
        check=False,
    )
    assert result.returncode == 0, result.stdout + result.stderr
    return result.stdout


def test_audit_event_is_raised_on_the_loop_thread_and_can_refuse():
    # Audit hooks cannot be removed, so this runs in its own interpreter.
    output = _run_script(
        """
        import asyncio, socket, sys, threading
        import rsloop

        events = []
        refuse = []

        def hook(event, args):
            if event == "socket.getaddrinfo":
                events.append((args, threading.get_ident()))
                if refuse:
                    raise PermissionError("refused by audit hook")

        sys.addaudithook(hook)

        async def main():
            loop = asyncio.get_running_loop()
            await loop.getaddrinfo("localhost", 80, type=socket.SOCK_STREAM)
            refuse.append(True)
            try:
                await loop.getaddrinfo("localhost", "http", type=socket.SOCK_STREAM)
            except PermissionError as error:
                print("refused:", error)

        loop_thread = threading.get_ident()
        rsloop.run(main())
        print(events[0][0])
        # Native: raised by the loop thread, not by an executor worker.
        print("loop thread:", events[0][1] == loop_thread)
        print(events[1][0])
        """
    )
    assert output.splitlines() == [
        "refused: refused by audit hook",
        "('localhost', 80, 0, 1, 0)",
        "loop thread: True",
        "('localhost', 'http', 0, 1, 0)",
    ]


def test_native_resolver_uses_c_audit_even_if_python_sys_audit_is_replaced():
    # CPython raises the event through PySys_Audit, so a replaced sys.audit
    # cannot skip a registered hook.
    output = _run_script(
        """
        import asyncio, socket, sys
        import rsloop

        def hook(event, args):
            if event == "socket.getaddrinfo":
                raise PermissionError("audit refusal")

        sys.addaudithook(hook)
        sys.audit = lambda *args: None

        async def main():
            loop = asyncio.get_running_loop()
            return await loop.getaddrinfo("localhost", 80, type=socket.SOCK_STREAM)

        try:
            rsloop.run(main())
        except PermissionError as error:
            print("refused:", error)
        """
    )
    assert output.splitlines() == ["refused: audit refusal"]


def test_native_resolver_respects_low_level_socket_override(monkeypatch):
    # socket.getaddrinfo delegates to _socket.getaddrinfo; a replaced
    # delegate receives the lookup, as on asyncio.
    expected = [(socket.AF_INET, socket.SOCK_STREAM, 6, "", ("192.0.2.25", 80))]
    calls = []

    def lookup(*args):
        calls.append(args)
        return expected

    monkeypatch.setattr(_socket, "getaddrinfo", lookup)
    assert _resolve("localhost", 80, **STREAM) == expected
    assert len(calls) == 1


def test_a_low_level_override_receives_every_lookup():
    # Installed before this interpreter's first lookup, removed and installed
    # again: native lookups (audited on the loop thread) only while the
    # builtin delegate is in place.
    output = _run_script(
        """
        import _socket, asyncio, socket, sys, threading
        import rsloop

        builtin = _socket.getaddrinfo
        calls = []
        native = {}

        def override(host, port, *args):
            calls.append(port)
            return builtin(host, port, *args)

        def hook(event, args):
            if event == "socket.getaddrinfo" and args[1] not in native:
                native[args[1]] = threading.get_ident() == loop_thread

        sys.addaudithook(hook)
        loop_thread = threading.get_ident()

        def lookup(port):
            async def main():
                loop = asyncio.get_running_loop()
                await loop.getaddrinfo("localhost", port, type=socket.SOCK_STREAM)

            rsloop.run(main())

        _socket.getaddrinfo = override
        lookup(1)
        _socket.getaddrinfo = builtin
        lookup(2)
        _socket.getaddrinfo = override
        lookup(3)
        print(calls)
        print(native)
        """
    )
    assert output.splitlines() == [
        "[1, 3]",
        "{1: False, 2: True, 3: False}",
    ]


def test_a_functools_wraps_wrapper_receives_every_lookup():
    # A wrapper installed before this interpreter's first lookup copies the
    # stdlib function's name and module; it must still see every call. The
    # audit event shows which path ran: native raises it on the loop thread.
    output = _run_script(
        """
        import asyncio, functools, socket, sys, threading
        import rsloop

        original = socket.getaddrinfo
        calls = []
        native = {}

        @functools.wraps(original)
        def wrapper(*args, **kwargs):
            calls.append(args[1])
            return original(*args, **kwargs)

        def hook(event, args):
            if event == "socket.getaddrinfo":
                native[args[1]] = threading.get_ident() == loop_thread

        sys.addaudithook(hook)
        loop_thread = threading.get_ident()

        def lookup(port):
            async def main():
                loop = asyncio.get_running_loop()
                await loop.getaddrinfo("localhost", port, type=socket.SOCK_STREAM)

            rsloop.run(main())

        socket.getaddrinfo = wrapper
        lookup(1)
        lookup(2)
        socket.getaddrinfo = original
        lookup(3)
        socket.getaddrinfo = wrapper
        lookup(4)
        print(calls)
        print(native)
        """
    )
    assert output.splitlines() == [
        "[1, 2, 4]",
        "{1: False, 2: False, 3: True, 4: False}",
    ]


@pytest.mark.skipif(
    not sys.platform.startswith("linux"), reason="reads thread names in /proc"
)
def test_a_lookup_burst_starts_a_bounded_number_of_threads():
    # At most min(32, CPUs + 4) lookups run at once, as on asyncio's default
    # executor (blocking::tests checks that exact bound). This counts smol's
    # blocking-pool threads ("blocking-N"): the pool may start a thread while
    # one that just gave up its slot is still on its way back to idle, so
    # allow up to twice the cap. Without the cap a burst grows the pool to its
    # 500-thread limit.
    output = _run_script(
        """
        import asyncio, os, socket, threading, time
        import rsloop

        def workers():
            count = 0
            for task in os.listdir("/proc/self/task"):
                try:
                    with open(f"/proc/self/task/{task}/comm") as comm:
                        count += comm.read().startswith("blocking-")
                except OSError:
                    pass
            return count

        async def main():
            loop = asyncio.get_running_loop()
            peak = [workers()]
            stop = threading.Event()

            def sample():
                while not stop.is_set():
                    peak[0] = max(peak[0], workers())
                    time.sleep(0.0005)

            sampler = threading.Thread(target=sample)
            sampler.start()
            ports = [1000 + index for index in range(1000)]
            try:
                results = await asyncio.gather(
                    *(loop.getaddrinfo("localhost", port, type=socket.SOCK_STREAM)
                      for port in ports)
                )
            finally:
                stop.set()
                sampler.join()
            for port, rows in zip(ports, results, strict=True):
                assert rows == socket.getaddrinfo("localhost", port, type=socket.SOCK_STREAM)
            print(peak[0])

        rsloop.run(main())
        """
    )
    cpus = (
        os.process_cpu_count()
        if hasattr(os, "process_cpu_count")
        else len(os.sched_getaffinity(0))
    )
    assert 1 <= int(output) <= 2 * min(32, (cpus or 1) + 4), output
