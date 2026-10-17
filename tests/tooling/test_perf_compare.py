"""Unit tests for the perf/bench comparison table."""

from __future__ import annotations

import importlib.util
import json
from pathlib import Path

import pytest

pytestmark = pytest.mark.tooling

# perf/bench is not a package; load the script by path.
_SPEC = importlib.util.spec_from_file_location(
    "perf_compare",
    Path(__file__).resolve().parents[2] / "perf" / "bench" / "compare.py",
)
assert _SPEC is not None and _SPEC.loader is not None
compare = importlib.util.module_from_spec(_SPEC)
_SPEC.loader.exec_module(compare)

ARMS = [("base", "base"), ("head", "head")]


def _measure(arm, rps, non_2xx=0, errors=None, contaminated=False):
    return {
        "kind": "measure",
        "contaminated": contaminated,
        "path": "/example",
        "concurrency": 16,
        "arm": arm,
        "rps": rps,
        "cpu_ms_per_1k": 50,
        "pg_sessions_per_1k": 1,
        "load1": 1,
        "load1_end": 1,
        "non_2xx": non_2xx,
        "errors": errors or {},
    }


def _table(tmp_path, monkeypatch, capsys, records):
    (tmp_path / "api.jsonl").write_text(
        "".join(json.dumps(record) + "\n" for record in records)
    )
    monkeypatch.setattr(compare, "OUT", str(tmp_path))
    compare.table("api", "", ARMS)
    return capsys.readouterr().out


def _row(output):
    return next(line for line in output.splitlines() if line.startswith("| `/"))


@pytest.mark.parametrize(
    "failure",
    [{"non_2xx": 2000}, {"errors": {"connection refused": 3}}],
    ids=["non_2xx", "transport_errors"],
)
def test_a_failed_measurement_never_counts_as_a_win(
    tmp_path, monkeypatch, capsys, failure
):
    output = _table(
        tmp_path,
        monkeypatch,
        capsys,
        [_measure("base", 1000), _measure("head", 2000, **failure)],
    )
    row = _row(output)
    assert "—, 1 failed" in row
    assert "ahead" not in row
    assert "+100.0%" not in row
    assert "geometric mean" not in output


def test_successful_rounds_still_compare_beside_a_failed_one(
    tmp_path, monkeypatch, capsys
):
    output = _table(
        tmp_path,
        monkeypatch,
        capsys,
        [
            _measure("base", 1000),
            _measure("base", 1100),
            _measure("head", 1200),
            _measure("head", 5000, non_2xx=1),
            # Contaminated rows are neither measured nor failed.
            _measure("head", 9000, contaminated=True, non_2xx=1),
        ],
    )
    row = _row(output)
    assert "1,200 (1,200–1,200, n=1), 1 failed" in row
    assert "+14.3%, ahead" in row
    assert "head vs base: geometric mean +14.3% over 1 rows" in output


def test_oha_end_of_run_entries_are_not_failures(tmp_path, monkeypatch, capsys):
    ended = {"aborted due to deadline": 16}
    output = _table(
        tmp_path,
        monkeypatch,
        capsys,
        [_measure("base", 1000, errors=ended), _measure("head", 1200, errors=ended)],
    )
    row = _row(output)
    assert "failed" not in row
    assert "1,200 (1,200–1,200, n=1)" in row
    assert "+20.0%, ahead" in row


def test_a_real_error_beside_the_end_of_run_entry_still_fails(
    tmp_path, monkeypatch, capsys
):
    errors = {"aborted due to deadline": 16, "connection reset": 1}
    output = _table(
        tmp_path,
        monkeypatch,
        capsys,
        [_measure("base", 1000), _measure("head", 2000, errors=errors)],
    )
    row = _row(output)
    assert "—, 1 failed" in row
    assert "ahead" not in row
    assert "geometric mean" not in output
