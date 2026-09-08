#!/usr/bin/env python3
"""Compare Granian HTTP performance across asyncio event loops using oha."""

from __future__ import annotations

import argparse
import importlib.metadata
import json
import math
import os
import shutil
import signal
import statistics
import subprocess
import sys
import time
from pathlib import Path
from typing import Any
from urllib.error import URLError
from urllib.request import Request, urlopen

LOOP_CHOICES = ("asyncio", "uvloop", "winloop", "rsloop")
ROOT = Path(__file__).resolve().parents[1]
SERVER = ROOT / "examples" / "granian_service.py"


def default_loops_csv() -> str:
    platform_loop = "winloop" if sys.platform == "win32" else "uvloop"
    return f"asyncio,{platform_loop},rsloop"


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description="Benchmark Granian on asyncio, uvloop/winloop, and rsloop."
    )
    parser.add_argument(
        "--loops",
        default=default_loops_csv(),
        help="Comma-separated loops (default: platform stdlib, optimized loop, rsloop).",
    )
    parser.add_argument("--host", default="127.0.0.1")
    parser.add_argument("--port", type=int, default=8000)
    parser.add_argument("--workers", type=int, default=1)
    parser.add_argument("--runtime-threads", type=int, default=1)
    parser.add_argument("--backpressure", type=int, default=1024)
    parser.add_argument(
        "--task-impl",
        choices=("asyncio", "rust"),
        default="asyncio",
        help="Granian task implementation used for every selected loop.",
    )
    parser.add_argument("--concurrency", type=int, default=128)
    parser.add_argument(
        "--warmup-duration", type=float, default=3.0, help="Warmup seconds per loop"
    )
    parser.add_argument(
        "--duration", type=float, default=10.0, help="Seconds per measured run"
    )
    parser.add_argument("--repeat", type=int, default=3)
    parser.add_argument("--startup-timeout", type=float, default=15.0)
    parser.add_argument("--json-output", type=Path)
    return parser.parse_args()


def selected_loops(value: str) -> list[str]:
    names = [item.strip() for item in value.split(",") if item.strip()]
    if not names:
        raise SystemExit("no loops selected")
    invalid = [name for name in names if name not in LOOP_CHOICES]
    if invalid:
        raise SystemExit(f"invalid loops: {', '.join(invalid)}")
    if len(names) != len(set(names)):
        raise SystemExit("--loops contains duplicates")
    return names


def validate_args(args: argparse.Namespace) -> None:
    positive = {
        "--port": args.port,
        "--workers": args.workers,
        "--runtime-threads": args.runtime_threads,
        "--backpressure": args.backpressure,
        "--concurrency": args.concurrency,
        "--duration": args.duration,
        "--repeat": args.repeat,
        "--startup-timeout": args.startup_timeout,
    }
    for option, value in positive.items():
        if not math.isfinite(value) or value <= 0:
            raise SystemExit(f"{option} must be > 0")
    if not math.isfinite(args.warmup_duration) or args.warmup_duration < 0:
        raise SystemExit("--warmup-duration must be >= 0")
    if args.port > 65535:
        raise SystemExit("--port must be <= 65535")


def server_command(args: argparse.Namespace, loop_name: str) -> list[str]:
    return [
        sys.executable,
        str(SERVER),
        "--event-loop",
        loop_name,
        "--host",
        args.host,
        "--port",
        str(args.port),
        "--workers",
        str(args.workers),
        "--runtime-threads",
        str(args.runtime_threads),
        "--backpressure",
        str(args.backpressure),
        "--task-impl",
        args.task_impl,
        "--no-log",
    ]


def start_server(args: argparse.Namespace, loop_name: str) -> subprocess.Popen[bytes]:
    if sys.platform == "win32":
        return subprocess.Popen(
            server_command(args, loop_name),
            cwd=ROOT,
            stdout=subprocess.DEVNULL,
            stderr=subprocess.PIPE,
            creationflags=subprocess.CREATE_NEW_PROCESS_GROUP,
        )
    return subprocess.Popen(
        server_command(args, loop_name),
        cwd=ROOT,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.PIPE,
        start_new_session=True,
    )


