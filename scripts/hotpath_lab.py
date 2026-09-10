"""Isolated Rust/LLVM experiments: build, profile, inspect, test, and compare.

Use the existing project Python, not `uv run`, to keep its ABI/environment fixed.
No command installs a wheel, replaces the development extension, or edits Rust.
"""

from __future__ import annotations

import argparse
import gc
import hashlib
import importlib
import json
import math
import os
import platform
import random
import re
import shutil
import statistics
import subprocess
import sys
import sysconfig
import time
import zipfile
from dataclasses import asdict
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SCHEMA = 1
WORKLOADS = (
    "callbacks",
    "tasks",
    "tcp_streams",
    "http_keepalive",
    "mixed_streams",
    "bulk_transfer",
    "tls_http",
    "websocket_messages",
)
DEFAULT_WORKLOADS = ",".join(WORKLOADS[:6])


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def write_json(path: Path, value: object) -> None:
    path.write_text(
        json.dumps(value, indent=2, allow_nan=False) + "\n", encoding="utf-8"
    )


def read_json(path: Path):
    return json.loads(path.read_text(encoding="utf-8"))


def command_output(command: list[str], **kwargs) -> str:
    return subprocess.check_output(command, text=True, **kwargs).strip()


def inventory(directory: Path) -> dict[str, str]:
    return {
        path.relative_to(directory).as_posix(): sha256(path)
        for path in sorted(directory.rglob("*"))
        if path.is_file() and "__pycache__" not in path.parts
    }


def verify_inventory(directory: Path, expected: dict[str, str]) -> None:
    if inventory(directory) != expected:
        raise ValueError(f"Artifact files changed: {directory}")


def load_artifact(directory: Path, *, source: bool = False) -> dict:
    manifest = read_json(directory / "manifest.json")
    if manifest["schema"] != SCHEMA:
        raise ValueError("Unsupported artifact schema")
    if (
        manifest["python"] != sys.version
        or manifest["python_executable"] != sys.executable
    ):
        raise ValueError("Use the exact Python interpreter that built the artifact")
    verify_inventory(directory / "package", manifest["package_files"])
    if source:
        verify_inventory(Path(manifest["source"]), manifest["source_files"])
    return manifest


def clean_environment() -> dict[str, str]:
    env = os.environ.copy()
    for key in list(env):
        if key.startswith("RSLOOP_") or key in {
            "RUSTFLAGS",
            "CARGO_ENCODED_RUSTFLAGS",
            "LLVM_PROFILE_FILE",
            "PYTHONPATH",
            "PYTHONHOME",
            "PYO3_CONFIG_FILE",
            "PYO3_CROSS",
            "PYO3_NO_PYTHON",
        }:
            env.pop(key)
    env.update(
        PYO3_PYTHON=sys.executable, PYTHONHASHSEED="0", PYTHONDONTWRITEBYTECODE="1"
    )
    return env


def llvm_tool(name: str) -> Path:
    host = command_output(["rustc", "-vV"]).split("host: ")[1].splitlines()[0]
    root = Path(command_output(["rustc", "--print", "sysroot"]))
    tool = (
        root
        / "lib"
        / "rustlib"
        / host
        / "bin"
        / (name + (".exe" if os.name == "nt" else ""))
    )
    if not tool.is_file():
        raise ValueError(
            "Install Rust's matching tools: rustup component add llvm-tools-preview"
        )
    return tool


def snapshot(revision: str, out: Path) -> tuple[str, Path]:
    commit = command_output(
        ["git", "rev-parse", "--verify", f"{revision}^{{commit}}"], cwd=ROOT
    )
    archive = out / "source.zip"
    subprocess.run(
        ["git", "archive", "--format=zip", f"--output={archive}", commit],
        cwd=ROOT,
        check=True,
    )
    source = out / "source"
    source.mkdir()
    with zipfile.ZipFile(archive) as bundle:
        for member in bundle.infolist():
            target = (source / member.filename).resolve()
            if (
                not target.is_relative_to(source)
                or (member.external_attr >> 16) & 0o170000 == 0o120000
            ):
                raise ValueError(f"Unsafe archive member: {member.filename}")
        bundle.extractall(source)
    return commit, source


