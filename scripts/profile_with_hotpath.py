"""Collect complete hotpath reports, one fresh process per workload.

Build with ``maturin develop --release --features profile`` first.
Use ``all --output target/hotpath-rs/run`` for the coverage suite, or profile
an application/test module with ``module --module NAME --module-args ...``.
"""

from __future__ import annotations

import argparse
import gc
import hashlib
import json
import math
import os
import platform
import runpy
import subprocess
import sys
import time
from dataclasses import asdict
from pathlib import Path
from typing import Any, cast

ROOT = Path(__file__).resolve().parents[1]
BASIC = ("callbacks", "tasks", "task_options", "tcp_streams")
TIMERS = ("timers_retained", "timers_discarded", "timers_cancelled", "timers_mixed")
MATRIX = (
    "http_keepalive",
    "tls_http",
    "websocket_messages",
    "websocket_tls",
    "mixed_streams",
    "bulk_transfer",
    "idle_connections",
)
PROBES = (
    "threadsafe",
    "executor_dns",
    "udp",
    "socket_ops",
    "subprocess_pipes",
    "unix_streams",
    "signals",
    "cancellation",
)
WORKLOADS = (*BASIC, *TIMERS, "tcp_connect_churn", *MATRIX, *PROBES)


def positive(value: str) -> int:
    number = int(value)
    if number <= 0:
        raise argparse.ArgumentTypeError("must be positive")
    return number


def parse_args(argv=None):
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("workload", choices=(*WORKLOADS, "all", "module"))
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument(
        "--format", choices=("json", "json-pretty", "table"), default="json"
    )
    parser.add_argument("--focus", help="hotpath function name substring (or /regex/)")
    parser.add_argument("--time-sampling-rate", type=float, default=1.0)
    parser.add_argument("--timeout", type=float, default=120.0)
    parser.add_argument("--callbacks", type=positive, default=20_000)
    parser.add_argument("--tasks", type=positive, default=5_000)
    parser.add_argument("--task-batch-size", type=positive, default=500)
    parser.add_argument("--tcp-roundtrips", type=positive, default=500)
    parser.add_argument("--payload-size", type=positive, default=1024)
    parser.add_argument("--timers", type=positive, default=5_000)
    parser.add_argument("--timer-batch-size", type=positive, default=500)
    parser.add_argument("--connections", type=positive, default=50)
    parser.add_argument("--probe-iterations", type=positive, default=20)
    parser.add_argument("--concurrency", type=positive, default=4)
    parser.add_argument("--requests-per-connection", type=positive, default=20)
    parser.add_argument("--bulk-bytes", type=positive, default=1024 * 1024)
    parser.add_argument("--idle-connections", type=positive, default=20)
    parser.add_argument("--idle-cycles", type=positive, default=10)
    parser.add_argument("--tls-dir", type=Path, default=ROOT / "tests/fixtures/tls")
    parser.add_argument("--module", help="Python module to run inside the profile")
    parser.add_argument("--child", action="store_true", help=argparse.SUPPRESS)
    parser.add_argument("--module-args", nargs=argparse.REMAINDER, default=[])
    args = parser.parse_args(argv)
    if (
        not math.isfinite(args.time_sampling_rate)
        or not 0 < args.time_sampling_rate <= 1
    ):
        parser.error("--time-sampling-rate must be in (0, 1]")
    if not math.isfinite(args.timeout) or args.timeout <= 0:
        parser.error("--timeout must be finite and positive")
    if args.workload == "all" and args.format == "table":
        parser.error("all requires JSON reports for coverage analysis")
    if (args.workload == "module") != bool(args.module):
        parser.error(
            "module workload requires --module; other workloads do not accept it"
        )
    return args


def profile_environment(args):
    # hotpath env settings override the builder and some are cached at import.
    # Clear stale filters, row limits, output paths and sampling configuration.
    env = {k: v for k, v in os.environ.items() if not k.startswith("HOTPATH_")}
    env.update(HOTPATH_LIMIT="0", HOTPATH_THREADS_INTERVAL_MS="10", PYTHONHASHSEED="0")
    if args.focus:
        env["HOTPATH_FOCUS"] = args.focus
    return env


def write_json(path, data):
    path.write_text(json.dumps(data, indent=2, sort_keys=True) + "\n")


