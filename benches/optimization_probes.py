#!/usr/bin/env python3
"""Focused before/after probes; fresh processes, rotating loop order, raw samples.

Build rsloop in release mode before running. This complements, rather than
replaces, compare_event_loops.py and workload_matrix.py.
"""

from __future__ import annotations

import argparse
import asyncio
import importlib
import importlib.metadata
import json
import os
import platform
import statistics
import subprocess
import sys
import time
from pathlib import Path


async def callbacks(mode: str) -> None:
    loop = asyncio.get_running_loop()
    batches, count = (40, 5000) if mode == "callbacks_recycled" else (1, 100_000)
    if mode.startswith("timers"):
        count = 10_000
    for _ in range(batches):
        done = loop.create_future()
        remaining = count

        def callback(value=None):
            nonlocal remaining
            remaining -= 1
            if not remaining:
                done.set_result(None)  # noqa: B023 - Each batch is awaited before advancing.

        for _ in range(count):
            if mode.startswith("timers"):
                loop.call_later(0 if mode == "timers_zero" else 0.001, callback)
            elif mode == "callbacks_one":
                loop.call_soon(callback, None)
            else:
                loop.call_soon(callback)
        await done


async def raw_socket() -> None:
    import socket

    loop = asyncio.get_running_loop()
    a, b = socket.socketpair()
    a.setblocking(False)
    b.setblocking(False)

    async def echo():
        for _ in range(2000):
            data = await loop.sock_recv(b, 1)
            assert data == b"x"
            await loop.sock_sendall(b, data)

    task = asyncio.create_task(echo())
    try:
        for _ in range(2000):
            await loop.sock_sendall(a, b"x")
            assert await loop.sock_recv(a, 1) == b"x"
        await task
    finally:
        task.cancel()
        await asyncio.gather(task, return_exceptions=True)
        a.close()
        b.close()


async def numeric_dns() -> None:
    import socket

    loop = asyncio.get_running_loop()
    for _ in range(10_000):
        result = await loop.getaddrinfo("127.0.0.1", 443, type=socket.SOCK_STREAM)
        assert result[0][4] == ("127.0.0.1", 443)


async def protocol_transfer(buffered: bool, lines: bool) -> None:
    loop = asyncio.get_running_loop()
    done = loop.create_future()
    total = 32 * 1024 * 1024
    chunk = b"x" * 4096

    class Sender(asyncio.Protocol):
        def connection_made(self, transport):
            self.transport = transport
            self.sent = 0
            self.paused = False
            self.send()

        def send(self):
            while self.sent < total and not self.paused:
                if lines:
                    self.transport.writelines((chunk[:128], chunk[128:]))
                else:
                    self.transport.write(chunk)
                self.sent += len(chunk)
            if self.sent == total:
                self.transport.close()

        def pause_writing(self):
            self.paused = True

        def resume_writing(self):
            self.paused = False
            self.send()

    class Receiver:
        def __init__(self):
            self.count = 0

        def connection_lost(self, exc):
            if done.done():
                return
            if exc:
                done.set_exception(exc)
            elif self.count != total:
                done.set_exception(AssertionError((self.count, total)))
            else:
                done.set_result(None)

    class PlainReceiver(Receiver, asyncio.Protocol):
        def data_received(self, data):
            self.count += len(data)

    class BufferedReceiver(Receiver, asyncio.BufferedProtocol):
        def __init__(self):
            super().__init__()
            self.buffer = bytearray(4096)

        def get_buffer(self, sizehint):
            return self.buffer

        def buffer_updated(self, count):
            self.count += count

    server = await loop.create_server(Sender, "127.0.0.1", 0)
    transport = None
    try:
        transport, _ = await loop.create_connection(
            BufferedReceiver if buffered else PlainReceiver,
            "127.0.0.1",
            server.sockets[0].getsockname()[1],
        )
        await done
    finally:
        if transport is not None:
            transport.close()
        server.close()
        await server.wait_closed()


SCENARIOS = (
    "raw_socket",
    "numeric_dns",
    "callbacks_zero",
    "callbacks_one",
    "callbacks_recycled",
    "timers_zero",
    "timers_positive",
    "protocol_read",
    "buffered_read",
    "writelines",
)


