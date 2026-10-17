#!/usr/bin/env python3
"""rsloop-perf quiet-machine gate: CPU used outside the benchmark's own containers.

The host is an i7-13650HX: 20 logical CPUs, of which 0-11 are 6 hyper-threaded P-cores and 12-19
are E-cores. The benchmark itself keeps about 2.3-3 cores busy (server capped at --cpus 2, load
generator --cpus 2, Postgres/Redis/NATS), and load1 counts all of it, so load1 cannot tell
external contention apart. "Foreign" CPU is host busy time (/proc/stat: user, nice, system, irq,
softirq, steal) minus the cgroup CPU of every running <RSPERF_PROJECT>_* container (rsperf_* by
default: the API under test, its Postgres/Redis/NATS, and the persistent load generator). It includes the desktop session,
dockerd, containerd shims and unattributed softirq: an idle host reads about 0.8-1.5 cores.

usage: quiet.py [seconds=10]  -> one JSON line (foreign_cores, busy_cores, own_cores, load1, top cgroups)
"""

import json
import os
import subprocess
import sys
import time

TICK = os.sysconf("SC_CLK_TCK")
CG = "/sys/fs/cgroup"


def host_busy_s():
    with open("/proc/stat") as f:
        fields = [int(x) for x in f.readline().split()[1:]]
    user, nice, system, _idle, _iowait, irq, softirq, steal = fields[:8]
    return (user + nice + system + irq + softirq + steal) / TICK


def usage_s(path):
    try:
        with open(f"{path}/cpu.stat") as f:
            for line in f:
                if line.startswith("usage_usec"):
                    return int(line.split()[1]) / 1e6
    except OSError:
        return None
    return None


def own_cgroups():
    out = subprocess.run(
        [
            "docker",
            "ps",
            "--filter",
            f"name=^{os.environ.get('RSPERF_PROJECT', 'rsperf')}_",
            "--format",
            "{{.ID}}",
            "--no-trunc",
        ],
        capture_output=True,
        text=True,
        check=False,
    ).stdout.split()
    paths = []
    for cid in out:
        for parent in ("docker.slice/docker-cap.slice", "docker.slice", "system.slice"):
            path = f"{CG}/{parent}/docker-{cid}.scope"
            if os.path.isdir(path):
                paths.append(path)
                break
    return paths


def top_level():
    return {
        name: usage_s(f"{CG}/{name}")
        for name in sorted(os.listdir(CG))
        if os.path.isdir(f"{CG}/{name}") and name.endswith((".slice", ".scope"))
    }


def host_clock():
    """P-core (CPUs 0-11) average clock in MHz and package temperature in C, as a host-state snapshot."""
    mhz = []
    for cpu in range(12):
        try:
            with open(
                f"/sys/devices/system/cpu/cpu{cpu}/cpufreq/cpuinfo_avg_freq"
            ) as f:
                mhz.append(int(f.read()) / 1000)
        except (OSError, ValueError):
            pass
    temp = None
    for zone in sorted(os.listdir("/sys/class/thermal")):
        try:
            with open(f"/sys/class/thermal/{zone}/type") as f:
                if f.read().strip() != "x86_pkg_temp":
                    continue
            with open(f"/sys/class/thermal/{zone}/temp") as f:
                temp = int(f.read()) / 1000
        except OSError:
            continue
    return {
        "pcore_mhz": round(sum(mhz) / len(mhz)) if mhz else None,
        "pkg_temp_c": temp,
    }


class Window:
    """CPU accounting over one interval; the own-cgroup set is fixed at start (containers stay up)."""

    def __init__(self, extra_paths=()):
        self.paths = sorted(set(own_cgroups()) | set(extra_paths))
        self.t0 = time.monotonic()
        self.busy0 = host_busy_s()
        self.own0 = {p: usage_s(p) for p in self.paths}
        self.top0 = top_level()

    def finish(self):
        elapsed = time.monotonic() - self.t0
        busy = host_busy_s() - self.busy0
        own = 0.0
        for p, start in self.own0.items():
            end = usage_s(p)
            if start is not None and end is not None:
                own += end - start
        top1 = top_level()
        top = {}
        for k, v in self.top0.items():
            end = top1.get(k)
            if v is not None and end is not None:
                top[k] = round((end - v) / elapsed, 3)
        return {
            "window_s": round(elapsed, 1),
            "foreign_cores": round(max(busy - own, 0.0) / elapsed, 3),
            "busy_cores": round(busy / elapsed, 3),
            "own_cores": round(own / elapsed, 3),
            "own_cgroups": len(self.paths),
            "load1": os.getloadavg()[0],
            "top_cores": top,
            **host_clock(),
        }


def sample(seconds):
    window = Window()
    time.sleep(seconds)
    return window.finish()


if __name__ == "__main__":
    print(json.dumps(sample(float(sys.argv[1]) if len(sys.argv) > 1 else 10)))
