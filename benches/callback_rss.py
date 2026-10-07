#!/usr/bin/env python3
"""Linux callback allocation/lifetime probe; each sample uses a fresh process.

Timing runs have no weakrefs or per-callback instrumentation. Lifetime runs
sample handles, record RSS before dispatch, and check cleanup before/after GC.
Use the same free-threaded binary with PYTHON_GIL=0 and 1 to isolate runtime
GIL effects, then compare a same-version conventional build separately.
"""

from __future__ import annotations

import argparse
import asyncio
import contextvars
import gc
import importlib
import json
import os
import platform
import resource
import statistics
import subprocess
import sys
import sysconfig
import threading
import time
import tracemalloc
import weakref
from pathlib import Path


def rss() -> int:
    return int(Path("/proc/self/statm").read_text().split()[1]) * os.sysconf(
        "SC_PAGE_SIZE"
    )


def metadata() -> dict:
    return {
        "python": sys.version,
        "executable": sys.executable,
        "free_threaded_build": bool(sysconfig.get_config_var("Py_GIL_DISABLED")),
        "gil_enabled": getattr(sys, "_is_gil_enabled", lambda: True)(),
        "context_aware_warnings": getattr(sys.flags, "context_aware_warnings", None),
        "xoptions": sys._xoptions,
        "config_args": sysconfig.get_config_var("CONFIG_ARGS"),
        "allocator": os.environ.get("PYTHONMALLOC", "default"),
        "platform": platform.platform(),
        "affinity": sorted(os.sched_getaffinity(0)),
    }


