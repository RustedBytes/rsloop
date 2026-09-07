"""Numeric resolver fast path and the boundaries that require the executor."""

import asyncio
import concurrent.futures
import socket

import pytest
import rsloop


@pytest.mark.parametrize(
    "host", ["127.0.0.1", "::1", "2001:0DB8:0:0::1", "::ffff:127.0.0.1", "::192.0.2.1"]
)
@pytest.mark.parametrize(
    "kind,proto",
    [(socket.SOCK_STREAM, socket.IPPROTO_TCP), (socket.SOCK_DGRAM, socket.IPPROTO_UDP)],
)
@pytest.mark.parametrize("port", [0, 443, 65535])
def test_numeric_results_match_system_resolver(host, kind, proto, port, monkeypatch):
    if ":" in host and not socket.has_ipv6:
        pytest.skip("IPv6 is unavailable")
    family = socket.AF_INET6 if ":" in host else socket.AF_INET
    expected = socket.getaddrinfo(host, port, family, kind, proto)

    def unexpected_lookup(*args, **kwargs):
        raise AssertionError("numeric fast path must not call the resolver")

    monkeypatch.setattr(socket, "getaddrinfo", unexpected_lookup)

    async def main():
        loop = asyncio.get_running_loop()
        for requested_family in (socket.AF_UNSPEC, family):
            for requested_proto in (0, proto):
                future = loop.getaddrinfo(
                    host,
                    port,
                    family=requested_family,
                    type=kind,
                    proto=requested_proto,
                )
                assert isinstance(future, asyncio.Future)
                assert future.done()
                assert await future == expected

    rsloop.run(main())


@pytest.mark.parametrize(
    "host,port,options",
    [
        ("localhost", 80, {}),
        ("127.0.0.1", "http", {}),
        ("127.0.0.1", "80", {}),
        (b"127.0.0.1", 80, {}),
        ("::1%lo", 80, {}),
        ("127.0.0.1", 80, {"flags": socket.AI_CANONNAME}),
        ("127.0.0.1", 80, {"type": 0}),
        ("127.0.0.1", 80, {"family": socket.AF_INET6}),
        ("127.0.0.1", 80, {"proto": socket.IPPROTO_UDP}),
        ("127.0.0.1", -1, {}),
        ("127.0.0.1", 65536, {}),
        ("127.0.0.1", None, {}),
        (None, 80, {}),
    ],
)
def test_general_resolution_keeps_original_arguments(host, port, options, monkeypatch):
    calls = []

    def resolver(*args):
        calls.append(args)
        return ["resolved through executor"]

    monkeypatch.setattr(socket, "getaddrinfo", resolver)
    kwargs = {"family": 0, "type": socket.SOCK_STREAM, "proto": 0, "flags": 0} | options

    async def main():
        return await asyncio.get_running_loop().getaddrinfo(host, port, **kwargs)

    assert rsloop.run(main()) == ["resolved through executor"]
    assert calls == [
        (host, port, kwargs["family"], kwargs["type"], kwargs["proto"], kwargs["flags"])
    ]


def test_numeric_resolution_respects_custom_executor_and_shutdown():
    calls = []

    class Executor:
        def submit(self, func, *args):
            calls.append(args)
            future = concurrent.futures.Future()
            future.set_result(["custom executor"])
            return future

        def shutdown(self, wait):
            pass

    async def main():
        loop = asyncio.get_running_loop()
        loop.set_default_executor(Executor())
        assert await loop.getaddrinfo("127.0.0.1", 80, type=socket.SOCK_STREAM) == [
            "custom executor"
        ]
        await loop.shutdown_default_executor()
        with pytest.raises(RuntimeError, match="Executor shutdown has been called"):
            await loop.getaddrinfo("127.0.0.1", 80, type=socket.SOCK_STREAM)

    rsloop.run(main())
    assert len(calls) == 1


def test_numeric_resolution_respects_loop_subclass():
    class Loop(rsloop.Loop):
        def run_in_executor(self, executor, func, *args):
            future = self.create_future()
            future.set_result(["subclass executor"])
            return future

    loop = Loop()
    try:
        result = loop.run_until_complete(
            loop.getaddrinfo("127.0.0.1", 80, type=socket.SOCK_STREAM)
        )
        assert result == ["subclass executor"]
    finally:
        loop.close()
