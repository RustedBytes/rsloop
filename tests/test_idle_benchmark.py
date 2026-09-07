"""Tests for measurement semantics, process pairing, and run-level inference."""

from __future__ import annotations

import argparse
import asyncio
import contextlib
import io
import json
import sys
import tempfile
import time
from pathlib import Path

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "benches"))
import check_regression as gate
import workload_matrix as matrix
from idle_statistics import latency_comparison


def arguments(monkeypatch, *extra: str) -> argparse.Namespace:
    with monkeypatch.context() as patch:
        patch.setattr(sys, "argv", ["matrix", *extra])
        args = matrix.parse_args()
    matrix.validate_args(args)
    return args


class TestIdleBenchmark:
    def test_shared_origin_includes_delay_before_later_clients_start(self, monkeypatch):
        args = arguments(
            monkeypatch,
            "--idle-connections",
            "2",
            "--idle-cycles",
            "1",
            "--idle-warmup-cycles",
            "0",
            "--idle-seconds",
            "0",
        )
        original = asyncio.open_connection
        first = True

        async def opened(*a, **kw):
            nonlocal first
            reader, writer = await original(*a, **kw)
            if first:
                first = False
                write = writer.write

                def delayed(data):
                    time.sleep(0.01)
                    write(data)

                writer.write = delayed
            return reader, writer

        with monkeypatch.context() as patch:
            patch.setattr(asyncio, "open_connection", opened)
            result = asyncio.run(matrix.run_idle_connections("asyncio", args))
        assert min(result.latency_ms) >= 10

    def test_cycles_reuse_connections_and_exclude_warmups(self, monkeypatch, mocker):
        args = arguments(
            monkeypatch,
            "--idle-connections",
            "4",
            "--idle-cycles",
            "3",
            "--idle-warmup-cycles",
            "2",
            "--idle-seconds",
            "0",
        )
        opened = mocker.spy(asyncio, "open_connection")
        try:
            result = asyncio.run(matrix.run_idle_connections("asyncio", args))
        finally:
            mocker.stop(opened)
        assert opened.call_count == 4
        assert result.operations == 12
        assert result.bytes_transferred == 24
        assert len(result.latency_ms) == 12
        assert len(result.idle_cycles) == 3
        assert result.benchmark_version == 2
        assert result.warmup_seconds > 0
        assert result.traffic_seconds == pytest.approx(
            sum(c["traffic_seconds"] for c in result.idle_cycles), rel=0, abs=5e-08
        )
        for cycle in result.idle_cycles:
            assert cycle["first_ms"] <= cycle["p50_ms"]
            assert cycle["p50_ms"] <= cycle["p95_ms"]
            assert cycle["p95_ms"] <= cycle["all_ms"]
            assert cycle["all_ms"] <= cycle["traffic_seconds"] * 1000
        assert (
            matrix.MatrixResult(**json.loads(json.dumps(matrix.asdict(result))))
            == result
        )

    def test_activation_timeout_closes_connections(self, monkeypatch):
        args = arguments(
            monkeypatch,
            "--idle-connections",
            "2",
            "--idle-cycles",
            "1",
            "--idle-warmup-cycles",
            "0",
            "--idle-seconds",
            "0",
            "--idle-timeout",
            "0.05",
        )
        original = asyncio.open_connection
        writers = []

        async def opened(*a, **kw):
            reader, writer = await original(*a, **kw)
            writers.append(writer)

            async def never_reply(n):
                await asyncio.Future()

            reader.readexactly = never_reply
            return reader, writer

        with monkeypatch.context() as patch:
            patch.setattr(asyncio, "open_connection", opened)
            with pytest.raises(asyncio.TimeoutError):
                asyncio.run(matrix.run_idle_connections("asyncio", args))
        assert writers
        assert all(w.is_closing() for w in writers)

    @pytest.mark.parametrize(
        "flags",
        [
            ("--idle-cycles", "0"),
            ("--idle-warmup-cycles", "-1"),
            ("--idle-timeout", "nan"),
            ("--idle-timeout", "0"),
            ("--idle-seconds", "inf"),
        ],
    )
    def test_invalid_settings(self, flags, monkeypatch):
        with pytest.raises(SystemExit):
            arguments(monkeypatch, *flags)

    def test_child_options_are_forwarded(self, monkeypatch):
        args = arguments(
            monkeypatch, "--idle-cycles", "17", "--idle-warmup-cycles", "2"
        )
        cmd = matrix.child_command(args, "asyncio", "idle_connections")
        assert cmd[cmd.index("--idle-cycles") + 1] == "17"
        assert cmd[cmd.index("--idle-warmup-cycles") + 1] == "2"

    def test_parent_alternates_fresh_process_blocks(self, monkeypatch, mocker):
        args = arguments(
            monkeypatch,
            "--loops",
            "rsloop,uvloop",
            "--scenarios",
            "idle_connections",
            "--repeat",
            "4",
            "--warmups",
            "9",
        )
        calls = []

        def child(args, name, scenario):
            calls.append(name)
            return matrix.MatrixResult(
                name,
                scenario,
                1,
                1,
                2,
                [1],
                benchmark_version=2,
                idle_cycles=[
                    {"first_ms": 1.0, "p50_ms": 1.0, "p95_ms": 1.0, "all_ms": 1.0}
                ],
            )

        with tempfile.TemporaryDirectory() as directory:
            args.json_output = Path(directory) / "result.json"
            mocker.patch.object(matrix, "is_loop_available", return_value=(True, ""))
            monkeypatch.setattr(matrix, "run_child", child)
            with contextlib.redirect_stdout(io.StringIO()):
                assert matrix.parent_main(args) == 0
            result = json.loads(args.json_output.read_text())
        assert calls == ["rsloop", "uvloop", "uvloop", "rsloop"] * 2
        assert result[0]["measurement_mode"] == "paired-cold"
        assert result[0]["idle_comparison"]["classification"] == "inconclusive"
        assert len(result[0]["runs"]) == 4

    def test_inference_uses_process_runs_not_connections(self):
        for ratio, expected in [
            (0.8, "improved"),
            (1.2, "regressed"),
            (1.02, "inconclusive"),
        ]:
            result = latency_comparison([10.0] * 7, [10 * ratio] * 7, samples=200)
            assert result["classification"] == expected
        assert (
            latency_comparison([10.0], [1.0], samples=200)["classification"]
            == "inconclusive"
        )
        assert (
            latency_comparison([10.0] * 8, [2.0, 40.0] * 4, samples=1000)[
                "classification"
            ]
            == "inconclusive"
        )
        independent = latency_comparison(
            [10.0] * 7, [20.0] * 9, paired=False, samples=200
        )
        assert independent["classification"] == "regressed"
        with pytest.raises(ValueError):
            latency_comparison([1], [1, 2])
        with pytest.raises(ValueError):
            latency_comparison([0], [1])

    def test_gate_rejects_legacy_and_v2_comparison(self, mocker):
        old = {"idle_connections": {"throughput": 100}}
        new = {"idle_connections": {"benchmark_version": 2, "settings": {}}}
        mocker.patch.object(
            gate,
            "parse_args",
            return_value=argparse.Namespace(
                baseline="old", candidate="new", require_improvement=0
            ),
        )
        mocker.patch.object(gate, "load_metrics", side_effect=[old, new])
        with contextlib.redirect_stdout(io.StringIO()):
            assert gate.main() == 1

    def test_v2_gate_distinguishes_regressed_improved_and_inconclusive(
        self, monkeypatch, mocker
    ):
        for scale, expected_code in [(1.2, 1), (0.8, 0), (1.0, 2)]:
            old = {
                "idle_connections": {
                    "benchmark_version": 2,
                    "settings": {"cycles": 100},
                    "samples": [10.0] * 8,
                }
            }
            new = {
                "idle_connections": {
                    "benchmark_version": 2,
                    "settings": {"cycles": 100},
                    "samples": [10.0 * scale] * 8,
                }
            }
            with monkeypatch.context() as patch:
                patch.setattr(
                    gate,
                    "parse_args",
                    mocker.Mock(
                        return_value=argparse.Namespace(
                            baseline="old",
                            candidate="new",
                            require_improvement=0,
                            latency_regression=5,
                        )
                    ),
                )
                patch.setattr(gate, "load_metrics", mocker.Mock(side_effect=[old, new]))
                with contextlib.redirect_stdout(io.StringIO()):
                    assert gate.main() == expected_code

    def test_short_sample_has_no_confidence_interval(self):
        result = latency_comparison([10.0] * 3, [1.0] * 3, samples=200)
        assert result["ci95_percent"] is None

    def test_legacy_result_can_still_be_read(self):
        legacy = {
            "loop": "asyncio",
            "scenario": "http_keepalive",
            "seconds": 1.0,
            "operations": 1,
            "bytes_transferred": 2,
            "latency_ms": [1.0],
        }
        result = matrix.MatrixResult(**legacy)
        assert result.benchmark_version == 1
        assert result.idle_cycles == []
