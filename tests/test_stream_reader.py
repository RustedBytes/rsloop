"""`readline()` / `readuntil()` on the native fast stream reader.

Importing rsloop patches `asyncio.open_connection()` and `asyncio.start_server()`,
so these methods have to behave exactly like `asyncio.StreamReader`'s or every
line-oriented protocol changes behaviour just by having rsloop installed. Most of
the coverage here is therefore differential: the same feed script is driven
through both readers and the results have to match, down to the exception type,
message, attributes, and the bytes left in the buffer.
"""

from __future__ import annotations

import asyncio
import contextlib
import sys
from typing import Any, cast

import pytest
import rsloop
import rsloop._loop as loop_module

PyFastStreamReader = cast(Any, loop_module).PyFastStreamReader

FEED = "feed"
EOF = "eof"
EXC = "exc"

FAST_STREAMS_ACTIVE = getattr(asyncio.open_connection, "__module__", "").startswith(
    "rsloop"
)

# The native reader accepts a tuple of separators on every supported version,
# but `asyncio.StreamReader` only learned the form in 3.13 — below that it
# raises TypeError out of `bytearray.find`. These cases therefore only have a
# stdlib reference to compare against on 3.13+; `test_tuple_separators` pins the
# native behaviour on every version.
STDLIB_HAS_TUPLE_SEPARATORS = sys.version_info >= (3, 13)
TUPLE_SEPARATOR_CASES = frozenset(
    {
        "tuple_separator_shortest_match_wins",
        "tuple_separator_tie_reports_the_shortest_match_start",
        "tuple_separator_earliest_match_wins",
        "tuple_separator_at_eof",
        "empty_separator_tuple_is_rejected",
        "tuple_containing_an_empty_separator_is_rejected",
    }
)


def apply_step(reader, step) -> None:
    kind = step[0]
    if kind == FEED:
        reader.feed_data(step[1])
    elif kind == EOF:
        reader.feed_eof()
    elif kind == EXC:
        reader.set_exception(RuntimeError("boom"))
    else:  # pragma: no cover - guards the table below against typos
        raise AssertionError(f"unknown step {kind!r}")


async def drive(reader, script, call):
    """Run `call(reader)` against `script`, reporting a comparable outcome.

    A read that never resolves reports `("pending",)` rather than blocking, so
    the "waits for more data" cases stay in the same table as the rest instead
    of needing a timeout.
    """
    task = asyncio.ensure_future(call(reader))
    await asyncio.sleep(0)
    for step in script:
        apply_step(reader, step)
        await asyncio.sleep(0)
    for _ in range(5):
        if task.done():
            break
        await asyncio.sleep(0)

    if not task.done():
        task.cancel()
        with contextlib.suppress(asyncio.CancelledError):
            await task
        return ("pending",)

    try:
        return ("ok", await task)
    except Exception as exc:  # noqa: BLE001 - the outcome under comparison
        details = {}
        if isinstance(exc, asyncio.IncompleteReadError):
            details = {"partial": exc.partial, "expected": exc.expected}
        elif isinstance(exc, asyncio.LimitOverrunError):
            details = {"consumed": exc.consumed}
        return ("exc", type(exc).__name__, str(exc), details)


