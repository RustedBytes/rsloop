"""Allocation/lifetime regressions without noisy wall-clock or RSS thresholds."""

import json
import os
import subprocess
import sys
from pathlib import Path

import pytest

pytestmark = [
    pytest.mark.tooling,
    pytest.mark.skipif(sys.platform != "linux", reason="Linux RSS probe"),
]
SCRIPT = Path(__file__).resolve().parents[2] / "benches" / "callback_rss.py"


def probe(tmp_path, *args):
    output = tmp_path / "sample.json"
    subprocess.run(
        [
            sys.executable,
            "-X",
            "context_aware_warnings=1",
            str(SCRIPT),
            "--count",
            "4096",
            "--repeat",
            "1",
            "--warmups",
            "0",
            "--output",
            str(output),
            *args,
        ],
        check=True,
        capture_output=True,
        text=True,
        env=os.environ.copy(),
    )
    return json.loads(output.read_text())["samples"][0]


def test_child_preserves_interpreter_options_and_empty_context(tmp_path):
    sample = probe(tmp_path, "--context", "empty", "--mode", "lifetime")
    assert sample["metadata"]["xoptions"]["context_aware_warnings"] == "1"
    assert sample["context_keys"] == []
    phases = {item["phase"]: item for item in sample["phases"]}
    assert phases["queued"]["sampled_alive"] == sample["sampled"]
    assert phases["resumed"]["sampled_alive"] == 0
    assert phases["gc"]["sampled_alive"] == 0


def test_nonempty_context_adds_one_snapshot_per_callback(tmp_path):
    empty = probe(tmp_path, "--context", "empty", "--mode", "allocations")
    nonempty = probe(tmp_path, "--context", "nonempty", "--mode", "allocations")
    empty_alloc = max(empty["queued_allocations"], key=lambda item: item["bytes"])
    nonempty_alloc = max(nonempty["queued_allocations"], key=lambda item: item["bytes"])
    # The common scheduling line owns N handles, plus N snapshots only when
    # the implicit context has a binding. Small task/stop allocations are allowed.
    count = empty["count"]
    assert abs(empty_alloc["count"] - count) < 10
    assert abs(nonempty_alloc["count"] - 2 * count) < 10
    assert nonempty_alloc["bytes"] > empty_alloc["bytes"]
    assert nonempty["context_keys"] == ["benchmark_value"]
    assert empty["cleanup_traced"][0] < 32_768
    assert nonempty["cleanup_traced"][0] < 32_768


def test_cross_thread_handles_release_callback_payloads(tmp_path):
    sample = probe(
        tmp_path, "--context", "empty", "--mode", "cleanup", "--producer", "thread"
    )
    phases = {item["phase"]: item for item in sample["phases"]}
    assert phases["queued"]["payload_alive"] == sample["sampled"]
    assert phases["gc"]["sampled_alive"] == 0
    assert phases["gc"]["payload_alive"] == 0
    assert phases["gc"]["payload_released"] == sample["sampled"]