async def measure(scenario):
    start = time.perf_counter()
    if scenario == "raw_socket":
        await raw_socket()
    elif scenario == "numeric_dns":
        await numeric_dns()
    elif scenario in ("protocol_read", "buffered_read", "writelines"):
        await protocol_transfer(scenario == "buffered_read", scenario == "writelines")
    else:
        await callbacks(scenario)
    return (time.perf_counter() - start) * 1000


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--loops", default="rsloop,uvloop,zuvloop")
    parser.add_argument("--scenarios", default=",".join(SCENARIOS))
    parser.add_argument("--repeat", type=int, default=7)
    parser.add_argument("--label", default="unlabelled")
    parser.add_argument("--child", nargs=2, metavar=("LOOP", "SCENARIO"))
    parser.add_argument(
        "--baseline-pythonpath",
        help="Installed baseline wheel directory; adds rsloop_before to rotated runs",
    )
    args = parser.parse_args()
    if args.baseline_pythonpath:
        args.baseline_pythonpath = str(Path(args.baseline_pythonpath).resolve())
        if not (Path(args.baseline_pythonpath) / "rsloop" / "__init__.py").is_file():
            parser.error("baseline directory must contain an installed rsloop package")
    if args.child:
        name, scenario = args.child
        if name == "rsloop_before":
            if not args.baseline_pythonpath:
                parser.error("rsloop_before requires --baseline-pythonpath")
            sys.path.insert(0, args.baseline_pythonpath)
            name = "rsloop"
        module = importlib.import_module(name)
        if args.child[0] == "rsloop_before" and not Path(
            module.__file__
        ).is_relative_to(args.baseline_pythonpath):
            raise RuntimeError(
                "baseline import did not use the supplied wheel directory"
            )
        loop = module.new_event_loop()
        asyncio.set_event_loop(loop)
        try:
            print(
                json.dumps(
                    loop.run_until_complete(asyncio.wait_for(measure(scenario), 60))
                )
            )
        finally:
            loop.close()
        return
    if args.repeat < 1:
        parser.error("--repeat must be positive")
    names = args.loops.split(",")
    if args.baseline_pythonpath:
        names.insert(0, "rsloop_before")
    scenarios = args.scenarios.split(",")
    if any(s not in SCENARIOS for s in scenarios):
        parser.error("unknown scenario")
    rows = {s: {n: [] for n in names} for s in scenarios}
    for scenario in scenarios:
        for block in range(args.repeat + 1):
            offset = block % len(names)
            for name in names[offset:] + names[:offset]:
                command = [sys.executable, __file__, "--child", name, scenario]
                if args.baseline_pythonpath:
                    command.extend(["--baseline-pythonpath", args.baseline_pythonpath])
                result = subprocess.run(
                    command,
                    capture_output=True,
                    text=True,
                    check=False,
                    timeout=90,
                )
                if result.returncode:
                    print(result.stderr, file=sys.stderr, flush=True)
                    result.check_returncode()
                if block:
                    rows[scenario][name].append(json.loads(result.stdout))
        print(
            f"{scenario}: "
            + str(
                {n: round(statistics.median(v), 3) for n, v in rows[scenario].items()}
            ),
            file=sys.stderr,
            flush=True,
        )
    print(
        json.dumps(
            {
                "label": args.label,
                "python": sys.version,
                "platform": platform.platform(),
                "versions": {
                    n: (
                        next(
                            distribution.version
                            for distribution in importlib.metadata.distributions(
                                path=[args.baseline_pythonpath]
                            )
                            if distribution.metadata["Name"] == "rsloop"
                        )
                        if n == "rsloop_before"
                        else importlib.metadata.version(n)
                    )
                    for n in names
                },
                "baseline_pythonpath": args.baseline_pythonpath,
                "cpu_affinity": sorted(os.sched_getaffinity(0))
                if hasattr(os, "sched_getaffinity")
                else None,
                "method": "fresh process per scenario/run; rotating loop order; first block discarded",
                "samples_ms": rows,
            },
            indent=2,
        )
    )


if __name__ == "__main__":
    main()
