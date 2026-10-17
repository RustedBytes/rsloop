#!/usr/bin/env python3
"""rsloop-perf comparison table: median rps per row for any set of harness arms.

usage: compare.py <suffix> <api>... --arm <label>=<harness arm> [--arm ...]

Reads $RSPERF_ROOT/run/bench/<api><suffix>.jsonl (harness.py output). Only clean, successful rows
count: rows the quiet gate marked "contaminated" are measured again by the harness and ignored here,
and a measurement with any non-2xx response or transport error is a failed run, left out of every
median and delta and counted in its arm's cell ("k failed"). A row an arm never measured cleanly and
successfully is shown as "—".

The last --arm is the candidate. For every endpoint x concurrency the table gives each arm's median
rps with its min–max over the rounds, the candidate's change against every other arm (median against
median), and a noise reading per other arm: "within noise" when the two arms' round ranges overlap,
"ahead" or "behind" when they do not. CPU-ms per 1k requests (the API container's cgroup), new
Postgres sessions per 1k requests, load1 and the CPU used outside the benchmark's containers follow
as medians. The per-API footer gives the geometric mean of the candidate's ratios.

Example: compare.py _cmp api --arm uvloop=uvloop --arm 0.1.65=fork:v0165 \\
    --arm branch=fork:branch
"""

import argparse
import json
import math
import os
import statistics
from collections import defaultdict

OUT = os.path.join(os.path.abspath(os.environ.get("RSPERF_ROOT", ".")), "run", "bench")


def med(values):
    values = [v for v in values if v is not None]
    return statistics.median(values) if values else None


def f0(value):
    return "—" if value is None else f"{value:,.0f}"


def f1(value):
    return "—" if value is None else f"{value:.1f}"


def pct(value, base):
    return f"{(value / base - 1) * 100:+.1f}%" if value and base else "—"


def noise(candidate, other):
    if not candidate or not other:
        return "—"
    if min(candidate) > max(other):
        return "ahead"
    if max(candidate) < min(other):
        return "behind"
    return "within noise"


# oha ends every run at its time limit and reports the requests still in flight under this key of
# errorDistribution; that is the run ending as planned, not a failed request.
END_OF_RUN = "aborted due to deadline"


def failures(record):
    """Non-2xx responses plus transport errors (oha's errorDistribution) of one measurement."""
    errors = record.get("errors") or {}
    return (record.get("non_2xx") or 0) + sum(
        count for kind, count in errors.items() if kind != END_OF_RUN
    )


def load(api, suffix):
    """Clean successful measurements, and the count of clean failed ones, per (path, c, arm)."""
    rows = defaultdict(list)
    failed = defaultdict(int)
    with open(os.path.join(OUT, f"{api}{suffix}.jsonl")) as f:
        for line in f:
            record = json.loads(line)
            if record.get("kind") == "measure" and not record.get("contaminated"):
                key = (record["path"], record["concurrency"], record["arm"])
                if failures(record):
                    failed[key] += 1
                else:
                    rows[key].append(record)
    return rows, failed


def table(api, suffix, arms):
    rows, failed = load(api, suffix)
    labels = [label for label, _ in arms]
    candidate_label, candidate_arm = arms[-1]
    others = arms[:-1]
    keys = sorted({(path, c) for path, c, _ in [*rows, *failed]})
    head = ["endpoint", "c"]
    head += [f"{label} rps (min–max)" for label in labels]
    head += [f"Δ {candidate_label} vs {label}" for label, _ in others]
    head += ["CPU-ms/1k " + " / ".join(labels), "PG sessions/1k " + " / ".join(labels)]
    head += ["load1", "foreign cores"]
    print(f"\n#### {api}: median of clean rounds ({suffix})\n")
    print("| " + " | ".join(head) + " |")
    print("|" + "---|" * len(head))
    ratios = defaultdict(list)
    for path, c in keys:
        per_arm = {arm: rows.get((path, c, arm), []) for _, arm in arms}
        rps = {arm: [r["rps"] for r in per_arm[arm]] for _, arm in arms}
        cells = [f"`{path}`", str(c)]
        for _, arm in arms:
            values = rps[arm]
            lost = failed.get((path, c, arm), 0)
            cell = (
                f"{f0(med(values))} ({f0(min(values))}–{f0(max(values))}, n={len(values)})"
                if values
                else "—"
            )
            cells.append(f"{cell}, {lost} failed" if lost else cell)
        for label, arm in others:
            mine, theirs = med(rps[candidate_arm]), med(rps[arm])
            cells.append(f"{pct(mine, theirs)}, {noise(rps[candidate_arm], rps[arm])}")
            if mine and theirs:
                ratios[label].append(mine / theirs)
        cells.append(
            " / ".join(
                f0(med(r["cpu_ms_per_1k"] for r in per_arm[arm])) for _, arm in arms
            )
        )
        cells.append(
            " / ".join(
                f1(med(r["pg_sessions_per_1k"] for r in per_arm[arm]))
                for _, arm in arms
            )
        )
        everything = [r for _, arm in arms for r in per_arm[arm]]
        loads = [r["load1"] for r in everything] + [r["load1_end"] for r in everything]
        foreign = [(r.get("quiet") or {}).get("foreign_cores") for r in everything]
        foreign = [v for v in foreign if v is not None]
        cells.append(f"{min(loads):.1f}–{max(loads):.1f}" if loads else "—")
        cells.append(f"{min(foreign):.2f}–{max(foreign):.2f}" if foreign else "—")
        print("| " + " | ".join(cells) + " |")
    for label, values in ratios.items():
        geomean = math.exp(sum(math.log(v) for v in values) / len(values))
        print(
            f"\n{candidate_label} vs {label}: geometric mean {(geomean - 1) * 100:+.1f}% over {len(values)} rows"
        )


def main():
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument("suffix")
    parser.add_argument("apis", nargs="+")
    parser.add_argument("--arm", action="append", required=True, metavar="LABEL=ARM")
    args = parser.parse_args()
    arms = [tuple(spec.split("=", 1)) for spec in args.arm]
    if len(arms) < 2 or any(len(arm) != 2 for arm in arms):
        parser.error("give at least two --arm LABEL=ARM")
    for api in args.apis:
        table(api, args.suffix, arms)


if __name__ == "__main__":
    main()
