"""Measure buffered reads and segmented TCP writes with content validation."""

import argparse
import asyncio
import json
import platform
import statistics
import time

import rsloop
from rsloop._loop import PyFastStreamReader


async def buffered_reads(rounds, size):
    reader = PyFastStreamReader(size * 2, asyncio.get_running_loop())
    payload = b"r" * size
    piece = b"r" * 4096
    started = time.perf_counter()
    for _ in range(rounds):
        reader.feed_data(payload)
        for _ in range(size // len(piece)):
            assert await reader.readexactly(len(piece)) == piece
    return time.perf_counter() - started


async def segmented_writes(rounds, size):
    pieces = [bytes([index]) * (size // 4) for index in range(4)]
    expected = b"".join(pieces)
    done = asyncio.get_running_loop().create_future()

    async def receive(reader, writer):
        try:
            for _ in range(rounds):
                assert await reader.readexactly(size) == expected
                writer.write(b"!")
                await writer.drain()
            done.set_result(None)
        except (AssertionError, OSError, asyncio.IncompleteReadError) as exc:
            done.set_exception(exc)
        finally:
            writer.close()
            await writer.wait_closed()

    server = await asyncio.start_server(receive, "127.0.0.1", 0)
    try:
        reader, writer = await asyncio.open_connection(
            "127.0.0.1", server.sockets[0].getsockname()[1]
        )
        try:
            started = time.perf_counter()
            for _ in range(rounds):
                writer.writelines(pieces)
                await writer.drain()
                assert await reader.readexactly(1) == b"!"
            elapsed = time.perf_counter() - started
            await done
            return elapsed
        finally:
            writer.close()
            await writer.wait_closed()
    finally:
        server.close()
        await server.wait_closed()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rounds", type=int, default=512)
    parser.add_argument("--size", type=int, default=1024 * 1024)
    parser.add_argument("--repeat", type=int, default=7)
    parser.add_argument("--warmups", type=int, default=2)
    parser.add_argument("--output", required=True)
    args = parser.parse_args()
    if args.size <= 0 or args.size % 4096 or min(args.rounds, args.repeat) <= 0:
        parser.error(
            "size must be a positive multiple of 4096; rounds/repeat must be positive"
        )
    result = {"settings": vars(args), "python": platform.python_version(), "runs": {}}
    for workload in (buffered_reads, segmented_writes):
        samples = []
        for index in range(args.warmups + args.repeat):
            elapsed = rsloop.run(
                asyncio.wait_for(workload(args.rounds, args.size), 120)
            )
            if index >= args.warmups:
                samples.append(elapsed)
        result["runs"][workload.__name__] = samples
        print(workload.__name__, statistics.median(samples), flush=True)
    with open(args.output, "w") as output:
        json.dump(result, output, indent=2)


if __name__ == "__main__":
    main()