# (name, limit, feed script, read call)
READER_CASES = [
    (
        "separator_already_buffered",
        64,
        [(FEED, b"hello\nworld\n")],
        lambda reader: reader.readline(),
    ),
    (
        "separator_split_across_chunks",
        64,
        [(FEED, b"he"), (FEED, b"ll"), (FEED, b"o\nrest")],
        lambda reader: reader.readline(),
    ),
    (
        "multibyte_separator_split_across_chunks",
        64,
        [(FEED, b"abc\r"), (FEED, b"\ndef")],
        lambda reader: reader.readuntil(b"\r\n"),
    ),
    (
        "partial_separator_at_chunk_boundary",
        64,
        [(FEED, b"xxA"), (FEED, b"BCyy")],
        lambda reader: reader.readuntil(b"ABC"),
    ),
    (
        "partial_separator_across_three_chunks",
        64,
        [(FEED, b"xxA"), (FEED, b"B"), (FEED, b"Czz")],
        lambda reader: reader.readuntil(b"ABC"),
    ),
    (
        "eof_with_partial_line",
        64,
        [(FEED, b"partial"), (EOF,)],
        lambda reader: reader.readline(),
    ),
    (
        "eof_with_nothing_buffered",
        64,
        [(EOF,)],
        lambda reader: reader.readline(),
    ),
    (
        "eof_with_partial_readuntil",
        64,
        [(FEED, b"partial"), (EOF,)],
        lambda reader: reader.readuntil(b"\n"),
    ),
    (
        "separator_completed_by_the_chunk_that_hits_eof",
        64,
        [(FEED, b"a\nb"), (EOF,)],
        lambda reader: reader.readline(),
    ),
    (
        "separator_longer_than_everything_buffered",
        64,
        [(FEED, b"ab"), (EOF,)],
        lambda reader: reader.readuntil(b"abcdef"),
    ),
    (
        "readline_limit_overrun_without_separator",
        8,
        [(FEED, b"x" * 40)],
        lambda reader: reader.readline(),
    ),
    (
        "readline_limit_overrun_with_separator",
        8,
        [(FEED, b"x" * 40 + b"\n")],
        lambda reader: reader.readline(),
    ),
    (
        "readuntil_limit_overrun_without_separator",
        8,
        [(FEED, b"x" * 40)],
        lambda reader: reader.readuntil(b"\n"),
    ),
    (
        "readuntil_limit_overrun_with_separator",
        8,
        [(FEED, b"x" * 40 + b"\n")],
        lambda reader: reader.readuntil(b"\n"),
    ),
    # The limit is inclusive: a chunk exactly `limit` long still reads.
    (
        "readuntil_chunk_exactly_at_limit",
        8,
        [(FEED, b"x" * 8 + b"\n")],
        lambda reader: reader.readuntil(b"\n"),
    ),
    (
        "readuntil_chunk_one_past_limit",
        8,
        [(FEED, b"x" * 9 + b"\n")],
        lambda reader: reader.readuntil(b"\n"),
    ),
    # Without a separator the unsearchable prefix is what is measured, so
    # `limit` bytes still waits and `limit + 1` overruns.
    (
        "readuntil_waits_at_the_limit",
        8,
        [(FEED, b"x" * 8)],
        lambda reader: reader.readuntil(b"\n"),
    ),
    (
        "readuntil_overruns_one_past_the_limit",
        8,
        [(FEED, b"x" * 9)],
        lambda reader: reader.readuntil(b"\n"),
    ),
    # CPython 3.13+ accepts a tuple; the shortest match wins a tie on the end.
    (
        "tuple_separator_shortest_match_wins",
        64,
        [(FEED, b"xxab yy")],
        lambda reader: reader.readuntil((b"ab", b"b")),
    ),
    # The sort only shows up in `match_start`, which is what the limit is
    # measured against: sorted picks the b"b" match at 3, unsorted the b"ab"
    # match at 2, and only the former is past a limit of 2.
    (
        "tuple_separator_tie_reports_the_shortest_match_start",
        2,
        [(FEED, b"xxab yy")],
        lambda reader: reader.readuntil((b"ab", b"b")),
    ),
    (
        "tuple_separator_earliest_match_wins",
        64,
        [(FEED, b"hello\r\nworld")],
        lambda reader: reader.readuntil((b"\n", b"\r\n")),
    ),
    (
        "tuple_separator_at_eof",
        64,
        [(FEED, b"nope"), (EOF,)],
        lambda reader: reader.readuntil((b"\n", b"\r\n")),
    ),
    (
        "empty_separator_is_rejected",
        64,
        [],
        lambda reader: reader.readuntil(b""),
    ),
    (
        "empty_separator_tuple_is_rejected",
        64,
        [],
        lambda reader: reader.readuntil(()),
    ),
    (
        "tuple_containing_an_empty_separator_is_rejected",
        64,
        [],
        lambda reader: reader.readuntil((b"\n", b"")),
    ),
    (
        "bytearray_separator",
        64,
        [(FEED, b"a;b")],
        lambda reader: reader.readuntil(bytearray(b";")),
    ),
    (
        "pending_until_more_data_arrives",
        64,
        [(FEED, b"no newline yet")],
        lambda reader: reader.readline(),
    ),
    (
        "stream_exception_wins",
        64,
        [(EXC,)],
        lambda reader: reader.readline(),
    ),
    (
        "long_line_assembled_from_many_chunks",
        4096,
        [(FEED, b"y" * 1000)] * 3 + [(FEED, b"\n")],
        lambda reader: reader.readline(),
    ),
]


