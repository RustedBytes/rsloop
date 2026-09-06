"""Run-level uncertainty for idle activation (connections aren't independent trials)."""

from __future__ import annotations

import math
import random
import statistics


def latency_comparison(
    reference: list[float],
    candidate: list[float],
    *,
    paired: bool = True,
    threshold: float = 5.0,
    samples: int = 10_000,
    seed: int = 0,
) -> dict[str, object]:
    """Bootstrap whole process runs; negative change means lower latency.

    Paired inputs must be matched by experimental block, not arbitrary index.
    Separate baseline/candidate invocations must use independent resampling.
    The interval is an approximate percentile bootstrap 95% confidence interval.
    """
    if not reference or not candidate or (paired and len(reference) != len(candidate)):
        raise ValueError("nonempty matched runs are required for paired comparison")
    if any(not math.isfinite(x) or x <= 0 for x in reference + candidate):
        raise ValueError("latencies must be finite and positive")
    if samples <= 0 or not math.isfinite(threshold) or threshold < 0:
        raise ValueError("samples must be positive and threshold nonnegative")
    old = [math.log(x) for x in reference]
    new = [math.log(x) for x in candidate]
    differences = [b - a for a, b in zip(old, new)]
    rng = random.Random(seed)
    estimate = statistics.mean(new) - statistics.mean(old)
    boot = sorted(
        statistics.mean(rng.choices(differences, k=len(differences)))
        if paired
        else statistics.mean(rng.choices(new, k=len(new)))
        - statistics.mean(rng.choices(old, k=len(old)))
        for _ in range(samples)
    )
    low, high = (100 * math.expm1(boot[int(q * (samples - 1))]) for q in (0.025, 0.975))
    enough = min(len(old), len(new)) >= 7
    status = "inconclusive"
    if enough and high < -threshold:
        status = "improved"
    elif enough and low > threshold:
        status = "regressed"
    return {
        "metric": "median_cycle_p95_ms",
        "change_percent": 100 * math.expm1(estimate),
        "ci95_percent": [low, high] if enough else None,
        "classification": status,
        "threshold_percent": threshold,
        "reference_runs": len(old),
        "candidate_runs": len(new),
        "paired": paired,
        "bootstrap_samples": samples,
        "seed": seed,
        "reason": "fewer than 7 process runs" if not enough else "run-level bootstrap",
    }
