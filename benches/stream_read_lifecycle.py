"""Content-checked buffered and waiting reads; compare identical release builds."""

import argparse
import asyncio
import json
import platform
import statistics
import time
from typing import Any, cast

import rsloop
import rsloop._loop as loop_module

FastReader = cast(Any, loop_module).PyFastStreamReader


async def measure(method, waiting, rounds):
    reader = FastReader(65536, asyncio.get_running_loop())
    payload = b"x" * 63 + b"\n"
    call = {
        "read": lambda: reader.read(64),
        "readexactly": lambda: reader.readexactly(64),
        "readuntil": lambda: reader.readuntil(b"\n"),
    }[method]
    started = time.perf_counter_ns()
    for _ in range(rounds):
        if waiting:
            pending = call()
            reader.feed_data(payload[:32])
            if method == "read":
                assert await pending == payload[:32]
                pending = call()
            reader.feed_data(payload[32:])
            assert await pending == (payload[32:] if method == "read" else payload)
        else:
            reader.feed_data(payload)
            assert await call() == payload
        if waiting:
            # Include dispatch of Future done callbacks in the measurement.
            await asyncio.sleep(0)
    return (time.perf_counter_ns() - started) / rounds


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rounds", type=int, default=100000)
    parser.add_argument("--repeat", type=int, default=7)
    parser.add_argument("--output", required=True)
    args = parser.parse_args()
    results = {
        "python": platform.python_version(),
        "settings": vars(args),
        "ns_per_round": {},
    }
    for waiting in (False, True):
        for method in ("read", "readexactly", "readuntil"):
            rsloop.run(measure(method, waiting, 1000))
            samples = [
                rsloop.run(measure(method, waiting, args.rounds))
                for _ in range(args.repeat)
            ]
            name = f"{method}-{'waiting' if waiting else 'buffered'}"
            results["ns_per_round"][name] = samples
            print(name, round(statistics.median(samples), 1), flush=True)
    with open(args.output, "w") as output:
        json.dump(results, output, indent=2)


if __name__ == "__main__":
    main()
