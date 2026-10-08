#!/usr/bin/env python3
"""Linux paired benchmarks; use one harness for every installed revision."""

from __future__ import annotations

import argparse
import asyncio
import gc
import hashlib
import importlib.metadata
import json
import os
import platform
import random
import statistics
import subprocess
import sys
import sysconfig
import time
from dataclasses import asdict
from pathlib import Path
from typing import Any

import compare_event_loops as bench


def distribution(values):
    ordered = sorted(values)
    median = statistics.median(ordered)
    return {
        "median": median,
        "min": ordered[0],
        "max": ordered[-1],
        "p95": ordered[max(0, (95 * len(ordered) + 99) // 100 - 1)],
        "p99": ordered[max(0, (99 * len(ordered) + 99) // 100 - 1)],
        "cv_percent": statistics.stdev(ordered) / abs(statistics.mean(ordered)) * 100
        if len(ordered) > 1 and statistics.mean(ordered) != 0
        else 0,
    }


async def tcp_latency(args):
    """Separate instrumented run: clock overhead never changes throughput runs."""
    payload = b"x" * args.payload_size

    async def echo(reader, writer):
        try:
            while data := await reader.read(65536):
                writer.write(data)
                await writer.drain()
        finally:
            writer.close()
            await bench.maybe_wait_closed(writer)

    server = await asyncio.start_server(echo, "127.0.0.1", 0)
    samples = []
    try:
        reader, writer = await asyncio.open_connection(
            *server.sockets[0].getsockname()[:2]
        )
        try:
            for index in range(args.tcp_roundtrips + 100):
                start = time.perf_counter_ns()
                writer.write(payload)
                await writer.drain()
                data = await reader.readexactly(len(payload))
                elapsed = time.perf_counter_ns() - start
                if data != payload:
                    raise RuntimeError("echo payload mismatch")
                if index >= 100:
                    samples.append(elapsed / 1000)
        finally:
            writer.close()
            await bench.maybe_wait_closed(writer)
    finally:
        server.close()
        await server.wait_closed()
    return {"rtt_us": distribution(samples), "samples_us": samples}


def worker(args):
    # Set affinity before importing native modules (their threads inherit it).
    os.sched_setaffinity(0, {args.cpu})
    bench.loop_factory_for(args.loop)
    gil = getattr(sys, "_is_gil_enabled", lambda: True)()
    if bool(sysconfig.get_config_var("Py_GIL_DISABLED")) != (
        args.mode == "free-threaded"
    ):
        raise RuntimeError("unexpected Python build")
    if gil != (args.mode == "gil"):
        raise RuntimeError(f"{args.loop} has unexpected GIL state: {gil}")
    metadata = {
        "python": sys.version,
        "executable": sys.executable,
        "free_threaded_build": bool(sysconfig.get_config_var("Py_GIL_DISABLED")),
        "gil_enabled_after_import": gil,
        "affinity": sorted(os.sched_getaffinity(0)),
        "packages": {
            name: importlib.metadata.version(name)
            for name in ("rsloop", "uvloop", "zuvloop")
        },
    }
    gc.collect()
    gc.disable()
    baseline = bench.get_current_rss_bytes()
    try:
        result: dict[str, Any]
        if args.workload == "tcp_latency":
            result = asyncio.run(
                tcp_latency(args), loop_factory=bench.loop_factory_for(args.loop)
            )
        else:
            coroutine = {
                "callbacks": lambda: bench.bench_callbacks(args.loop, args.callbacks),
                "tasks": lambda: bench.bench_tasks(
                    args.loop, args.tasks, args.task_batch_size
                ),
                "tcp_streams": lambda: bench.bench_tcp_streams(
                    args.loop, args.tcp_roundtrips, args.payload_size
                ),
            }[args.workload]()
            measured = bench.run_with_loop(args.loop, coroutine)
            result = asdict(measured)
            result["ops_per_sec"] = measured.ops_per_sec
    finally:
        gc.enable()
    if getattr(sys, "_is_gil_enabled", lambda: True)() != gil:
        raise RuntimeError("GIL state changed during workload")
    result.update(
        metadata=metadata,
        baseline_rss_bytes=baseline,
        peak_rss_bytes=bench.get_peak_rss_bytes(),
    )
    result["peak_rss_delta_bytes"] = max(0, result["peak_rss_bytes"] - baseline)
    return result


def summarize(runs):
    rows = []
    for workload in ("callbacks", "tasks", "tcp_streams", "tcp_latency"):
        labels = sorted({r["label"] for r in runs if r["workload"] == workload})
        for label in labels:
            subset = [
                r["result"]
                for r in runs
                if r["label"] == label and r["workload"] == workload
            ]
            row = {
                "label": label,
                "workload": workload,
                "peak_rss_bytes": distribution([r["peak_rss_bytes"] for r in subset]),
            }
            if workload == "tcp_latency":
                row["rtt_us"] = {
                    key: distribution([r["rtt_us"][key] for r in subset])
                    for key in ("median", "p95", "p99")
                }
            else:
                row["seconds"] = distribution([r["seconds"] for r in subset])
                row["ops_per_sec"] = distribution([r["ops_per_sec"] for r in subset])
            rows.append(row)
    return rows


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--env-root", type=Path)
    parser.add_argument("--output", type=Path)
    parser.add_argument("--mode", choices=("gil", "free-threaded"), required=True)
    parser.add_argument("--cpu", type=int, default=min(os.sched_getaffinity(0)))
    parser.add_argument("--warmups", type=int, default=2)
    parser.add_argument("--repeat", type=int, default=9)
    parser.add_argument("--seed", type=int, default=101)
    parser.add_argument("--callbacks", type=int, default=1000000)
    parser.add_argument("--tasks", type=int, default=200000)
    parser.add_argument("--task-batch-size", type=int, default=5000)
    parser.add_argument("--tcp-roundtrips", type=int, default=20000)
    parser.add_argument("--payload-size", type=int, default=1024)
    parser.add_argument("--worker", action="store_true")
    parser.add_argument("--loop", choices=("rsloop", "uvloop", "zuvloop"))
    parser.add_argument(
        "--workload", choices=("callbacks", "tasks", "tcp_streams", "tcp_latency")
    )
    args = parser.parse_args()
    if (
        args.repeat < 2
        or args.warmups < 0
        or any(
            getattr(args, key) <= 0
            for key in (
                "callbacks",
                "tasks",
                "task_batch_size",
                "tcp_roundtrips",
                "payload_size",
            )
        )
    ):
        parser.error("repeat must be >=2, warmups >=0 and workloads positive")
    if args.worker:
        print(json.dumps(worker(args)))
        return
    if args.env_root is None or args.output is None:
        parser.error("--env-root and --output are required")
    args.output.mkdir(parents=True, exist_ok=True)
    cases = [(label, "rsloop", label) for label in ("historical", "pre", "current")]
    cases += [(loop, loop, "current") for loop in ("uvloop", "zuvloop")]
    environment = {
        "platform": platform.platform(),
        "cpu": Path("/proc/cpuinfo").read_text(),
        "parameters": {
            k: str(v) if isinstance(v, Path) else v for k, v in vars(args).items()
        },
        "harness_sha256": {
            path.name: hashlib.sha256(path.read_bytes()).hexdigest()
            for path in (Path(__file__), Path(bench.__file__))
        },
        "revisions": json.loads((args.env_root / "revisions.json").read_text()),
        "environment": {
            k: os.environ.get(k)
            for k in ("PYTHONHASHSEED", "PYTHON_GIL", "RUSTUP_TOOLCHAIN", "RUSTFLAGS")
        },
    }
    (args.output / "environment.json").write_text(json.dumps(environment, indent=2))
    rng = random.Random(args.seed)
    runs = []
    with (args.output / "raw.jsonl").open("w") as raw:
        for workload in ("callbacks", "tasks", "tcp_streams", "tcp_latency"):
            for iteration in range(-args.warmups, args.repeat):
                order = cases.copy()
                rng.shuffle(order)
                for label, loop, env_label in order:
                    command = [
                        str(args.env_root.absolute() / env_label / "bin/python"),
                        "-X",
                        "context_aware_warnings=0",
                        str(Path(__file__).resolve()),
                        "--worker",
                        "--loop",
                        loop,
                        "--workload",
                        workload,
                        "--mode",
                        args.mode,
                        "--cpu",
                        str(args.cpu),
                    ]
                    for name in (
                        "callbacks",
                        "tasks",
                        "task_batch_size",
                        "tcp_roundtrips",
                        "payload_size",
                    ):
                        command += [
                            "--" + name.replace("_", "-"),
                            str(getattr(args, name)),
                        ]
                    env = os.environ.copy()
                    env.pop("PYTHONPATH", None)
                    # Do not force the GIL off to conceal an incompatible extension.
                    env.pop("PYTHON_GIL", None)
                    proc = subprocess.run(
                        command,
                        capture_output=True,
                        text=True,
                        check=False,
                        timeout=180,
                        env=env,
                    )
                    if proc.returncode:
                        (args.output / "failure.json").write_text(
                            json.dumps(
                                {
                                    "command": command,
                                    "stdout": proc.stdout,
                                    "stderr": proc.stderr,
                                    "exit_code": proc.returncode,
                                },
                                indent=2,
                            )
                        )
                        raise RuntimeError(f"{label}/{workload} failed: {proc.stderr}")
                    result = json.loads(proc.stdout.splitlines()[-1])
                    row = {
                        "label": label,
                        "workload": workload,
                        "iteration": iteration,
                        "result": result,
                        "stderr": proc.stderr,
                    }
                    raw.write(json.dumps(row) + "\n")
                    raw.flush()
                    print(
                        f"{args.mode}: {workload}/{label} round {iteration}", flush=True
                    )
                    if iteration >= 0:
                        runs.append(row)
    versions = {r["result"]["metadata"]["python"] for r in runs}
    if len(versions) != 1:
        raise RuntimeError("comparison mixed Python builds")
    summary = summarize(runs)
    (args.output / "summary.json").write_text(json.dumps(summary, indent=2))
    lines = [
        f"# Paired event-loop comparison ({args.mode})",
        "",
        "Fresh processes, one CPU; GC disabled. RTT is measured separately.",
        "",
        "| Workload | Loop/revision | Median ms | Ops/s | CV % | Peak RSS MiB |",
        "|---|---|---:|---:|---:|---:|",
    ]
    for row in summary:
        if row["workload"] == "tcp_latency":
            continue
        seconds = row["seconds"]
        lines.append(
            f"| {row['workload']} | {row['label']} | {seconds['median'] * 1000:.3f} | "
            f"{row['ops_per_sec']['median']:.0f} | {seconds['cv_percent']:.2f} | "
            f"{row['peak_rss_bytes']['median'] / 2**20:.2f} |"
        )
    lines += [
        "",
        "| Revision | RTT median µs | RTT p95 µs | RTT p99 µs |",
        "|---|---:|---:|---:|",
    ]
    for row in summary:
        if row["workload"] == "tcp_latency":
            d = row["rtt_us"]
            lines.append(
                f"| {row['label']} | {d['median']['median']:.2f} | "
                f"{d['p95']['median']:.2f} | {d['p99']['median']:.2f} |"
            )
    lines += ["", "Current versus pre-migration (positive time/RSS means worse):", ""]
    for workload in ("callbacks", "tasks", "tcp_streams", "tcp_latency"):
        for metric in (
            ("peak_rss_bytes", "seconds")
            if workload != "tcp_latency"
            else ("peak_rss_bytes",)
        ):
            values = {
                r["label"]: r[metric]["median"]
                for r in summary
                if r["workload"] == workload
            }
            lines.append(
                f"- {workload} {metric}: {(values['current'] / values['pre'] - 1) * 100:+.2f}%"
            )
    deltas = []
    for workload in ("callbacks", "tasks", "tcp_streams", "tcp_latency"):
        metrics = (
            ("peak_rss_bytes", "rtt_median", "rtt_p95", "rtt_p99")
            if workload == "tcp_latency"
            else ("seconds", "ops_per_sec", "peak_rss_bytes")
        )
        for metric in metrics:
            paired = []
            for iteration in range(args.repeat):
                pair = {
                    r["label"]: r["result"]
                    for r in runs
                    if r["workload"] == workload and r["iteration"] == iteration
                }

                def value(label, pair=pair, metric=metric):
                    result = pair[label]
                    return (
                        result["rtt_us"][metric.removeprefix("rtt_")]
                        if metric.startswith("rtt_")
                        else result[metric]
                    )

                paired.append((value("current") / value("pre") - 1) * 100)
            delta = distribution(paired)
            deltas.append(
                {
                    "workload": workload,
                    "metric": metric,
                    "paired_percent_changes": paired,
                    "distribution": delta,
                }
            )
            lines.append(
                f"- Paired {workload} {metric}: median {delta['median']:+.2f}%, range {delta['min']:+.2f}…{delta['max']:+.2f}%"
            )
    (args.output / "paired-deltas.json").write_text(json.dumps(deltas, indent=2))
    lines += [
        "",
        "Hosted runners are noisy: inspect min/max/CV and raw paired rounds;",
        "repeat complete runs before attributing a small difference to smol.",
    ]
    (args.output / "summary.md").write_text("\n".join(lines) + "\n")


if __name__ == "__main__":
    main()
