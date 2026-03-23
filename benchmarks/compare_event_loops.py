#!/usr/bin/env python3
from __future__ import annotations

import argparse
import asyncio
import ctypes
from ctypes import wintypes
import gc
import importlib
import json
import os
import statistics
import subprocess
import sys
import time
from dataclasses import dataclass, replace
from typing import Callable


LOOP_CHOICES = ("asyncio", "uvloop", "winloop", "rsloop")
WORKLOAD_CHOICES = ("callbacks", "tasks", "tcp_streams")
RSLOOP_PROFILE_ENV = "RSLOOP_TRACY"


def default_loops_csv() -> str:
    loops = ["asyncio", "rsloop"]
    if sys.platform == "win32":
        loops.insert(2, "winloop")
    else:
        loops.insert(2, "uvloop")
    return ",".join(loops)


@dataclass(frozen=True)
class ChildResult:
    loop: str
    workload: str
    seconds: float
    operations: int
    baseline_rss_bytes: int
    peak_rss_bytes: int
    peak_rss_delta_bytes: int

    @property
    def ops_per_sec(self) -> float:
        return self.operations / self.seconds if self.seconds > 0 else float("inf")


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Compare stdlib asyncio, uvloop, winloop, and the Rust prototype event loop.",
    )
    parser.add_argument(
        "--loops",
        default=default_loops_csv(),
        help="Comma-separated loops to benchmark. Choices: asyncio,uvloop,winloop,rsloop",
    )
    parser.add_argument(
        "--workloads",
        default="callbacks,tasks,tcp_streams",
        help="Comma-separated workloads to benchmark. Choices: callbacks,tasks,tcp_streams",
    )
    parser.add_argument(
        "--warmups", type=int, default=1, help="Warmup runs per loop/workload"
    )
    parser.add_argument(
        "--repeat", type=int, default=5, help="Measured runs per loop/workload"
    )
    parser.add_argument(
        "--callbacks",
        type=int,
        default=200_000,
        help="Number of chained call_soon callbacks for the callbacks workload",
    )
    parser.add_argument(
        "--tasks",
        type=int,
        default=50_000,
        help="Number of tiny tasks for the tasks workload",
    )
    parser.add_argument(
        "--task-batch-size",
        type=int,
        default=5_000,
        help="Batch size for the tasks workload to avoid huge one-shot gathers",
    )
    parser.add_argument(
        "--tcp-roundtrips",
        type=int,
        default=5_000,
        help="Round trips for the tcp_streams workload",
    )
    parser.add_argument(
        "--payload-size",
        type=int,
        default=1024,
        help="Payload size in bytes for the tcp_streams workload",
    )
    stream_mode = parser.add_mutually_exclusive_group()
    stream_mode.add_argument(
        "--rsloop-fast-streams",
        dest="rsloop_fast_streams",
        action="store_true",
        help="Use rsloop's native stream wrappers for tcp_streams (default).",
    )
    stream_mode.add_argument(
        "--no-rsloop-fast-streams",
        dest="rsloop_fast_streams",
        action="store_false",
        help="Use the stdlib asyncio streams layer for tcp_streams on rsloop.",
    )
    parser.set_defaults(rsloop_fast_streams=True)
    parser.add_argument(
        "--json-output",
        type=str,
        default=None,
        help="Optional path to write raw benchmark results as JSON",
    )
    parser.add_argument(
        "--profile-rsloop-dir",
        type=str,
        default=None,
        help="Optional directory placeholder used to label one rsloop Tracy run per workload before measured runs",
    )
    parser.add_argument("--child", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument("--loop", choices=LOOP_CHOICES, help=argparse.SUPPRESS)
    parser.add_argument("--workload", choices=WORKLOAD_CHOICES, help=argparse.SUPPRESS)
    parser.add_argument("--profile-label", help=argparse.SUPPRESS)
    return parser.parse_args()


def normalize_csv(value: str, *, allowed: tuple[str, ...], label: str) -> list[str]:
    items = [item.strip() for item in value.split(",") if item.strip()]
    invalid = [item for item in items if item not in allowed]
    if invalid:
        raise SystemExit(f"invalid {label}: {', '.join(invalid)}")
    if not items:
        raise SystemExit(f"no {label} selected")
    return items


def validate_args(args: argparse.Namespace) -> None:
    if args.warmups < 0:
        raise SystemExit("--warmups must be >= 0")
    if args.repeat <= 0:
        raise SystemExit("--repeat must be > 0")
    if args.callbacks <= 0:
        raise SystemExit("--callbacks must be > 0")
    if args.tasks <= 0:
        raise SystemExit("--tasks must be > 0")
    if args.task_batch_size <= 0:
        raise SystemExit("--task-batch-size must be > 0")
    if args.tcp_roundtrips <= 0:
        raise SystemExit("--tcp-roundtrips must be > 0")
    if args.payload_size <= 0:
        raise SystemExit("--payload-size must be > 0")


def env_flag(name: str) -> bool:
    value = os.environ.get(name)
    if value is None:
        return False
    return value.strip().lower() not in {"", "0", "false", "no", "off"}


def loop_factory_for(loop_name: str) -> Callable[[], asyncio.AbstractEventLoop]:
    if loop_name == "asyncio":
        return asyncio.new_event_loop
    if loop_name == "uvloop":
        return importlib.import_module("uvloop").new_event_loop
    if loop_name == "winloop":
        if sys.platform != "win32":
            raise RuntimeError("winloop is only supported on Windows")

        winloop = importlib.import_module("winloop")
        factory = getattr(winloop, "new_event_loop", None)
        if callable(factory):
            return factory

        policy_cls = getattr(winloop, "EventLoopPolicy", None) or getattr(
            winloop, "WinLoopPolicy", None
        )
        if policy_cls is None:
            raise RuntimeError("winloop does not expose a usable event loop factory")

        def factory() -> asyncio.AbstractEventLoop:
            return policy_cls().new_event_loop()

        return factory
    if loop_name == "rsloop":
        return importlib.import_module("rsloop").new_event_loop
    raise AssertionError(f"unsupported loop: {loop_name}")


def is_loop_available(loop_name: str) -> tuple[bool, str | None]:
    try:
        loop_factory_for(loop_name)
    except Exception as exc:  # pragma: no cover - exercised in real env
        return False, f"{type(exc).__name__}: {exc}"
    return True, None


def get_peak_rss_bytes() -> int:
    if sys.platform == "win32":

        class PROCESS_MEMORY_COUNTERS(ctypes.Structure):
            _fields_ = [
                ("cb", wintypes.DWORD),
                ("PageFaultCount", wintypes.DWORD),
                ("PeakWorkingSetSize", ctypes.c_size_t),
                ("WorkingSetSize", ctypes.c_size_t),
                ("QuotaPeakPagedPoolUsage", ctypes.c_size_t),
                ("QuotaPagedPoolUsage", ctypes.c_size_t),
                ("QuotaPeakNonPagedPoolUsage", ctypes.c_size_t),
                ("QuotaNonPagedPoolUsage", ctypes.c_size_t),
                ("PagefileUsage", ctypes.c_size_t),
                ("PeakPagefileUsage", ctypes.c_size_t),
            ]

        kernel32 = ctypes.WinDLL("kernel32", use_last_error=True)
        psapi = ctypes.WinDLL("psapi", use_last_error=True)
        get_current_process = kernel32.GetCurrentProcess
        get_current_process.restype = wintypes.HANDLE

        get_process_memory_info = psapi.GetProcessMemoryInfo
        get_process_memory_info.argtypes = [
            wintypes.HANDLE,
            ctypes.POINTER(PROCESS_MEMORY_COUNTERS),
            wintypes.DWORD,
        ]
        get_process_memory_info.restype = wintypes.BOOL

        counters = PROCESS_MEMORY_COUNTERS()
        counters.cb = ctypes.sizeof(PROCESS_MEMORY_COUNTERS)
        if not get_process_memory_info(
            get_current_process(),
            ctypes.byref(counters),
            counters.cb,
        ):
            raise ctypes.WinError(ctypes.get_last_error())
        return int(counters.PeakWorkingSetSize)

    import resource

    peak_rss = resource.getrusage(resource.RUSAGE_SELF).ru_maxrss
    if sys.platform == "darwin":
        return int(peak_rss)
    return int(peak_rss * 1024)


def get_current_rss_bytes() -> int:
    if sys.platform != "linux":
        return 0

    try:
        with open("/proc/self/status", "r", encoding="utf-8") as f:
            for line in f:
                if not line.startswith("VmRSS:"):
                    continue
                parts = line.split()
                if len(parts) < 2:
                    break
                return int(parts[1]) * 1024
    except OSError:
        return 0

    return 0


def format_bytes(num_bytes: int) -> str:
    units = ["B", "KiB", "MiB", "GiB", "TiB"]
    value = float(num_bytes)
    for unit in units:
        if value < 1024.0 or unit == units[-1]:
            if unit == "B":
                return f"{int(value)} {unit}"
            return f"{value:.1f} {unit}"
        value /= 1024.0
    raise AssertionError("unreachable")


def run_with_loop(loop_name: str, coro: asyncio.coroutines) -> ChildResult:
    loop_factory = loop_factory_for(loop_name)
    if sys.version_info[:2] >= (3, 12):
        return asyncio.run(coro, loop_factory=loop_factory)

    loop = loop_factory()
    try:
        asyncio.set_event_loop(loop)
        return loop.run_until_complete(coro)
    finally:
        asyncio.set_event_loop(None)
        loop.close()


async def bench_callbacks(loop_name: str, iterations: int) -> ChildResult:
    loop = asyncio.get_running_loop()
    done = loop.create_future()
    remaining = iterations

    def callback() -> None:
        nonlocal remaining
        remaining -= 1
        if remaining == 0:
            done.set_result(None)
            return
        loop.call_soon(callback)

    start = time.perf_counter()
    loop.call_soon(callback)
    await done
    return ChildResult(
        loop_name,
        "callbacks",
        time.perf_counter() - start,
        iterations,
        baseline_rss_bytes=0,
        peak_rss_bytes=0,
        peak_rss_delta_bytes=0,
    )


async def bench_tasks(loop_name: str, iterations: int, batch_size: int) -> ChildResult:
    async def tiny_task() -> None:
        await asyncio.sleep(0)

    start = time.perf_counter()
    remaining = iterations
    while remaining > 0:
        current_batch = min(batch_size, remaining)
        await asyncio.gather(*(tiny_task() for _ in range(current_batch)))
        remaining -= current_batch
    return ChildResult(
        loop_name,
        "tasks",
        time.perf_counter() - start,
        iterations,
        baseline_rss_bytes=0,
        peak_rss_bytes=0,
        peak_rss_delta_bytes=0,
    )


async def maybe_wait_closed(writer: asyncio.StreamWriter) -> None:
    wait_closed = getattr(writer, "wait_closed", None)
    if wait_closed is None:
        return
    try:
        await wait_closed()
    except Exception:
        return


async def bench_tcp_streams(
    loop_name: str, roundtrips: int, payload_size: int
) -> ChildResult:
    payload = b"x" * payload_size

    async def handle_echo(
        reader: asyncio.StreamReader, writer: asyncio.StreamWriter
    ) -> None:
        try:
            while True:
                data = await reader.read(64 * 1024)
                if not data:
                    break
                writer.write(data)
                await writer.drain()
        finally:
            writer.close()
            await maybe_wait_closed(writer)

    server = await asyncio.start_server(handle_echo, "127.0.0.1", 0)
    host, port = server.sockets[0].getsockname()[:2]

    try:
        reader, writer = await asyncio.open_connection(host, port)
        start = time.perf_counter()
        for _ in range(roundtrips):
            writer.write(payload)
            await writer.drain()
            response = await reader.readexactly(payload_size)
            if response != payload:
                raise RuntimeError("echo payload mismatch")
        duration = time.perf_counter() - start
        writer.close()
        await maybe_wait_closed(writer)
        return ChildResult(
            loop_name,
            "tcp_streams",
            duration,
            roundtrips,
            baseline_rss_bytes=0,
            peak_rss_bytes=0,
            peak_rss_delta_bytes=0,
        )
    finally:
        server.close()
        await server.wait_closed()


def child_main(args: argparse.Namespace) -> int:
    gc.collect()
    gc.disable()
    baseline_rss_bytes = 0
    try:
        if args.workload == "callbacks":
            coro = bench_callbacks(args.loop, args.callbacks)
        elif args.workload == "tasks":
            coro = bench_tasks(args.loop, args.tasks, args.task_batch_size)
        elif args.workload == "tcp_streams":
            coro = bench_tcp_streams(args.loop, args.tcp_roundtrips, args.payload_size)
        else:  # pragma: no cover - parser guards this
            raise AssertionError(f"unsupported workload: {args.workload}")

        profile_requested = args.profile_label is not None or (
            args.loop == "rsloop" and env_flag(RSLOOP_PROFILE_ENV)
        )
        loop_factory_for(args.loop)
        baseline_rss_bytes = get_current_rss_bytes()
        if profile_requested and not args.profile_label:
            print(
                f"[profile] Tracy enabled via {RSLOOP_PROFILE_ENV}=1 for {args.loop}/{args.workload}",
                flush=True,
            )
        if profile_requested:
            if args.loop != "rsloop":
                raise RuntimeError("profiling is only supported for rsloop")
            rsloop = importlib.import_module("rsloop")
            if args.profile_label:
                print(
                    f"[profile] Tracy session label: {args.profile_label}", flush=True
                )
            with rsloop.profile():
                result = run_with_loop(args.loop, coro)
        else:
            result = run_with_loop(args.loop, coro)
    finally:
        gc.enable()

    result = replace(
        result,
        baseline_rss_bytes=baseline_rss_bytes,
        peak_rss_bytes=get_peak_rss_bytes(),
        peak_rss_delta_bytes=0,
    )
    result = replace(
        result,
        peak_rss_delta_bytes=max(0, result.peak_rss_bytes - result.baseline_rss_bytes),
    )

    print(
        json.dumps(
            {
                "loop": result.loop,
                "workload": result.workload,
                "seconds": result.seconds,
                "operations": result.operations,
                "ops_per_sec": result.ops_per_sec,
                "baseline_rss_bytes": result.baseline_rss_bytes,
                "peak_rss_bytes": result.peak_rss_bytes,
                "peak_rss_delta_bytes": result.peak_rss_delta_bytes,
            }
        )
    )
    return 0


def run_child(
    script_path: str,
    loop_name: str,
    workload: str,
    args: argparse.Namespace,
    *,
    profile_label: str | None = None,
) -> ChildResult:
    cmd = [
        sys.executable,
        script_path,
        "--child",
        "--loop",
        loop_name,
        "--workload",
        workload,
        "--callbacks",
        str(args.callbacks),
        "--tasks",
        str(args.tasks),
        "--task-batch-size",
        str(args.task_batch_size),
        "--tcp-roundtrips",
        str(args.tcp_roundtrips),
        "--payload-size",
        str(args.payload_size),
    ]
    if profile_label is not None:
        cmd.extend(["--profile-label", profile_label])
    env = os.environ.copy()
    if loop_name == "rsloop":
        env["RSLOOP_USE_FAST_STREAMS"] = "1" if args.rsloop_fast_streams else "0"
    proc = subprocess.run(
        cmd,
        check=False,
        capture_output=True,
        text=True,
        cwd=os.path.dirname(script_path),
        env=env,
    )
    if proc.returncode != 0:
        raise RuntimeError(
            f"{loop_name}/{workload} failed with exit code {proc.returncode}\n"
            f"stdout:\n{proc.stdout}\n"
            f"stderr:\n{proc.stderr}"
        )

    lines = [line for line in proc.stdout.splitlines() if line.strip()]
    if not lines:
        raise RuntimeError(f"{loop_name}/{workload} produced no output")
    payload = json.loads(lines[-1])
    return ChildResult(
        loop=payload["loop"],
        workload=payload["workload"],
        seconds=payload["seconds"],
        operations=payload["operations"],
        baseline_rss_bytes=payload.get("baseline_rss_bytes", 0),
        peak_rss_bytes=payload["peak_rss_bytes"],
        peak_rss_delta_bytes=payload.get("peak_rss_delta_bytes", 0),
    )


def print_workload_table(workload: str, runs: dict[str, list[ChildResult]]) -> None:
    rows: list[tuple[str, float, float, float, int, int, int, int]] = []
    for loop_name, loop_runs in runs.items():
        seconds = [item.seconds for item in loop_runs]
        baseline_rss_values = [item.baseline_rss_bytes for item in loop_runs]
        peak_rss_values = [item.peak_rss_bytes for item in loop_runs]
        peak_rss_delta_values = [item.peak_rss_delta_bytes for item in loop_runs]
        median_seconds = statistics.median(seconds)
        median_baseline_rss = int(statistics.median(baseline_rss_values))
        median_peak_rss = int(statistics.median(peak_rss_values))
        median_peak_rss_delta = int(statistics.median(peak_rss_delta_values))
        operations = loop_runs[0].operations
        ops_per_sec = (
            operations / median_seconds if median_seconds > 0 else float("inf")
        )
        rows.append(
            (
                loop_name,
                median_seconds,
                ops_per_sec,
                min(seconds),
                operations,
                median_baseline_rss,
                median_peak_rss,
                median_peak_rss_delta,
            )
        )

    rows.sort(key=lambda item: item[1])
    fastest = rows[0][1]

    print()
    print(f"{workload} ({rows[0][4]:,} ops)")
    if sys.platform == "linux":
        print(
            f"{'loop':<10} {'median_s':>12} {'best_s':>12} {'ops_per_s':>14} {'baseline_rss':>14} {'peak_rss':>12} {'peak_delta':>12} {'vs_fastest':>12}"
        )
    else:
        print(
            f"{'loop':<10} {'median_s':>12} {'best_s':>12} {'ops_per_s':>14} {'peak_rss':>12} {'vs_fastest':>12}"
        )
    for (
        loop_name,
        median_seconds,
        ops_per_sec,
        best_seconds,
        _,
        median_baseline_rss,
        median_peak_rss,
        median_peak_rss_delta,
    ) in rows:
        relative = median_seconds / fastest if fastest > 0 else 1.0
        if sys.platform == "linux":
            print(
                f"{loop_name:<10} "
                f"{median_seconds:>12.6f} "
                f"{best_seconds:>12.6f} "
                f"{ops_per_sec:>14,.0f} "
                f"{format_bytes(median_baseline_rss):>14} "
                f"{format_bytes(median_peak_rss):>12} "
                f"{format_bytes(median_peak_rss_delta):>12} "
                f"{relative:>11.2f}x"
            )
        else:
            print(
                f"{loop_name:<10} "
                f"{median_seconds:>12.6f} "
                f"{best_seconds:>12.6f} "
                f"{ops_per_sec:>14,.0f} "
                f"{format_bytes(median_peak_rss):>12} "
                f"{relative:>11.2f}x"
            )


def parent_main(args: argparse.Namespace) -> int:
    selected_loops = normalize_csv(args.loops, allowed=LOOP_CHOICES, label="loops")
    selected_workloads = normalize_csv(
        args.workloads, allowed=WORKLOAD_CHOICES, label="workloads"
    )
    profile_rsloop_dir = (
        os.path.abspath(args.profile_rsloop_dir) if args.profile_rsloop_dir else None
    )
    env_profile_enabled = env_flag(RSLOOP_PROFILE_ENV)

    available_loops: list[str] = []
    skipped_loops: dict[str, str] = {}
    for loop_name in selected_loops:
        ok, reason = is_loop_available(loop_name)
        if ok:
            available_loops.append(loop_name)
        else:
            skipped_loops[loop_name] = reason or "unknown error"

    if skipped_loops:
        print("Skipping unavailable loops:")
        for loop_name, reason in skipped_loops.items():
            print(f"  - {loop_name}: {reason}")

    if not available_loops:
        raise SystemExit("no benchmarkable loops are available")

    script_path = os.path.abspath(__file__)
    all_results: list[dict[str, object]] = []

    if env_profile_enabled:
        print(f"Tracy profiling enabled via {RSLOOP_PROFILE_ENV}=1")

    for workload in selected_workloads:
        workload_runs: dict[str, list[ChildResult]] = {}
        if workload == "tcp_streams":
            stream_mode = (
                "rsloop native fast streams"
                if args.rsloop_fast_streams
                else "stdlib asyncio streams"
            )
            print(f"tcp_streams mode: {stream_mode}")
        for loop_name in available_loops:
            print(f"Running {workload} on {loop_name}...")
            if profile_rsloop_dir and loop_name == "rsloop":
                os.makedirs(profile_rsloop_dir, exist_ok=True)
                profile_label = os.path.join(profile_rsloop_dir, f"rsloop-{workload}")
                print(f"  starting Tracy session labeled {profile_label}")
                run_child(
                    script_path,
                    loop_name,
                    workload,
                    args,
                    profile_label=profile_label,
                )
            for _ in range(args.warmups):
                run_child(script_path, loop_name, workload, args)
            measured = [
                run_child(script_path, loop_name, workload, args)
                for _ in range(args.repeat)
            ]
            workload_runs[loop_name] = measured
            all_results.append(
                {
                    "workload": workload,
                    "loop": loop_name,
                    "runs": [
                        {
                            "seconds": item.seconds,
                            "operations": item.operations,
                            "ops_per_sec": item.ops_per_sec,
                            "baseline_rss_bytes": item.baseline_rss_bytes,
                            "peak_rss_bytes": item.peak_rss_bytes,
                            "peak_rss_delta_bytes": item.peak_rss_delta_bytes,
                        }
                        for item in measured
                    ],
                }
            )
        print_workload_table(workload, workload_runs)

    if args.json_output:
        with open(args.json_output, "w", encoding="utf-8") as f:
            json.dump(all_results, f, indent=2)
        print()
        print(f"Wrote raw results to {args.json_output}")

    return 0


def main() -> int:
    args = parse_args()
    validate_args(args)
    if args.child:
        return child_main(args)
    return parent_main(args)


if __name__ == "__main__":
    raise SystemExit(main())
