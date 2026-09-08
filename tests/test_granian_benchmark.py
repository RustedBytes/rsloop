"""Unit tests for the Granian benchmark harness."""

from __future__ import annotations

import json
import signal
import sys
from pathlib import Path
from types import SimpleNamespace

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "benches"))
import compare_granian as benchmark


def test_parse_oha_output() -> None:
    output = json.dumps(
        {
            "summary": {
                "requestsPerSec": 1234.5,
                "average": 0.002,
                "slowest": 0.008,
            },
            "latencyPercentiles": {
                "p50": 0.001,
                "p95": 0.004,
                "p99": 0.006,
            },
            "statusCodeDistribution": {"200": 999},
        }
    )
    assert benchmark.parse_oha_output(output) == {
        "requests": 999,
        "requests_per_second": 1234.5,
        "average_ms": 2.0,
        "p50_ms": 1.0,
        "p95_ms": 4.0,
        "p99_ms": 6.0,
        "max_ms": 8.0,
    }


def test_parse_oha_output_rejects_no_successes() -> None:
    output = json.dumps(
        {
            "summary": {
                "requestsPerSec": 1,
                "average": 1,
                "slowest": 1,
            },
            "latencyPercentiles": {"p50": 1, "p95": 1, "p99": 1},
            "statusCodeDistribution": {"200": 0},
        }
    )
    with pytest.raises(RuntimeError, match="without any HTTP 200"):
        benchmark.parse_oha_output(output)


def test_parse_oha_output_rejects_error_status() -> None:
    output = json.dumps(
        {
            "summary": {
                "requestsPerSec": 1,
                "average": 1,
                "slowest": 1,
            },
            "latencyPercentiles": {"p50": 1, "p95": 1, "p99": 1},
            "statusCodeDistribution": {"200": 1, "500": 1},
        }
    )
    with pytest.raises(RuntimeError, match="non-200"):
        benchmark.parse_oha_output(output)


def test_server_command_selects_loop(monkeypatch: pytest.MonkeyPatch) -> None:
    monkeypatch.setattr(sys, "argv", ["benchmark"])
    args = benchmark.parse_args()
    command = benchmark.server_command(args, "rsloop")
    assert command[command.index("--event-loop") + 1] == "rsloop"
    assert command[command.index("--workers") + 1] == "1"
    assert command[command.index("--runtime-threads") + 1] == "1"
    assert command[command.index("--task-impl") + 1] == "asyncio"


def test_oha_command_uses_duration_and_concurrency() -> None:
    command = benchmark.oha_command(
        "http://127.0.0.1:8000/benchmark", concurrency=64, duration=2.5
    )
    assert command[command.index("-c") + 1] == "64"
    assert command[command.index("-z") + 1] == "2.5s"
    assert command[-1].endswith("/benchmark")


def test_stop_server_reaps_forkserver_process_group(
    monkeypatch: pytest.MonkeyPatch, mocker
) -> None:
    process = SimpleNamespace(pid=1234, poll=mocker.Mock(return_value=0))
    killpg = mocker.patch.object(
        benchmark.os,
        "killpg",
        side_effect=[None, ProcessLookupError],
        create=True,
    )
    monkeypatch.setattr(benchmark.sys, "platform", "linux")

    benchmark.stop_server(process)

    assert killpg.call_args_list == [
        mocker.call(1234, signal.SIGTERM),
        mocker.call(1234, 0),
    ]


def test_selected_loops_rejects_duplicates() -> None:
    with pytest.raises(SystemExit, match="duplicates"):
        benchmark.selected_loops("asyncio,asyncio")