def sha256(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def inventory():
    result = subprocess.run(
        [
            "cargo",
            "run",
            "--quiet",
            "--locked",
            "--manifest-path",
            str(ROOT / "tools/hotpath-coverage/Cargo.toml"),
            "--",
            "--json",
        ],
        cwd=ROOT,
        capture_output=True,
        text=True,
        check=True,
    )
    return json.loads(result.stdout)


def report_names(report):
    for section, values in report.items():
        if (
            isinstance(values, dict)
            and "total_count" in values
            and values["total_count"] != values.get("included_count")
        ):
            raise ValueError(f"{section} is truncated; remove hotpath row limits")
    names = set()
    for section in ("functions_timing", "functions_alloc"):
        values = report.get(section) or {}
        names.update(row["name"] for row in values.get("data", []))
    if not names:
        raise ValueError("hotpath report contains no measured functions")
    return names


def coverage(entries, reports):
    observed = {name for report in reports for name in report_names(report)}
    rows = []
    for entry in entries:
        row = dict(entry)
        row["observed"] = row["name"] in observed
        rows.append(row)
    eligible = {r["name"] for r in rows if r["status"] == "instrumented"}
    return {
        "instrumented_names": len(eligible),
        "observed_names": len(eligible & observed),
        "unmapped_observed_names": sorted(observed - eligible - {"rsloop"}),
        "note": "Static inventory includes inactive platforms/features. Unobserved does not mean unused or cheap. Times are inclusive, not CPU self time.",
        "functions": rows,
    }


def summary(manifest, coverage_data, reports):
    lines = [
        "# rsloop hotpath profile",
        "",
        (
            "Diagnostic instrumentation changes runtime cost. Function times overlap; "
            "they are inclusive wall durations, not CPU self time. Compare performance "
            "using a separate uninstrumented build."
        ),
        "",
        (
            f"Observed {coverage_data['observed_names']} of "
            f"{coverage_data['instrumented_names']} instrumented names across all source "
            "platforms/features. See coverage.json for unobserved paths and exclusions."
        ),
        "",
        "| Workload | Status | Function rows | Future rows | Thread rows |",
        "| --- | --- | ---: | ---: | ---: |",
    ]
    successful = iter(reports)
    for run in manifest["runs"]:
        if run["status"] != "ok":
            lines.append(f"| {run['workload']} | {run['status']} | — | — | — |")
            continue
        report = next(successful)
        functions = report.get("functions_timing", report.get("functions_alloc", {}))
        lines.append(
            f"| [{run['workload']}]({Path(run['report']).name}) | ok | "
            f"{functions.get('included_count', 0)} | "
            f"{len(report.get('futures', {}).get('data', []))} | "
            f"{report.get('threads', {}).get('included_count', 0)} |"
        )
    return "\n".join(lines) + "\n"


def run_workload(args):
    import asyncio

    import rsloop

    sys.path.insert(0, str(ROOT / "benches"))
    import compare_event_loops as small
    import profile_workloads as probes
    import scheduler_workloads as scheduler
    import workload_matrix as matrix

    name = args.workload
    if name == "module":
        sys.argv = [args.module, *args.module_args]
        try:
            runpy.run_module(args.module, run_name="__main__", alter_sys=True)
        except SystemExit as exc:
            if exc.code not in (None, 0):
                raise
        return {"module": args.module, "arguments": args.module_args}
    if name == "callbacks":
        coro = small.bench_callbacks("rsloop", args.callbacks)
    elif name in ("tasks", "task_options"):
        runner = small.bench_tasks if name == "tasks" else small.bench_task_options
        coro = runner("rsloop", args.tasks, args.task_batch_size)
    elif name == "tcp_streams":
        coro = small.bench_tcp_streams("rsloop", args.tcp_roundtrips, args.payload_size)
    elif name in TIMERS:
        coro = scheduler.bench_timers(
            "rsloop", args.timers, args.timer_batch_size, name
        )
    elif name == "tcp_connect_churn":
        coro = scheduler.bench_tcp_connect_churn(
            "rsloop", args.connections, args.payload_size
        )
    elif name in PROBES:
        coro = probes.RUNNERS[name](args.probe_iterations)
    else:
        old_argv = sys.argv
        sys.argv = ["matrix"]
        try:
            matrix_args = matrix.parse_args()
        finally:
            sys.argv = old_argv
        for key in (
            "concurrency",
            "requests_per_connection",
            "bulk_bytes",
            "idle_connections",
            "idle_cycles",
            "tls_dir",
        ):
            setattr(matrix_args, key, getattr(args, key))
        matrix.validate_args(matrix_args)
        coro = matrix.SCENARIO_RUNNERS[name]("rsloop", matrix_args)
    # Include loop construction, shutdown, worker joins, and cancellation.
    result = rsloop.run(asyncio.wait_for(coro, args.timeout))
    return (
        asdict(result) if result is not None else {"iterations": args.probe_iterations}
    )


def child(args):
    import rsloop
    from rsloop import _loop

    native = cast(Any, _loop)
    if not hasattr(native, "hotpath_start"):
        raise RuntimeError("build rsloop with --features profile first")
    extension = Path(native.__file__).resolve()
    metadata = {
        "workload": args.workload,
        "pid": os.getpid(),
        "python": sys.version,
        "platform": platform.platform(),
        "extension": str(extension),
        "extension_sha256": sha256(extension),
        "build_info": rsloop.build_info(),
        "settings": {
            k: str(v) if isinstance(v, Path) else v for k, v in vars(args).items()
        },
        "environment": {
            k: v for k, v in os.environ.items() if k.startswith(("HOTPATH_", "RSLOOP_"))
        },
        "scope": "cold workload, including loop setup/teardown; no in-process warmup",
    }
    gc.collect()
    gc_enabled = gc.isenabled()
    gc.disable()
    started = time.perf_counter()
    try:
        native.hotpath_start(
            str(args.output),
            format=args.format,
            time_sampling_rate=args.time_sampling_rate,
        )
        try:
            metadata["result"] = run_workload(args)
            metadata["status"] = "ok"
        finally:
            native.hotpath_stop()
    except BaseException as exc:
        metadata["status"] = "failed"
        metadata["error"] = repr(exc)
        raise
    finally:
        metadata["instrumented_seconds_with_report_flush"] = (
            time.perf_counter() - started
        )
        if gc_enabled:
            gc.enable()
        write_json(args.output.with_suffix(".metadata.json"), metadata)


def child_command(args, name, output):
    command = [
        sys.executable,
        str(Path(__file__).resolve()),
        name,
        "--child",
        "--output",
        str(output),
    ]
    for key, value in vars(args).items():
        if key in {"workload", "child", "output", "module_args"} or value is None:
            continue
        command.extend(["--" + key.replace("_", "-"), str(value)])
    if args.module_args:
        command.extend(["--module-args", *args.module_args])
    return command


def unsupported(name):
    if name == "unix_streams" and sys.platform == "win32":
        return "Unix stream APIs are unavailable on Windows"
    if name == "signals" and os.name != "posix":
        return "POSIX signal probe"
    return None


def main(argv=None):
    args = parse_args(argv)
    if args.child:
        child(args)
        return
    args.output = args.output.expanduser().resolve()
    output = args.output
    sidecars = (".metadata.json", ".manifest.json", ".coverage.json", ".log")
    if output.exists() or any(output.with_suffix(s).exists() for s in sidecars):
        raise SystemExit(f"refusing to overwrite {output}")
    entries = inventory()
    if args.workload == "all":
        output.mkdir(parents=True)
        names = WORKLOADS
    else:
        output.parent.mkdir(parents=True, exist_ok=True)
        names = (args.workload,)
    source_hashes = {
        str(p.relative_to(ROOT)): sha256(p) for p in sorted(ROOT.glob("src/**/*.rs"))
    }
    source_hashes.update(
        {
            str(p.relative_to(ROOT)): sha256(p)
            for directory in ("benches", "scripts", "python/rsloop")
            for p in sorted((ROOT / directory).glob("*.py"))
        }
    )
    source_hashes.update(
        {
            name: sha256(ROOT / name)
            for name in ("Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "build.rs")
        }
    )
    manifest = {
        "revision": subprocess.check_output(
            ["git", "rev-parse", "HEAD"], cwd=ROOT, text=True
        ).strip(),
        "git_status": subprocess.check_output(
            ["git", "status", "--porcelain"], cwd=ROOT, text=True
        ),
        "source_sha256": source_hashes,
        "note": "Source hashes describe the checkout at collection time; extension SHA-256 identifies the actual binary. Rebuild after source edits.",
        "runs": [],
    }
    manifest_path = (
        output / "manifest.json"
        if args.workload == "all"
        else output.with_suffix(".manifest.json")
    )
    reports = []
    failures = []
    write_json(manifest_path, manifest)
    for name in names:
        reason = unsupported(name)
        if reason:
            manifest["runs"].append(
                {"workload": name, "status": "skipped", "reason": reason}
            )
            write_json(manifest_path, manifest)
            continue
        report = output / f"{name}.json" if args.workload == "all" else output
        command = child_command(args, name, report)
        record = {"workload": name, "command": command, "report": str(report)}
        try:
            result = subprocess.run(
                command,
                cwd=ROOT,
                env=profile_environment(args),
                capture_output=True,
                text=True,
                timeout=args.timeout + 30,
                check=False,
            )
            report.with_suffix(".log").write_text(result.stdout + result.stderr)
            if result.returncode:
                raise RuntimeError(
                    f"child exited {result.returncode}; see {report.with_suffix('.log')}"
                )
            if args.format != "table":
                data = json.loads(report.read_text())
                report_names(data)
                reports.append(data)
            elif not report.is_file():
                raise RuntimeError("hotpath did not create a report")
            record["status"] = "ok"
            print(f"{name}: {report}", flush=True)
        except (OSError, ValueError, RuntimeError, subprocess.TimeoutExpired) as exc:
            if isinstance(exc, subprocess.TimeoutExpired):
                parts = [
                    p.decode(errors="replace") if isinstance(p, bytes) else (p or "")
                    for p in (exc.stdout, exc.stderr)
                ]
                report.with_suffix(".log").write_text(
                    "".join(parts) + "\nProfile child timed out.\n"
                )
            record.update(status="failed", error=str(exc))
            failures.append(name)
            print(f"{name}: failed: {exc}", file=sys.stderr, flush=True)
        manifest["runs"].append(record)
        write_json(manifest_path, manifest)
    if reports:
        destination = (
            output / "coverage.json"
            if args.workload == "all"
            else output.with_suffix(".coverage.json")
        )
        coverage_data = coverage(entries, reports)
        write_json(destination, coverage_data)
        if args.workload == "all":
            (output / "summary.md").write_text(
                summary(manifest, coverage_data, reports)
            )
    if failures:
        raise SystemExit(
            f"profiling failed: {', '.join(failures)} (partial results retained)"
        )


if __name__ == "__main__":
    main()