class TestFastStreamReaderCompat:
    def _run_case(self, limit, script, call):
        results = {}

        async def main() -> None:
            loop = asyncio.get_running_loop()
            for label, factory in (
                ("native", lambda: PyFastStreamReader(limit, loop)),
                ("stdlib", lambda: asyncio.StreamReader(limit=limit, loop=loop)),
            ):
                reader = factory()
                outcome = await drive(reader, script, call)
                results[label] = (outcome, bytes(cast(Any, reader)._buffer))

        rsloop.run(main())
        return results

    @pytest.mark.parametrize(
        "name,limit,script,call", READER_CASES, ids=[case[0] for case in READER_CASES]
    )
    def test_matches_asyncio_stream_reader(self, name, limit, script, call) -> None:
        if name in TUPLE_SEPARATOR_CASES and not STDLIB_HAS_TUPLE_SEPARATORS:
            pytest.skip("asyncio gained tuple separators in 3.13")
        results = self._run_case(limit, script, call)
        native_outcome, native_buffer = results["native"]
        stdlib_outcome, stdlib_buffer = results["stdlib"]
        assert native_outcome == stdlib_outcome, (
            f"{name}: native reader disagrees with asyncio.StreamReader"
        )
        assert native_buffer == stdlib_buffer, (
            f"{name}: buffer left behind differs from asyncio.StreamReader"
        )

    def test_readline_defaults_and_readuntil_default_separator_agree(self) -> None:
        async def main() -> tuple[bytes, bytes]:
            loop = asyncio.get_running_loop()
            with_default = PyFastStreamReader(64, loop)
            with_default.feed_data(b"first\nsecond\n")
            explicit = PyFastStreamReader(64, loop)
            explicit.feed_data(b"first\nsecond\n")
            return (await with_default.readuntil(), await explicit.readuntil(b"\n"))

        first, second = rsloop.run(main())
        assert first == b"first\n"
        assert second == b"first\n"

    def test_tuple_separators(self) -> None:
        """The tuple form, pinned on every supported version.

        `test_matches_asyncio_stream_reader` can only check these against the
        stdlib on 3.13+, so the expected values are spelled out here.
        """

        async def main():
            loop = asyncio.get_running_loop()

            def reader_with(data: bytes, limit: int = 64):
                reader = PyFastStreamReader(limit, loop)
                reader.feed_data(data)
                return reader

            results = {}
            results["earliest"] = await reader_with(b"hello\r\nworld").readuntil(
                (b"\n", b"\r\n")
            )
            results["shortest"] = await reader_with(b"xxab yy").readuntil((b"ab", b"b"))
            results["single_element"] = await reader_with(b"a;b").readuntil((b";",))

            for label, separator in (
                ("empty_tuple", ()),
                ("tuple_with_empty", (b"\n", b"")),
            ):
                try:
                    await reader_with(b"anything").readuntil(separator)
                except ValueError as exc:
                    results[label] = str(exc)
            return results

        results = rsloop.run(main())
        assert results["earliest"] == b"hello\r\n"
        assert results["shortest"] == b"xxab"
        assert results["single_element"] == b"a;"
        assert results["empty_tuple"] == "Separator should contain at least one element"
        assert (
            results["tuple_with_empty"]
            == "Separator should be at least one-byte string"
        )

    def test_readline_rejects_a_second_concurrent_reader(self) -> None:
        async def main() -> None:
            loop = asyncio.get_running_loop()
            reader = PyFastStreamReader(64, loop)
            pending = asyncio.ensure_future(reader.readline())
            await asyncio.sleep(0)
            try:
                with pytest.raises((ValueError, RuntimeError)):
                    await reader.readline()
            finally:
                pending.cancel()
                with contextlib.suppress(asyncio.CancelledError):
                    await pending

        rsloop.run(main())


class TestFastStreamReaderNetwork:
    """The reported break: `readline()` over a real `asyncio.open_connection()`."""

    @staticmethod
    async def _echo_lines(reader, writer) -> None:
        try:
            while True:
                line = await reader.readline()
                if not line:
                    break
                writer.write(line.upper())
                await writer.drain()
        finally:
            writer.close()
            with contextlib.suppress(ConnectionResetError, BrokenPipeError):
                await writer.wait_closed()

    def _round_trip(self, send):
        async def main():
            server = await asyncio.start_server(self._echo_lines, "127.0.0.1", 0)
            port = server.sockets[0].getsockname()[1]
            try:
                reader, writer = await asyncio.open_connection("127.0.0.1", port)
                try:
                    return await send(reader, writer)
                finally:
                    writer.close()
                    with contextlib.suppress(ConnectionResetError, BrokenPipeError):
                        await writer.wait_closed()
            finally:
                server.close()
                await server.wait_closed()

        return rsloop.run(main())

    def test_readline_round_trip(self) -> None:
        async def send(reader, writer):
            writer.write(b"one\ntwo\nthree\n")
            await writer.drain()
            return [await reader.readline() for _ in range(3)]

        assert self._round_trip(send) == [b"ONE\n", b"TWO\n", b"THREE\n"]

    def test_readline_reassembles_a_line_split_across_writes(self) -> None:
        async def send(reader, writer):
            for chunk in (b"sp", b"li", b"t li", b"ne"):
                writer.write(chunk)
                await writer.drain()
                await asyncio.sleep(0.01)
            writer.write(b"\n")
            await writer.drain()
            return await reader.readline()

        assert self._round_trip(send) == b"SPLIT LINE\n"

    def test_readuntil_round_trip(self) -> None:
        async def send(reader, writer):
            writer.write(b"alpha\nbeta\n")
            await writer.drain()
            return await reader.readuntil(b"\n")

        assert self._round_trip(send) == b"ALPHA\n"

    def test_readline_returns_empty_bytes_at_eof(self) -> None:
        async def send(reader, writer):
            writer.write(b"only\n")
            await writer.drain()
            first = await reader.readline()
            writer.write_eof()
            return (first, await reader.readline())

        assert self._round_trip(send) == (b"ONLY\n", b"")

    @pytest.mark.skipif(
        not (FAST_STREAMS_ACTIVE),
        reason="requires the fast-stream patch (RSLOOP_USE_FAST_STREAMS)",
    )
    def test_open_connection_really_uses_the_native_reader(self) -> None:
        # Without this the round-trip tests above would still pass against the
        # stdlib reader and quietly stop covering the native one.
        async def send(reader, writer):
            return type(reader)

        assert self._round_trip(send) is PyFastStreamReader