def stop_server(process: subprocess.Popen[bytes]) -> None:
    if sys.platform == "win32":
        if process.poll() is not None:
            return
        try:
            process.send_signal(signal.CTRL_BREAK_EVENT)
            process.wait(timeout=5)
        except (OSError, subprocess.TimeoutExpired):
            process.terminate()
            try:
                process.wait(timeout=3)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait()
        return

    process_group = process.pid
    if process.poll() is None:
        try:
            os.killpg(process_group, signal.SIGINT)
            process.wait(timeout=5)
        except (OSError, subprocess.TimeoutExpired):
            pass

    # Python 3.15 defaults to a forkserver start method. Granian's supervisor
    # can exit before that forkserver, its resource tracker, and its worker.
    # They inherit this dedicated process group, so explicitly reap anything
    # left after the graceful shutdown before the next loop binds the port.
    try:
        os.killpg(process_group, signal.SIGTERM)
    except ProcessLookupError:
        return
    deadline = time.monotonic() + 1
    while time.monotonic() < deadline:
        try:
            os.killpg(process_group, 0)
        except ProcessLookupError:
            break
        time.sleep(0.05)
    else:
        try:
            os.killpg(process_group, signal.SIGKILL)
        except ProcessLookupError:
            pass

    if process.poll() is None:
        process.wait()


def wait_until_ready(
    process: subprocess.Popen[bytes],
    url: str,
    loop_name: str,
    timeout: float,
) -> dict[str, str]:
    deadline = time.monotonic() + timeout
    last_error: BaseException | None = None
    while time.monotonic() < deadline:
        if process.poll() is not None:
            stderr = (process.stderr.read() if process.stderr else b"").decode(
                errors="replace"
            )
            raise RuntimeError(
                f"Granian exited before startup for {loop_name}: {stderr.strip()}"
            )
        try:
            request = Request(url, headers={"Connection": "close"})
            with urlopen(request, timeout=0.5) as response:
                payload = json.load(response)
            if payload.get("selected") != loop_name:
                raise RuntimeError(
                    f"Granian selected {payload.get('selected')!r}, expected {loop_name!r}"
                )
            module_name = payload.get("module", "")
            if module_name != loop_name and not module_name.startswith(f"{loop_name}."):
                raise RuntimeError(
                    f"Granian reported loop module {module_name!r}, "
                    f"expected {loop_name!r}"
                )
            return payload
        except (OSError, URLError, json.JSONDecodeError) as exc:
            last_error = exc
            time.sleep(0.05)
    raise RuntimeError(f"Granian did not start for {loop_name}") from last_error


def oha_command(url: str, *, concurrency: int, duration: float) -> list[str]:
    return [
        "oha",
        "--no-tui",
        "-c",
        str(concurrency),
        "-z",
        f"{duration:g}s",
        "--output-format",
        "json",
        url,
    ]


def parse_oha_output(output: str) -> dict[str, float | int]:
    data = json.loads(output)
    summary = data["summary"]
    percentiles = data["latencyPercentiles"]
    status_codes = data["statusCodeDistribution"]
    unexpected_statuses = {
        str(code): int(count)
        for code, count in status_codes.items()
        if str(code) != "200" and count
    }
    if unexpected_statuses:
        raise RuntimeError(f"oha received non-200 responses: {unexpected_statuses}")
    requests = int(status_codes.get("200", status_codes.get(200, 0)))
    if requests <= 0:
        raise RuntimeError("oha completed without any HTTP 200 responses")
    return {
        "requests": requests,
        "requests_per_second": float(summary["requestsPerSec"]),
        "average_ms": float(summary["average"]) * 1000,
        "p50_ms": float(percentiles["p50"]) * 1000,
        "p95_ms": float(percentiles["p95"]) * 1000,
        "p99_ms": float(percentiles["p99"]) * 1000,
        "max_ms": float(summary["slowest"]) * 1000,
    }


