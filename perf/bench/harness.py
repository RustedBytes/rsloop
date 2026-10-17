#!/usr/bin/env python3
"""rsloop-perf harness: full-service throughput of one API image per event-loop arm.

Each API under test is a Litestar web service served by Granian (2 workers), backed by Postgres
(SQLAlchemy + asyncpg), Redis and NATS. The stack runs in an isolated docker compose project (no host ports) whose
containers are named <RSPERF_PROJECT>_* ("rsperf" by default).

Arms: uvloop, rsloop (the image-pinned release), fork (same image, PYTHONPATH=<fork dir> first),
asyncio. FORK_DIR picks the fork build: a host site dir under $RSPERF_ROOT/sites (mounted at /fork)
or "image" for the wheel baked into the API's :fork image at /opt/rsloop-fork.
Same images and commit for every arm; only EVENT_LOOP differs. The image provides a bench_api_server
module that serves the API with the loop BENCH_EVENT_LOOP names.
Arms interleave per round, rotated each round. A run never starts below 8 GiB
MemAvailable (waits up to 15 min, then the run is recorded as aborted).

usage: harness.py api <api> [rounds] | soak <api> <arms> <seconds> <path>
Results: one JSON object per line in $RSPERF_ROOT/run/bench/<target>.jsonl.

Environment (paths and names have neutral defaults; each API's workload is required, so no
application routes or accounts live in this file):
  RSPERF_ROOT              benchmark workspace (default: the current directory): override.yml, sites/,
                           bench/ (mounted at /bench), run/bench/
  RSPERF_COMPOSE_FILE      the application's compose file (default $RSPERF_ROOT/snap/docker-compose.yml)
  RSPERF_COMPOSE_PROFILE   compose profile to enable (default: none)
  RSPERF_PROJECT           compose project and container-name prefix (default rsperf)
  RSPERF_NETWORK           network the load generator joins (default <project>_default)
  RSPERF_DB_CONTAINER, RSPERF_PG_USER, RSPERF_PG_DB
                           where the new-session counter is read (default <project>_db, postgres,
                           postgres)
  RSPERF_<API>_SERVICE     the API's compose service (default: the API's name)
  RSPERF_<API>_ENDPOINTS   comma-separated request paths to measure, e.g. RSPERF_API_ENDPOINTS;
                           the first is also the readiness probe
  RSPERF_<API>_LOGIN_PATH, RSPERF_<API>_LOGIN_BODY
                           a seeded test account's login: the JSON body is POSTed to the path, and
                           the session cookies it sets go with every measured request

Quiet-machine gate (bench/quiet.py; QUIET_GATE=0 disables): load1 counts the benchmark's own ~2.3-3
busy cores, so it cannot tell external contention apart. Every row first waits until CPU used
outside the <project>_* containers stays at or below QUIET_PRE_CORES (2.0) over a QUIET_PROBE_S
window, then records that foreign CPU over its own measured window. A row above QUIET_ROW_CORES
(2.5) is written with "contaminated": true and measured again (after a fresh warm-up), up to
QUIET_ATTEMPTS times. Together with the benchmark that keeps total demand within the host's 6
physical P-cores; one heavy cargo build (4+ cores) trips it.
oha runs by docker exec in one persistent load-generator container (same image, --cpus 2), so its
CPU stays attributable to the benchmark.
"""

import contextlib
import http.client
import http.cookiejar
import json
import os
import subprocess
import sys
import time
import urllib.request

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import quiet