def build(args) -> None:
    out = args.out.resolve()
    out.mkdir(parents=True, exist_ok=False)
    if args.source_artifact:
        previous = load_artifact(args.source_artifact.resolve(), source=True)
        commit, source = previous["commit"], Path(previous["source"])
    else:
        commit, source = snapshot(args.revision or "HEAD", out)
    rustc = command_output(["rustc", "-vV"])
    host = rustc.split("host: ")[1].splitlines()[0]
    mode = "generate" if args.instrument else "use" if args.pgo else "release"
    # Explicit --target keeps instrumentation out of build scripts/proc macros.
    flags = ["-Cdebuginfo=1", f"--remap-path-prefix={source}=/rsloop"]
    profile = None
    if args.instrument:
        flags.append(f"-Cprofile-generate={out / 'unassigned-profiles'}")
    elif args.pgo:
        training = read_json(args.pgo.resolve() / "training.json")
        origin = load_artifact(Path(training["artifact"]), source=True)
        if (
            origin["source_files"] != inventory(source)
            or origin["rustc"] != rustc
            or origin["python"] != sys.version
        ):
            raise ValueError(
                "PGO training must match source, Rust toolchain, and Python ABI"
            )
        profile = args.pgo.resolve() / "merged.profdata"
        if sha256(profile) != training["profile_sha256"]:
            raise ValueError("PGO profile changed after training")
        flags += [f"-Cprofile-use={profile}", "-Cllvm-args=-pgo-warn-missing-function"]
    env = clean_environment()
    env["CARGO_ENCODED_RUSTFLAGS"] = "\x1f".join(flags)
    target = ROOT / "target" / "hotpath-cache"
    command = [
        "cargo",
        "rustc",
        "--manifest-path",
        str(source / "Cargo.toml"),
        "--release",
        "--locked",
        "--lib",
        "--target",
        host,
        "--target-dir",
        str(target),
        "--",
        "--emit=link,llvm-ir,asm",
    ]
    manifest = {
        "schema": SCHEMA,
        "commit": commit,
        "source": str(source),
        "source_files": inventory(source),
        "python": sys.version,
        "python_executable": sys.executable,
        "rustc": rustc,
        "cargo": command_output(["cargo", "-V"]),
        "platform": platform.platform(),
        "mode": mode,
        "flags": flags,
        "command": command,
        "profile_sha256": sha256(profile) if profile else None,
        "started_utc": time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime()),
    }
    write_json(out / "build-request.json", manifest)
    print(f"Building {commit[:12]} ({mode}); log: {out / 'build.log'}", flush=True)
    with (out / "build.log").open("w", encoding="utf-8") as log:
        subprocess.run(
            command,
            cwd=source,
            env=env,
            stdout=log,
            stderr=subprocess.STDOUT,
            check=True,
        )
    verify_inventory(source, manifest["source_files"])
    package = out / "package" / "rsloop"
    shutil.copytree(
        source / "python" / "rsloop",
        package,
        ignore=shutil.ignore_patterns("__pycache__", "*.pyd", "*.so", "*.pdb"),
    )
    release = target / host / "release"
    library = (
        "rsloop.dll"
        if os.name == "nt"
        else "librsloop.dylib"
        if sys.platform == "darwin"
        else "librsloop.so"
    )
    extension = package / ("_loop" + sysconfig.get_config_var("EXT_SUFFIX"))
    shutil.copy2(release / library, extension)
    outputs = out / "compiler"
    outputs.mkdir()
    for suffix in ("ll", "s", "pdb"):
        matches = list((release / "deps").glob(f"rsloop*.{suffix}"))
        if suffix != "pdb" and len(matches) != 1:
            raise ValueError(
                f"Expected exactly one .{suffix} compiler output, found {matches}"
            )
        for path in matches:
            shutil.copy2(path, outputs / path.name)
    manifest.update(
        extension=extension.relative_to(out).as_posix(),
        package_files=inventory(out / "package"),
        compiler_files=inventory(outputs),
    )
    write_json(out / "manifest.json", manifest)
    print(f"Built artifact: {out}", flush=True)


def select_workloads(csv: str) -> list[str]:
    names = csv.split(",")
    if (
        len(set(names)) != len(names)
        or not names
        or any(name not in WORKLOADS for name in names)
    ):
        raise ValueError(f"Select distinct workloads from {','.join(WORKLOADS)}")
    return names