def run_oha(url: str, *, concurrency: int, duration: float) -> dict[str, float | int]:
    completed = subprocess.run(
        oha_command(url, concurrency=concurrency, duration=duration),
        check=False,
        capture_output=True,
        text=True,
    )
    if completed.returncode:
        raise RuntimeError(f"oha failed: {completed.stderr.strip()}")
    return parse_oha_output(completed.stdout)


def benchmark_loop(
    args: argparse.Namespace, loop_name: str
) -> tuple[dict[str, str], list[dict[str, float | int]]]:
    base_url = f"http://{args.host}:{args.port}"
    process = start_server(args, loop_name)
    try:
        identity = wait_until_ready(
            process, f"{base_url}/loop", loop_name, args.startup_timeout
        )
        print(
            f"{loop_name}: {identity['module']}.{identity['class']} (worker ready)",
            flush=True,
        )
        if args.warmup_duration:
            run_oha(
                f"{base_url}/benchmark",
                concurrency=args.concurrency,
                duration=args.warmup_duration,
            )
        runs: list[dict[str, float | int]] = []
        for run_number in range(1, args.repeat + 1):
            result = run_oha(
                f"{base_url}/benchmark",
                concurrency=args.concurrency,
                duration=args.duration,
            )
            runs.append(result)
            print(
                f"  run {run_number}/{args.repeat}: "
                f"{result['requests_per_second']:,.0f} req/s, "
                f"p99 {result['p99_ms']:.3f} ms",
                flush=True,
            )
        return identity, runs
    finally:
        stop_server(process)


def median(run_data: list[dict[str, float | int]], key: str) -> float:
    return statistics.median(float(run[key]) for run in run_data)


def print_summary(results: list[dict[str, Any]]) -> None:
    fastest = max(median(item["runs"], "requests_per_second") for item in results)
    print("\nloop            req/s     p50 ms     p95 ms     p99 ms   vs fastest")
    print("------------  ---------  ---------  ---------  ---------  ----------")
    for item in results:
        runs = item["runs"]
        rps = median(runs, "requests_per_second")
        print(
            f"{item['loop']:<12}  {rps:>9,.0f}  "
            f"{median(runs, 'p50_ms'):>9.3f}  "
            f"{median(runs, 'p95_ms'):>9.3f}  "
            f"{median(runs, 'p99_ms'):>9.3f}  "
            f"{rps / fastest:>9.2%}"
        )


def package_version(name: str) -> str | None:
    try:
        return importlib.metadata.version(name)
    except importlib.metadata.PackageNotFoundError:
        return None


def main() -> int:
    args = parse_args()
    validate_args(args)
    loop_names = selected_loops(args.loops)
    if shutil.which("oha") is None:
        raise SystemExit(
            "oha is required. Install it from https://github.com/hatoo/oha, then retry."
        )

    results: list[dict[str, Any]] = []
    for loop_name in loop_names:
        identity, runs = benchmark_loop(args, loop_name)
        results.append({"loop": loop_name, "identity": identity, "runs": runs})

    print_summary(results)
    if args.json_output:
        payload: dict[str, Any] = {
            "benchmark": "granian-event-loops-v1",
            "python": sys.version,
            "platform": sys.platform,
            "versions": {
                name: package_version(name)
                for name in ("granian", "uvloop", "winloop", "rsloop")
            },
            "settings": {
                "host": args.host,
                "port": args.port,
                "workers": args.workers,
                "runtime_threads": args.runtime_threads,
                "backpressure": args.backpressure,
                "task_impl": args.task_impl,
                "concurrency": args.concurrency,
                "warmup_duration": args.warmup_duration,
                "duration": args.duration,
                "repeat": args.repeat,
                "endpoint": "/benchmark",
                "body_bytes": 10 * 1024,
            },
            "results": results,
        }
        args.json_output.parent.mkdir(parents=True, exist_ok=True)
        args.json_output.write_text(json.dumps(payload, indent=2) + "\n")
        print(f"\nwrote {args.json_output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