ROOT = os.path.abspath(os.environ.get("RSPERF_ROOT", "."))
BENCH = f"{ROOT}/bench"
OUT = f"{ROOT}/run/bench"
PROJECT = os.environ.get("RSPERF_PROJECT", "rsperf")
NET = os.environ.get("RSPERF_NETWORK", f"{PROJECT}_default")
DB_CONTAINER = os.environ.get("RSPERF_DB_CONTAINER", f"{PROJECT}_db")
PG_USER = os.environ.get("RSPERF_PG_USER", "postgres")
PG_DB = os.environ.get("RSPERF_PG_DB", "postgres")
OHA = "ghcr.io/hatoo/oha:latest"
PROFILE = os.environ.get("RSPERF_COMPOSE_PROFILE")
COMPOSE = [
    "docker",
    "compose",
    "-p",
    PROJECT,
    "-f",
    os.environ.get("RSPERF_COMPOSE_FILE", f"{ROOT}/snap/docker-compose.yml"),
    "-f",
    f"{ROOT}/override.yml",
    *(["--profile", PROFILE] if PROFILE else []),
]
MIN_AVAIL_KB = 8 * 1024 * 1024
MEMORY_WAIT_S = 15 * 60
OHA_CPUS = "2"
WARMUP_S = int(os.environ.get("BENCH_WARMUP_S", "15"))
MEASURE_S = int(os.environ.get("BENCH_MEASURE_S", "60"))
IDLE_S = int(os.environ.get("BENCH_IDLE_S", "10"))
CONCURRENCY = (16, 64)
ROUND_BASE = int(os.environ.get("BENCH_ROUND_BASE", "0"))

API_ARMS = os.environ.get("BENCH_ARMS", "uvloop,rsloop,fork,asyncio").split(",")
FORK_DIR = os.environ.get("FORK_DIR", "image")
MEASURE_CONCURRENCY = tuple(
    int(c) for c in os.environ.get("BENCH_CONCURRENCY", "16,64").split(",")
)
ENDPOINTS = os.environ.get("BENCH_ENDPOINTS")
QUIET_GATE = os.environ.get("QUIET_GATE", "1") != "0"
QUIET_PRE_CORES = float(os.environ.get("QUIET_PRE_CORES", "2.0"))
QUIET_ROW_CORES = float(os.environ.get("QUIET_ROW_CORES", "2.5"))
QUIET_PROBE_S = float(os.environ.get("QUIET_PROBE_S", "10"))
QUIET_WAIT_S = int(os.environ.get("QUIET_WAIT_S", str(3 * 3600)))
QUIET_ATTEMPTS = int(os.environ.get("QUIET_ATTEMPTS", "4"))
LOADGEN = f"{PROJECT}_loadgen_{os.getpid()}"


class Aborted(Exception):
    pass


def run(cmd, check=True, **kw):
    return subprocess.run(cmd, check=check, capture_output=True, text=True, **kw)


def mem_avail_kb():
    with open("/proc/meminfo") as f:
        for line in f:
            if line.startswith("MemAvailable"):
                return int(line.split()[1])
    return 0


def wait_for_memory():
    deadline = time.monotonic() + MEMORY_WAIT_S
    while mem_avail_kb() < MIN_AVAIL_KB:
        if time.monotonic() > deadline:
            raise Aborted(
                f"MemAvailable {mem_avail_kb() // 1024} MiB < 8 GiB for {MEMORY_WAIT_S} s"
            )
        time.sleep(15)