def workload_config(name: str, suite: str) -> dict:
    # Holdout changes both traffic shape and volume. It is never used by profile().
    holdout = suite == "holdout"
    return {
        "workload": name,
        "suite": suite,
        "callbacks": 3_000_000 if holdout else 2_000_000,
        "tasks": 350_000 if holdout else 300_000,
        "task_batch_size": 777 if holdout else 10_000,
        "tcp_roundtrips": 25_000 if holdout else 20_000,
        "payload_size": 8192 if holdout else 1024,
        "concurrency": 7 if holdout else 16,
        "requests_per_connection": 4000 if holdout else 2000,
        "http_response_size": 1024 if holdout else 4096,
        "mixed_payload_sizes": "128,2048,32768" if holdout else "64,1024,16384,65536",
        "bulk_bytes": 128 * 1024 * 1024 if holdout else 64 * 1024 * 1024,
        "bulk_chunk_size": 16384 if holdout else 65536,
    }


def percentile(values: list[float], fraction: float) -> float:
    ordered = sorted(values)
    position = (len(ordered) - 1) * fraction
    lower = int(position)
    upper = min(lower + 1, len(ordered) - 1)
    return ordered[lower] + (ordered[upper] - ordered[lower]) * (position - lower)


def import_artifact(directory: Path) -> dict:
    manifest = load_artifact(directory)
    if "rsloop" in sys.modules:
        raise ValueError("Artifact must be imported in a fresh process")
    sys.path.insert(0, str(directory / "package"))
    native = importlib.import_module("rsloop._loop")
    actual = Path(native.__file__).resolve()
    if actual != (directory / manifest["extension"]).resolve():
        raise ValueError(f"Wrong extension loaded: {actual}")
    return {"extension": str(actual), "sha256": sha256(actual), "pid": os.getpid()}


def child(args) -> None:
    identity = import_artifact(args.artifact.resolve())
    sys.path.insert(0, str(ROOT / "benches"))
    import compare_event_loops as small
    import workload_matrix as matrix

    config = json.loads(args.config)
    name = config["workload"]
    if name not in WORKLOADS:
        raise ValueError(f"Unknown workload: {name}")
    argv = ["matrix", "--loop", "rsloop", "--scenario", name]
    for key in (
        "concurrency",
        "requests_per_connection",
        "http_response_size",
        "mixed_payload_sizes",
        "bulk_bytes",
        "bulk_chunk_size",
    ):
        argv.extend(["--" + key.replace("_", "-"), str(config[key])])
    if name not in WORKLOADS[:3]:
        sys.argv = argv
        matrix_args = matrix.parse_args()
        matrix.validate_args(matrix_args)
    samples = []
    for _ in range(args.warmups + 1):
        gc.collect()
        gc.disable()
        try:
            started = time.process_time()
            if name == "callbacks":
                coro = small.bench_callbacks("rsloop", config["callbacks"])
            elif name == "tasks":
                coro = small.bench_tasks(
                    "rsloop", config["tasks"], config["task_batch_size"]
                )
            elif name == "tcp_streams":
                coro = small.bench_tcp_streams(
                    "rsloop", config["tcp_roundtrips"], config["payload_size"]
                )
            else:
                coro = matrix.SCENARIO_RUNNERS[name]("rsloop", matrix_args)
            result = asdict(small.run_with_loop("rsloop", coro))
            result["process_cpu_seconds"] = time.process_time() - started
        finally:
            gc.enable()
        latencies = result.pop("latency_ms", [])
        if latencies:
            result["latency_summary_ms"] = {
                "count": len(latencies),
                "p50": percentile(latencies, 0.5),
                "p95": percentile(latencies, 0.95),
                "p99": percentile(latencies, 0.99),
            }
        result["peak_rss_bytes"] = small.get_peak_rss_bytes()
        samples.append(result)
    print(
        json.dumps(
            {
                "identity": identity,
                "config": config,
                "warmups": samples[:-1],
                "result": samples[-1],
            },
            allow_nan=False,
        )
    )