def child(args) -> dict:
    factory = asyncio.new_event_loop
    if args.loop == "rsloop":
        import rsloop

        factory = rsloop.new_event_loop
    elif args.loop == "zuvloop":
        # Optional benchmark baseline; load only when explicitly selected.
        factory = importlib.import_module("zuvloop").new_event_loop
    result = {
        "metadata": metadata(),
        "loop": args.loop,
        "mode": args.mode,
        "count": args.count,
        "context_mode": args.context,
        "producer": args.producer,
        "baseline_rss": rss(),
    }
    refs = []
    payload_refs = []
    released = 0
    phases = []
    gc.disable()
    if args.mode == "allocations":
        tracemalloc.start()
    loop = factory()

    def snapshot(name):
        phases.append(
            {
                "phase": name,
                "rss": rss(),
                "sampled_alive": sum(ref() is not None for ref in refs),
                "payload_alive": sum(ref() is not None for ref in payload_refs),
                "payload_released": released,
            }
        )

    async def run():
        result["context_keys"] = [var.name for var in contextvars.copy_context()]
        done = loop.create_future()
        remaining = args.count
        checkpoints = {args.count // 4, args.count // 2, 0}

        def callback():
            nonlocal remaining
            remaining -= 1
            if remaining == 0:
                done.set_result(None)

        if args.mode in ("lifetime", "cleanup"):
            original_callback = callback

            def observed_callback():
                original_callback()
                if remaining in checkpoints:
                    snapshot(f"drained_{args.count - remaining}")

            callback = observed_callback

        snapshot("before_queue")
        started = time.perf_counter()
        stride = max(1, args.count // 1024)

        class Probe:
            def __call__(self):
                callback()

            def __del__(self):
                nonlocal released
                released += 1

        errors = []

        def enqueue():
            try:
                schedule = (
                    loop.call_soon_threadsafe
                    if args.producer == "thread"
                    else loop.call_soon
                )
                if args.mode in ("timing", "allocations"):
                    for _ in range(args.count):
                        schedule(callback)
                    return
                for index in range(args.count):
                    target = (
                        Probe()
                        if args.mode == "cleanup" and index % stride == 0
                        else callback
                    )
                    handle = schedule(target)
                    if args.mode in ("lifetime", "cleanup") and index % stride == 0:
                        refs.append(weakref.ref(handle))
                        if args.mode == "cleanup":
                            payload_refs.append(weakref.ref(target))
                        if index == 0:
                            result["handle_layout"] = {
                                "basicsize": type(handle).__basicsize__,
                                "sizeof": sys.getsizeof(handle),
                                "gc_tracked": gc.is_tracked(handle),
                            }
            except BaseException as exc:  # noqa: BLE001 -- propagate producer errors to the main thread
                errors.append(exc)

        if args.producer == "thread":
            producer = threading.Thread(target=enqueue)
            producer.start()
            producer.join()
        else:
            enqueue()
        if errors:
            raise errors[0]
        queued = time.perf_counter()
        if args.mode in ("lifetime", "cleanup", "allocations"):
            snapshot("queued")
        if args.mode == "allocations":
            result["queued_traced"] = tracemalloc.get_traced_memory()
            result["queued_allocations"] = [
                {
                    "location": str(stat.traceback),
                    "count": stat.count,
                    "bytes": stat.size,
                }
                for stat in tracemalloc.take_snapshot().statistics("lineno")[:10]
            ]
        await done
        finished = time.perf_counter()
        result.update(
            queue_ms=(queued - started) * 1000,
            total_ms=(finished - started) * 1000,
            dispatch_ms=(finished - queued) * 1000,
        )
        snapshot("resumed")

    try:
        if args.context == "ambient":
            loop.run_until_complete(run())
        else:
            context = contextvars.Context()
            if args.context == "nonempty":
                variable = contextvars.ContextVar("benchmark_value")
                context.run(variable.set, "captured")
            context.run(loop.run_until_complete, run())
        snapshot("returned")
    finally:
        loop.close()
    snapshot("closed")
    result["collected"] = gc.collect()
    snapshot("gc")
    if args.mode == "allocations":
        result["cleanup_traced"] = tracemalloc.get_traced_memory()
    result["phases"] = phases
    result["sampled"] = len(refs)
    result["peak_rss"] = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss * 1024
    result["peak_delta"] = result["peak_rss"] - result["baseline_rss"]
    if any(ref() is not None for ref in refs + payload_refs):
        raise RuntimeError("sampled callback handles survived loop.close + gc.collect")
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--loop", choices=["rsloop", "asyncio", "zuvloop"], default="rsloop"
    )
    parser.add_argument("--count", type=int, default=1_000_000)
    parser.add_argument(
        "--mode",
        choices=["timing", "lifetime", "allocations", "cleanup"],
        default="timing",
    )
    parser.add_argument(
        "--context", choices=["ambient", "empty", "nonempty"], default="ambient"
    )
    parser.add_argument("--producer", choices=["local", "thread"], default="local")
    parser.add_argument("--repeat", type=int, default=7)
    parser.add_argument("--warmups", type=int, default=2)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--child", action="store_true")
    args = parser.parse_args()
    if args.count <= 0 or args.repeat <= 0 or args.warmups < 0:
        parser.error("count/repeat must be positive and warmups nonnegative")
    if args.child:
        print(json.dumps(child(args)))
        return
    xoptions = [
        arg
        for key, value in sys._xoptions.items()
        for arg in ["-X", key if value is True else f"{key}={value}"]
    ]
    command = [
        sys.executable,
        *xoptions,
        __file__,
        "--child",
        "--loop",
        args.loop,
        "--count",
        str(args.count),
        "--mode",
        args.mode,
        "--context",
        args.context,
        "--producer",
        args.producer,
    ]
    samples = []
    for index in range(args.warmups + args.repeat):
        value = json.loads(subprocess.check_output(command, text=True))
        if index >= args.warmups:
            samples.append(value)
    output = {
        "samples": samples,
        "median": {
            key: statistics.median(sample[key] for sample in samples)
            for key in ["queue_ms", "dispatch_ms", "total_ms", "peak_rss", "peak_delta"]
        },
    }
    if args.output:
        args.output.parent.mkdir(parents=True, exist_ok=True)
        args.output.write_text(json.dumps(output, indent=2) + "\n")
    print(json.dumps(output["median"], indent=2))


if __name__ == "__main__":
    main()