def host_state():
    return {"load1": os.getloadavg()[0], "mem_avail_mib": mem_avail_kb() // 1024}


def container_ip(name):
    return run(
        [
            "docker",
            "inspect",
            "-f",
            "{{range .NetworkSettings.Networks}}{{.IPAddress}}{{end}}",
            name,
        ]
    ).stdout.strip()


def cgroup_dir(name):
    cid = run(["docker", "inspect", "-f", "{{.Id}}", name]).stdout.strip()
    return f"/sys/fs/cgroup/docker.slice/docker-cap.slice/docker-{cid}.scope"


def cpu_usec(cg):
    with open(f"{cg}/cpu.stat") as f:
        for line in f:
            if line.startswith("usage_usec"):
                return int(line.split()[1])
    raise RuntimeError("no usage_usec")


def mem_current_mib(cg):
    with open(f"{cg}/memory.current") as f:
        return int(f.read()) / 2**20


def worker_pids(name):
    out = run(["docker", "top", name, "-eo", "pid,args"]).stdout.splitlines()[1:]
    return [int(line.split()[0]) for line in out if "spawn_main" in line]


def proc_rss(pid):
    fields = {}
    with open(f"/proc/{pid}/status") as f:
        for line in f:
            key, _, value = line.partition(":")
            if key in ("VmRSS", "VmHWM"):
                fields[key] = int(value.split()[0]) / 1024
    return fields


def http_get(url, headers=None, timeout=5):
    request = urllib.request.Request(url, headers=headers or {})
    with urllib.request.urlopen(request, timeout=timeout) as response:
        return response.status, response.read()


def wait_http(url, timeout=120):
    deadline = time.monotonic() + timeout
    while time.monotonic() < deadline:
        with contextlib.suppress(OSError, http.client.HTTPException):
            status, _ = http_get(url)
            if status == 200:
                return
        time.sleep(0.25)
    raise RuntimeError(f"{url} not ready")


def workload(api):
    """The API's service, endpoints and test-account login, from RSPERF_<API>_* variables."""
    prefix = f"RSPERF_{api.upper()}_"
    try:
        endpoints = [p for p in os.environ[prefix + "ENDPOINTS"].split(",") if p]
        login_path = os.environ[prefix + "LOGIN_PATH"]
        login_body = os.environ[prefix + "LOGIN_BODY"]
    except KeyError as missing:
        raise SystemExit(f"set {missing.args[0]} (the workload of {api})") from None
    if not endpoints:
        raise SystemExit(f"{prefix}ENDPOINTS names no path")
    return {
        "service": os.environ.get(prefix + "SERVICE", api),
        "endpoints": endpoints,
        "login_path": login_path,
        "login_body": login_body,
    }


def login(base, spec):
    jar = http.cookiejar.CookieJar()
    opener = urllib.request.build_opener(urllib.request.HTTPCookieProcessor(jar))
    request = urllib.request.Request(
        f"{base}{spec['login_path']}",
        data=spec["login_body"].encode(),
        headers={"content-type": "application/json"},
    )
    # A non-2xx answer raises HTTPError.
    with opener.open(request, timeout=10):
        pass
    if not jar:
        raise RuntimeError("login set no session cookie")
    return "; ".join(f"{c.name}={c.value}" for c in jar)


def start_loadgen():
    run(["docker", "rm", "-f", LOADGEN], check=False)
    run(
        [
            "docker",
            "run",
            "-d",
            "--name",
            LOADGEN,
            "--network",
            NET,
            "--cpus",
            OHA_CPUS,
            "--entrypoint",
            "sleep",
            OHA,
            "infinity",
        ]
    )


def stop_loadgen():
    run(["docker", "rm", "-f", LOADGEN], check=False)


def oha(url, concurrency, seconds, cookie):
    cmd = [
        "docker",
        "exec",
        LOADGEN,
        "/bin/oha",
        "-z",
        f"{seconds}s",
        "-c",
        str(concurrency),
        "--no-tui",
        "--output-format",
        "json",
        "-H",
        f"Cookie: {cookie}",
        url,
    ]
    return json.loads(run(cmd).stdout)


def wait_quiet():
    """Blocks until foreign CPU stays under QUIET_PRE_CORES for one probe window; returns that window."""
    if not QUIET_GATE:
        return None
    deadline = time.monotonic() + QUIET_WAIT_S
    waited = 0
    while True:
        window = quiet.sample(QUIET_PROBE_S)
        if window["foreign_cores"] <= QUIET_PRE_CORES:
            window["waited_s"] = waited
            return window
        if time.monotonic() > deadline:
            raise Aborted(
                f"foreign CPU above {QUIET_PRE_CORES} cores for {QUIET_WAIT_S} s: {window}"
            )
        waited += QUIET_PROBE_S
        time.sleep(5)
        waited += 5


def fork_build(name):
    """sha256 of the native module the fork arm imports (provenance per start)."""
    out = run(
        [
            "docker",
            "exec",
            name,
            "sh",
            "-c",
            "sha256sum /opt/rsloop-fork/rsloop/_loop*.so /fork/rsloop/_loop*.so 2>/dev/null",
        ],
        check=False,
    ).stdout.split()
    return out[0] if out else None


PROVENANCE = r"""
import hashlib, importlib, json, os, sys
out = {"event_loop": os.environ.get("BENCH_EVENT_LOOP"), "python": sys.version.split()[0]}
for name, native in (("rsloop", "_loop"), ("uvloop", "loop")):
    try:
        module = importlib.import_module(name)
    except ImportError as error:
        out[name] = {"error": str(error)}
        continue
    folder = os.path.dirname(module.__file__)
    so = sorted(f for f in os.listdir(folder) if f.startswith(native + ".") and f.endswith(".so"))
    out[name] = {
        "file": module.__file__,
        "version": getattr(module, "__version__", None),
        "native": so[0] if so else None,
        "sha256": hashlib.sha256(open(os.path.join(folder, so[0]), "rb").read()).hexdigest() if so else None,
    }
print(json.dumps(out))
"""


def loop_provenance(name):
    """What the arm's interpreter imports: rsloop and uvloop module paths, versions and native-module
    sha256, read with the container's own environment (PYTHONPATH included)."""
    out = run(["docker", "exec", name, "python", "-c", PROVENANCE], check=False)
    try:
        return json.loads(out.stdout.strip().splitlines()[-1])
    except (IndexError, ValueError):
        return {"error": (out.stdout + out.stderr)[-400:]}


def startup_lines(name):
    logs = run(["docker", "logs", name], check=False)
    return [
        line
        for line in (logs.stdout + logs.stderr).splitlines()
        if "event loop: " in line
    ]


def stop(name):
    started = time.monotonic()
    run(["docker", "stop", "-t", "15", name], check=False)
    took = time.monotonic() - started
    code = run(
        ["docker", "inspect", "-f", "{{.State.ExitCode}}", name], check=False
    ).stdout.strip()
    logs = run(["docker", "logs", name], check=False)
    run(["docker", "rm", "-f", name], check=False)
    lines = (logs.stdout + logs.stderr).splitlines()
    return {
        "stop_s": round(took, 2),
        "exit_code": int(code) if code.lstrip("-").isdigit() else code,
        "log_tail": lines[-12:],
        # Opt-in sitecustomize trace (RSPERF_TRACE_POOL).
        "traces": [line for line in lines if line.startswith("rsperf-")],
    }


VARIANTS = json.loads(
    os.environ.get(
        "BENCH_VARIANTS",
        '{"nospin": {"RSLOOP_WAKE_SPIN_US": "0", "RSLOOP_READER_SPIN_US": "0"}}',
    )
)


def arm_args(arm):
    """Same image and code for every arm; the fork arm only puts the fork build first on sys.path.

    "<arm>@<variant>" adds the variant's environment (BENCH_VARIANTS), e.g. fork@nospin.
    "fork:<site>" runs a specific site dir instead of FORK_DIR."""
    arm, _, variant = arm.partition("@")
    env = []
    for key, value in (VARIANTS[variant] if variant else {}).items():
        env += ["-e", f"{key}={value}"]
    arm, _, site = arm.partition(":")
    fork_dir = site or FORK_DIR
    loop = "rsloop" if arm == "fork" else arm
    path = "/app:/bench"
    extra = []
    if arm == "fork":
        if fork_dir == "image":
            path = "/opt/rsloop-fork:" + path
        else:
            path = "/fork:" + path
            extra = ["-v", f"{ROOT}/sites/{fork_dir}:/fork:ro"]
    return [
        "-e",
        f"BENCH_EVENT_LOOP={loop}",
        "-e",
        f"PYTHONPATH={path}",
        "-e",
        f"BENCH_ARM={arm}",
        "-v",
        f"{BENCH}:/bench:ro",
        *env,
        *extra,
    ]


def pg_sessions():
    """Postgres sessions opened so far in the service database (connection churn counter)."""
    out = run(
        [
            "docker",
            "exec",
            DB_CONTAINER,
            "psql",
            "-U",
            PG_USER,
            "-d",
            PG_DB,
            "-Atc",
            "select sessions from pg_stat_database where datname = current_database()",
        ],
        check=False,
    )
    try:
        return int(out.stdout.strip())
    except ValueError:
        return None


def rotate(arms, round_index):
    shift = round_index % len(arms)
    return arms[shift:] + arms[:shift]


def write(target, record):
    with open(f"{OUT}/{target}{os.environ.get('BENCH_SUFFIX', '')}.jsonl", "a") as f:
        f.write(json.dumps(record) + "\n")
    print(
        json.dumps(
            {k: v for k, v in record.items() if k not in ("oha_details", "log_tail")}
        ),
        flush=True,
    )


def measure_row(api, round_index, arm, name, cg, base, path, concurrency, cookie):
    wait_for_memory()
    state = host_state()
    window = quiet.Window() if QUIET_GATE else None
    before = cpu_usec(cg)
    sessions_before = pg_sessions()
    result = oha(f"{base}{path}", concurrency, MEASURE_S, cookie)
    cpu_ms = (cpu_usec(cg) - before) / 1000
    sessions_after = pg_sessions()
    quiet_row = window.finish() if window else None
    codes = result.get("statusCodeDistribution") or {}
    total = sum(codes.values())
    lat = result["latencyPercentiles"]
    workers = {pid: proc_rss(pid) for pid in worker_pids(name)}
    return {
        "kind": "measure",
        "api": api,
        "round": round_index + 1,
        "arm": arm,
        "path": path,
        "fork_dir": FORK_DIR,
        "concurrency": concurrency,
        "rps": result["summary"]["requestsPerSec"],
        "requests": total,
        "non_2xx": sum(v for k, v in codes.items() if not k.startswith("2")),
        "errors": result.get("errorDistribution") or {},
        "p50_ms": lat["p50"] * 1e3,
        "p95_ms": lat["p95"] * 1e3,
        "p99_ms": lat["p99"] * 1e3,
        "cpu_ms": cpu_ms,
        "cpu_ms_per_1k": cpu_ms / total * 1000 if total else None,
        "pg_sessions_per_1k": (sessions_after - sessions_before) / total * 1000
        if total and sessions_before is not None and sessions_after is not None
        else None,
        "cgroup_mib": round(mem_current_mib(cg), 1),
        "worker_rss_mib": [round(w["VmRSS"], 1) for w in workers.values()],
        "worker_peak_rss_mib": [round(w["VmHWM"], 1) for w in workers.values()],
        **state,
        "load1_end": os.getloadavg()[0],
        "quiet": quiet_row,
        "contaminated": bool(
            quiet_row and quiet_row["foreign_cores"] > QUIET_ROW_CORES
        ),
    }


def bench_api(api, rounds):
    start_loadgen()
    try:
        bench_api_rounds(api, rounds)
    finally:
        stop_loadgen()


def bench_api_rounds(api, rounds):
    spec = workload(api)
    for round_index in range(ROUND_BASE, ROUND_BASE + rounds):
        for arm in rotate(API_ARMS, round_index):
            name = f"{PROJECT}_bench_{api}"
            try:
                wait_for_memory()
            except Aborted as error:
                write(
                    api,
                    {
                        "kind": "aborted",
                        "api": api,
                        "round": round_index + 1,
                        "arm": arm,
                        "reason": str(error),
                    },
                )
                continue
            run(
                COMPOSE
                + [
                    "run",
                    "-d",
                    "--no-deps",
                    "--name",
                    name,
                    *arm_args(arm),
                    "-e",
                    f"BENCH_WORKERS={os.environ.get('BENCH_WORKERS', '2')}",
                    "--entrypoint",
                    "python",
                    spec["service"],
                    "-m",
                    "bench_api_server",
                ]
            )
            outcome = {}
            try:
                ip = container_ip(name)
                base = f"http://{ip}:8000"
                wait_http(f"{base}{spec['endpoints'][0]}")
                cg = cgroup_dir(name)
                time.sleep(3)
                idle_start = cpu_usec(cg)
                time.sleep(IDLE_S)
                idle_ms_per_s = (cpu_usec(cg) - idle_start) / 1000 / IDLE_S
                cookie = login(base, spec)
                for path in spec["endpoints"]:
                    status, _ = http_get(f"{base}{path}", {"Cookie": cookie})
                    if status != 200:
                        raise RuntimeError(f"{path} answered {status}")
                lines = startup_lines(name)
                write(
                    api,
                    {
                        "kind": "start",
                        "api": api,
                        "round": round_index + 1,
                        "arm": arm,
                        "fork_dir": FORK_DIR,
                        "startup_lines": lines,
                        "idle_cpu_ms_per_s": round(idle_ms_per_s, 2),
                        "fork_build_sha256": fork_build(name)
                        if arm.startswith("fork")
                        else None,
                        "loop_provenance": loop_provenance(name),
                        "loadgen": LOADGEN,
                        **host_state(),
                    },
                )
                for path in ENDPOINTS.split(",") if ENDPOINTS else spec["endpoints"]:
                    warm = False
                    for concurrency in MEASURE_CONCURRENCY:
                        for attempt in range(1, QUIET_ATTEMPTS + 1):
                            gate = wait_quiet()
                            if (
                                not warm
                                or attempt > 1
                                or (gate and gate["waited_s"] > 0)
                            ):
                                # Fresh warm-up after any wait, so every measured row starts warm.
                                oha(
                                    f"{base}{path}",
                                    MEASURE_CONCURRENCY[0],
                                    WARMUP_S,
                                    cookie,
                                )
                                warm = True
                            record = measure_row(
                                api,
                                round_index,
                                arm,
                                name,
                                cg,
                                base,
                                path,
                                concurrency,
                                cookie,
                            )
                            record.update(attempt=attempt, pre_gate=gate)
                            write(api, record)
                            if not record["contaminated"]:
                                break
            except Aborted as error:
                outcome["aborted"] = str(error)
            except Exception as error:  # noqa: BLE001 - any failure ends this arm, recorded in its stop row
                outcome["error"] = f"{type(error).__name__}: {error}"
            finally:
                write(
                    api,
                    {
                        "kind": "stop",
                        "api": api,
                        "round": round_index + 1,
                        "arm": arm,
                        **outcome,
                        **stop(name),
                    },
                )


def soak(api, arms, seconds, path, concurrency=16):
    """Sustained load on one endpoint; per-worker RSS every 30 s (leak check)."""
    spec = workload(api)
    for arm in arms:
        name = f"{PROJECT}_bench_{api}"
        wait_for_memory()
        run(
            COMPOSE
            + [
                "run",
                "-d",
                "--no-deps",
                "--name",
                name,
                *arm_args(arm),
                "-e",
                f"BENCH_WORKERS={os.environ.get('BENCH_WORKERS', '2')}",
                "--entrypoint",
                "python",
                spec["service"],
                "-m",
                "bench_api_server",
            ]
        )
        series, outcome = [], {}
        try:
            ip = container_ip(name)
            base = f"http://{ip}:8000"
            wait_http(f"{base}{spec['endpoints'][0]}")
            cookie = login(base, spec)
            pids = worker_pids(name)
            started = time.monotonic()
            load = subprocess.Popen(
                [
                    "docker",
                    "run",
                    "--rm",
                    "--name",
                    f"{PROJECT}_oha_soak_{os.getpid()}",
                    "--network",
                    NET,
                    "--cpus",
                    OHA_CPUS,
                    OHA,
                    "-z",
                    f"{seconds}s",
                    "-c",
                    str(concurrency),
                    "--no-tui",
                    "--output-format",
                    "json",
                    "-H",
                    f"Cookie: {cookie}",
                    f"{base}{path}",
                ],
                stdout=subprocess.PIPE,
                text=True,
            )
            while load.poll() is None:
                series.append(
                    {
                        "t_s": round(time.monotonic() - started),
                        "rss_mib": [round(proc_rss(p)["VmRSS"], 1) for p in pids],
                    }
                )
                time.sleep(30)
            assert load.stdout is not None
            result = json.loads(load.stdout.read())
            series.append(
                {
                    "t_s": round(time.monotonic() - started),
                    "rss_mib": [round(proc_rss(p)["VmRSS"], 1) for p in pids],
                }
            )
            codes = result.get("statusCodeDistribution") or {}
            write(
                f"soak_{api}",
                {
                    "kind": "soak",
                    "api": api,
                    "arm": arm,
                    "path": path,
                    "concurrency": concurrency,
                    "seconds": seconds,
                    "requests": sum(codes.values()),
                    "rps": result["summary"]["requestsPerSec"],
                    "series": series,
                    **host_state(),
                },
            )
        except Exception as error:  # noqa: BLE001 - recorded in the soak stop row
            outcome["error"] = f"{type(error).__name__}: {error}"
        finally:
            write(
                f"soak_{api}",
                {"kind": "stop", "api": api, "arm": arm, **outcome, **stop(name)},
            )


def main():
    if sys.argv[1] in ("api", "soak"):
        workload(sys.argv[2])  # fail before any container starts
    os.makedirs(OUT, exist_ok=True)
    if sys.argv[1] == "api":
        bench_api(sys.argv[2], int(sys.argv[3]) if len(sys.argv) > 3 else 3)
    elif sys.argv[1] == "soak":
        soak(sys.argv[2], sys.argv[3].split(","), int(sys.argv[4]), sys.argv[5])
    else:
        raise SystemExit(__doc__)


if __name__ == "__main__":
    main()