def run_sample(
    artifact: Path,
    config: dict,
    warmups: int,
    timeout: float,
    profile_pattern: Path | None = None,
) -> dict:
    env = clean_environment()
    env["RSLOOP_USE_FAST_STREAMS"] = "1"
    if profile_pattern:
        env["LLVM_PROFILE_FILE"] = str(profile_pattern)
    command = [
        sys.executable,
        str(Path(__file__).resolve()),
        "_child",
        "--artifact",
        str(artifact),
        "--config",
        json.dumps(config),
        "--warmups",
        str(warmups),
    ]
    result = subprocess.run(
        command,
        cwd=ROOT,
        env=env,
        text=True,
        capture_output=True,
        timeout=timeout,
        check=False,
    )
    if result.returncode:
        raise RuntimeError(
            f"Benchmark failed ({result.returncode}):\n{result.stdout}\n{result.stderr}"
        )
    sample = json.loads(result.stdout)
    sample["stderr"] = result.stderr
    return sample


def balanced_orders(blocks: int, rng: random.Random) -> list[list[str]]:
    if blocks < 2 or blocks % 2:
        raise ValueError("Use an even number of at least two paired blocks")
    orders = [["baseline", "candidate"], ["candidate", "baseline"]] * (blocks // 2)
    rng.shuffle(orders)
    return orders


def paired_estimate(
    baseline: list[float],
    candidate: list[float],
    *,
    alpha: float = 0.05,
    seed: int = 0,
    resamples: int = 20000,
) -> dict:
    if len(baseline) != len(candidate) or len(baseline) < 2:
        raise ValueError("Statistics require matching process pairs")
    if any(not math.isfinite(v) or v <= 0 for v in baseline + candidate):
        raise ValueError("Timing samples must be positive and finite")
    deltas = [math.log(c / b) for b, c in zip(baseline, candidate)]
    rng = random.Random(seed)
    boot = [
        statistics.fmean(rng.choices(deltas, k=len(deltas))) for _ in range(resamples)
    ]
    change = lambda value: 100 * math.expm1(value)
    return {
        "pairs": len(deltas),
        "change_pct": change(statistics.fmean(deltas)),
        "ci_pct": [
            change(percentile(boot, alpha / 2)),
            change(percentile(boot, 1 - alpha / 2)),
        ],
        "confidence": 1 - alpha,
        "baseline_median_seconds": statistics.median(baseline),
        "candidate_median_seconds": statistics.median(candidate),
    }


def performance_decision(
    estimates: dict,
    *,
    primary: str,
    minimum_gain: float,
    regression_budget: float,
    reliable: bool,
) -> str:
    if not reliable:
        return "insufficient_measurement"
    if any(value["ci_pct"][0] > regression_budget for value in estimates.values()):
        return "reject_regression"
    if estimates[primary]["ci_pct"][1] < -minimum_gain and all(
        value["ci_pct"][1] <= regression_budget for value in estimates.values()
    ):
        return "performance_gate_passed"
    return "inconclusive"


def compare(args) -> None:
    baseline, candidate = args.baseline.resolve(), args.candidate.resolve()
    manifests = {
        "baseline": load_artifact(baseline),
        "candidate": load_artifact(candidate),
    }
    if any(m["mode"] == "generate" for m in manifests.values()):
        raise ValueError("Never use instrumented builds for performance comparison")
    for key in ("rustc", "python", "cargo", "platform"):
        if manifests["baseline"][key] != manifests["candidate"][key]:
            raise ValueError(f"Mismatched {key} across artifacts")

    def base_flags(manifest):
        return [
            "--remap-path-prefix=SOURCE=/rsloop"
            if flag.startswith("--remap-path-prefix=")
            else flag
            for flag in manifest["flags"]
            if not flag.startswith(
                ("-Cprofile-", "-Cllvm-args=-pgo-warn-missing-function")
            )
        ]

    if base_flags(manifests["baseline"]) != base_flags(manifests["candidate"]):
        raise ValueError("Mismatched non-PGO compiler flags")
    names = select_workloads(args.workloads)
    if args.primary not in names:
        raise ValueError("Primary workload must be included")
    out = args.out.resolve()
    out.mkdir(parents=True, exist_ok=False)
    rng = random.Random(args.seed)
    orders = {name: balanced_orders(args.blocks, rng) for name in names}
    plan = {
        "schema": SCHEMA,
        "baseline": str(baseline),
        "candidate": str(candidate),
        "artifact_manifests": manifests,
        "primary": args.primary,
        "metric": "seconds",
        "suite": args.suite,
        "seed": args.seed,
        "blocks": args.blocks,
        "warmups": args.warmups,
        "min_seconds": args.min_seconds,
        "minimum_gain_pct": args.minimum_gain,
        "regression_budget_pct": args.regression_budget,
        "orders": orders,
        "configurations": {name: workload_config(name, args.suite) for name in names},
        "runner_sha256": sha256(Path(__file__)),
        "benchmark_sha256": {
            name: sha256(ROOT / "benches" / name)
            for name in ("compare_event_loops.py", "workload_matrix.py")
        },
        "cpu_count": os.cpu_count(),
        "processor": platform.processor(),
        "affinity": sorted(os.sched_getaffinity(0))
        if hasattr(os, "sched_getaffinity")
        else None,
    }
    # Written before observing any timing: no post-hoc primary/threshold selection.
    write_json(out / "plan.json", plan)
    archive = out / "harness"
    archive.mkdir()
    shutil.copy2(Path(__file__), archive / "hotpath_lab.py")
    for filename in plan["benchmark_sha256"]:
        shutil.copy2(ROOT / "benches" / filename, archive / filename)
    pairs = {name: [] for name in names}
    with (out / "samples.jsonl").open("x", encoding="utf-8") as raw:
        for block in range(args.blocks):
            block_names = names.copy()
            rng.shuffle(block_names)
            for name in block_names:
                pair = {
                    "block": block,
                    "workload": name,
                    "order": orders[name][block],
                    "samples": {},
                }
                for label in pair["order"]:
                    sample = run_sample(
                        baseline if label == "baseline" else candidate,
                        plan["configurations"][name],
                        args.warmups,
                        args.timeout,
                    )
                    pair["samples"][label] = sample
                    raw.write(
                        json.dumps(
                            {"block": block, "workload": name, "label": label, **sample}
                        )
                        + "\n"
                    )
                    raw.flush()
                pairs[name].append(pair)
            print(f"Completed paired block {block + 1}/{args.blocks}", flush=True)
    if sha256(Path(__file__)) != plan["runner_sha256"] or any(
        sha256(ROOT / "benches" / name) != digest
        for name, digest in plan["benchmark_sha256"].items()
    ):
        raise ValueError("Harness changed during timing; results cannot be promoted")
    # Bonferroni-adjust intervals across the predeclared family of workload metrics.
    estimates = {}
    reliable = args.blocks >= 12
    for index, name in enumerate(names):
        values = {
            label: [p["samples"][label]["result"]["seconds"] for p in pairs[name]]
            for label in ("baseline", "candidate")
        }
        reliable &= min(values["baseline"] + values["candidate"]) >= args.min_seconds
        estimates[name] = paired_estimate(
            **values, alpha=0.05 / len(names), seed=args.seed + index
        )
        estimates[name]["min_observed_seconds"] = min(
            values["baseline"] + values["candidate"]
        )
    decision = performance_decision(
        estimates,
        primary=args.primary,
        minimum_gain=args.minimum_gain,
        regression_budget=args.regression_budget,
        reliable=reliable,
    )
    report = {
        "schema": SCHEMA,
        "decision": decision,
        "estimates": estimates,
        "correctness": "NOT certified by timing; run the test subcommand on both artifacts",
        "notes": [
            "Negative changes are faster; positive changes are slower.",
            "Percentile bootstrap resamples paired fresh processes, not request latencies.",
            "Intervals are approximate; host load, serial correlation, and repeated candidate searches can still bias inference.",
            "Per-process p95/p99, CPU time, and RSS are diagnostic only, not promotion metrics.",
        ],
    }
    write_json(out / "report.json", report)
    print(json.dumps(report, indent=2))


def profile(args) -> None:
    artifact = args.artifact.resolve()
    manifest = load_artifact(artifact, source=True)
    if manifest["mode"] != "generate":
        raise ValueError("Profile requires a build made with --instrument")
    out = args.out.resolve()
    out.mkdir(parents=True, exist_ok=False)
    tool = llvm_tool("llvm-profdata")
    names = select_workloads(args.workloads)
    profiles = []
    training = {
        "schema": SCHEMA,
        "artifact": str(artifact),
        "workloads": {},
        "kind": "instrumentation counts, NOT CPU time",
        "llvm_profdata": str(tool),
        "runner_sha256": sha256(Path(__file__)),
        "benchmark_sha256": {
            name: sha256(ROOT / "benches" / name)
            for name in ("compare_event_loops.py", "workload_matrix.py")
        },
    }
    for name in names:
        directory = out / name
        directory.mkdir()
        config = workload_config(name, "training")
        sample = run_sample(
            artifact, config, 0, args.timeout, directory / "%p-%m.profraw"
        )
        write_json(directory / "run.json", sample)
        raw = sorted(directory.glob("*.profraw"))
        if not raw:
            raise ValueError(f"No profile shards produced for {name}")
        merged = directory / "merged.profdata"
        subprocess.run(
            [str(tool), "merge", "-o", str(merged), *map(str, raw)], check=True
        )
        subprocess.run(
            [
                str(tool),
                "show",
                "--topn=40",
                "--counts",
                f"--output={directory / 'counts.txt'}",
                str(merged),
            ],
            check=True,
        )
        profiles.append(merged)
        training["workloads"][name] = {
            "config": config,
            "shards": {p.name: sha256(p) for p in raw},
        }
        print(f"Profiled {name}: {len(raw)} runtime-only shards", flush=True)
    merged = out / "merged.profdata"
    # Each workload has one training process. Execution counts are summed, not normalized.
    subprocess.run(
        [str(tool), "merge", "-o", str(merged), *map(str, profiles)], check=True
    )
    training["profile_sha256"] = sha256(merged)
    write_json(out / "training.json", training)


def extract_functions(path: Path, needle: str):
    """Analysis slices only: skip giant module-asm lines and unrelated functions."""
    with path.open(encoding="utf-8") as stream:
        body = None
        for line in stream:
            if line.startswith("define ") and needle in line:
                body = [line]
            elif body is not None:
                body.append(line)
                if line.rstrip() == "}":
                    yield "".join(body)
                    body = None


def analysis_metadata(path: Path, bodies: list[str]) -> list[str]:
    """Resolve profile weights and attributes without pulling in the debug graph."""
    text = "\n".join(bodies)
    profile_ids = set(re.findall(r"!(?:prof|PGOFuncName) !(\d+)", text))
    attribute_ids = set(re.findall(r"#(\d+)", text))
    selected = []
    with path.open(encoding="utf-8") as stream:
        for line in stream:
            if line.startswith("!"):
                match = re.match(r"!(\d+) =", line)
                if match and match[1] in profile_ids:
                    selected.append(line)
            elif line.startswith("attributes #"):
                match = re.match(r"attributes #(\d+) =", line)
                if match and match[1] in attribute_ids:
                    selected.append(line)
    return selected


def packet(args) -> None:
    artifact = args.artifact.resolve()
    manifest = load_artifact(artifact, source=True)
    verify_inventory(artifact / "compiler", manifest["compiler_files"])
    ir = next((artifact / "compiler").glob("*.ll"))
    # Structural verification is useful but is NOT an equivalence/safety proof.
    subprocess.run(
        [str(llvm_tool("opt")), "-passes=verify", "-disable-output", str(ir)],
        check=True,
    )
    bodies = list(extract_functions(ir, args.function))
    if not bodies:
        raise ValueError(
            "No IR definitions matched; the function may have been inlined"
        )
    out = args.out.resolve()
    out.mkdir(parents=True, exist_ok=False)
    (out / "selected.ll").write_text(
        "; ANALYSIS ONLY: dependencies/metadata omitted; not a compilable module.\n"
        + "\n".join(bodies)
        + "\n; Selected profile metadata and function/call attributes follow.\n"
        + "".join(analysis_metadata(ir, bodies)),
        encoding="utf-8",
    )
    source = Path(manifest["source"])
    source_file = (source / args.source_file).resolve()
    if (
        not source_file.is_relative_to(source)
        or source_file.relative_to(source).as_posix() not in manifest["source_files"]
    ):
        raise ValueError("Source file must belong to the preserved snapshot")
    shutil.copy2(source_file, out / "source.rs")
    if args.profile:
        training = read_json(args.profile.resolve() / "training.json")
        origin = load_artifact(Path(training["artifact"]), source=True)
        if (
            origin["source_files"] != manifest["source_files"]
            or origin["rustc"] != manifest["rustc"]
        ):
            raise ValueError(
                "Profile does not match the source/toolchain in this packet"
            )
        for name in training["workloads"]:
            subprocess.run(
                [
                    str(llvm_tool("llvm-profdata")),
                    "show",
                    f"--function={args.function}",
                    "--counts",
                    f"--output={out / (name + '-counts.txt')}",
                    str(args.profile.resolve() / name / "merged.profdata"),
                ],
                check=True,
            )
    write_json(
        out / "provenance.json",
        {
            "manifest": manifest,
            "function_filter": args.function,
            "definitions": len(bodies),
            "source_file": args.source_file,
        },
    )
    (out / "PROMPT.md").write_text(
        "# Rust hot-path review\n\n"
        "Review source.rs alongside selected.ll. IR is a non-standalone optimized analysis slice.\n"
        "Execution counts are not time samples; do not rank CPU hotspots by counts alone.\n"
        "Propose ONE minimal Rust patch, a falsifiable performance hypothesis, and edge-case tests.\n"
        "Preserve Python exceptions, refcounts, cancellation, ordering, aliasing, atomics, and FFI contracts.\n"
        "Do not remove safety checks or assume initialized memory without documenting the external guarantee.\n"
        "Explain why LLVM cannot already perform the transformation. Inspect emitted assembly too.\n"
        "Declare the primary workload and regression budget BEFORE comparing. Use fresh paired processes.\n"
        "Require Rust and Python tests on both exact artifacts; evaluate a separately shaped holdout suite.\n"
        "Report rejected/inconclusive candidates. Never claim a speedup from counts or unpaired timings.\n"
        "Do not apply raw IR edits automatically; no equivalence proof is provided by this harness.\n",
        encoding="utf-8",
    )
    print(f"Wrote {len(bodies)} function definitions to {out}")


def test_artifact(args) -> None:
    artifact = args.artifact.resolve()
    if args.python_child:
        os.environ["PYTHONPATH"] = str(artifact / "package")
        identity = import_artifact(artifact)
        print(f"Testing exact extension: {identity}", flush=True)
        import pytest

        raise SystemExit(pytest.main(["-q", str(ROOT / "tests")]))
    manifest = load_artifact(artifact, source=True)
    env = clean_environment()
    env["PYTHONHOME"] = sys.base_prefix
    if os.name == "nt":
        env["PATH"] = os.pathsep.join((sys.base_prefix, env.get("PATH", "")))
    source = Path(manifest["source"])
    log_path = args.log.resolve()
    # The TLS fixtures are generated/ignored, so git archive does not include
    # them. Add test-only data in a separate source copy, never the artifact.
    test_source = log_path.with_suffix(".source")
    shutil.copytree(source, test_source)
    fixtures = ROOT / "tests" / "fixtures" / "tls"
    if not fixtures.is_dir():
        raise ValueError(
            "Generate test TLS fixtures with scripts/generate_test_tls_certs.py first"
        )
    shutil.copytree(
        fixtures, test_source / "tests" / "fixtures" / "tls", dirs_exist_ok=True
    )
    # Force Cargo to distinguish copied snapshots: otherwise its shared cache
    # can retain a test executable with the previous CARGO_MANIFEST_DIR embedded.
    env["CARGO_ENCODED_RUSTFLAGS"] = f"--remap-path-prefix={test_source}=/rsloop-tests"
    with log_path.open("x", encoding="utf-8") as log:
        subprocess.run(
            [
                "cargo",
                "test",
                "--lib",
                "--locked",
                "--manifest-path",
                str(test_source / "Cargo.toml"),
                "--target-dir",
                str(ROOT / "target" / "hotpath-test-cache"),
            ],
            cwd=test_source,
            env=env,
            stdout=log,
            stderr=subprocess.STDOUT,
            check=True,
        )
        env.pop("PYTHONHOME", None)
        # Tests that spawn Python must inherit the selected package as well.
        env["PYTHONPATH"] = str(artifact / "package")
        subprocess.run(
            [
                sys.executable,
                str(Path(__file__).resolve()),
                "test",
                "--artifact",
                str(artifact),
                "--python-child",
            ],
            cwd=ROOT,
            env=env,
            stdout=log,
            stderr=subprocess.STDOUT,
            check=True,
        )
    write_json(
        log_path.with_suffix(".json"),
        {
            "artifact": str(artifact),
            "package_files": manifest["package_files"],
            "log_sha256": sha256(log_path),
            "passed": True,
            "rust_tests": "debug source build",
            "test_fixture_files": inventory(fixtures),
            "python_test_files": inventory(ROOT / "tests"),
            "python_tests": "exact release extension",
        },
    )
    print(f"Rust/Python tests passed; {log_path}")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="command", required=True)
    p = sub.add_parser(
        "build",
        help="Snapshot committed source and preserve an isolated release artifact",
    )
    source = p.add_mutually_exclusive_group()
    source.add_argument("--revision")
    source.add_argument("--source-artifact", type=Path)
    mode = p.add_mutually_exclusive_group()
    mode.add_argument("--instrument", action="store_true")
    mode.add_argument("--pgo", type=Path, help="Training output directory")
    p.add_argument("--out", type=Path, required=True)
    p.set_defaults(func=build)
    p = sub.add_parser(
        "compare", help="Balanced randomized A/B process blocks; negative means faster"
    )
    p.add_argument("--baseline", type=Path, required=True)
    p.add_argument("--candidate", type=Path, required=True)
    p.add_argument("--workloads", default=DEFAULT_WORKLOADS)
    p.add_argument("--primary", required=True)
    p.add_argument("--suite", choices=("training", "holdout"), default="holdout")
    p.add_argument("--blocks", type=int, default=12)
    p.add_argument("--seed", type=int, default=1729)
    p.add_argument("--warmups", type=int, default=1)
    p.add_argument("--min-seconds", type=float, default=0.25)
    p.add_argument("--minimum-gain", type=float, default=1.0)
    p.add_argument("--regression-budget", type=float, default=3.0)
    p.add_argument("--timeout", type=float, default=180.0)
    p.add_argument("--out", type=Path, required=True)
    p.set_defaults(func=compare)
    p = sub.add_parser(
        "profile", help="Collect runtime-only, per-workload instrumentation counts"
    )
    p.add_argument("--artifact", type=Path, required=True)
    p.add_argument("--workloads", default=DEFAULT_WORKLOADS)
    p.add_argument("--out", type=Path, required=True)
    p.add_argument("--timeout", type=float, default=180.0)
    p.set_defaults(func=profile)
    p = sub.add_parser(
        "packet", help="Prepare a bounded, provenance-linked LLM review packet"
    )
    p.add_argument("--artifact", type=Path, required=True)
    p.add_argument(
        "--function", required=True, help="Substring in the IR symbol (not a regex)"
    )
    p.add_argument("--source-file", required=True)
    p.add_argument("--profile", type=Path)
    p.add_argument("--out", type=Path, required=True)
    p.set_defaults(func=packet)
    p = sub.add_parser(
        "test", help="Run Rust source tests and Python tests on the exact extension"
    )
    p.add_argument("--artifact", type=Path, required=True)
    p.add_argument("--log", type=Path)
    p.add_argument("--python-child", action="store_true", help=argparse.SUPPRESS)
    p.set_defaults(func=test_artifact)
    p = sub.add_parser("_child", help=argparse.SUPPRESS)
    p.add_argument("--artifact", type=Path, required=True)
    p.add_argument("--config", required=True)
    p.add_argument("--warmups", type=int, default=1)
    p.set_defaults(func=child)
    args = parser.parse_args()
    if getattr(args, "warmups", 0) < 0:
        parser.error("warmups must be nonnegative")
    for key in ("timeout", "min_seconds", "minimum_gain", "regression_budget"):
        value = getattr(args, key, 1.0)
        if not math.isfinite(value) or value <= 0:
            parser.error(f"{key} must be positive and finite")
    if args.command == "test" and not args.python_child and not args.log:
        parser.error("test requires --log")
    args.func(args)


if __name__ == "__main__":
    main()
