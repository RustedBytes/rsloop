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
import unittest
from pathlib import Path
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "benches"))
import check_regression as gate
import workload_matrix as matrix
from idle_statistics import latency_comparison


def arguments(*extra: str) -> argparse.Namespace:
    with mock.patch.object(sys, "argv", ["matrix", *extra]):
        args = matrix.parse_args()
    matrix.validate_args(args)
    return args


class IdleBenchmarkTests(unittest.TestCase):
    def test_shared_origin_includes_delay_before_later_clients_start(self):
        args = arguments(
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

        with mock.patch.object(asyncio, "open_connection", side_effect=opened):
            result = asyncio.run(matrix.run_idle_connections("asyncio", args))
        self.assertGreaterEqual(min(result.latency_ms), 10)

    def test_cycles_reuse_connections_and_exclude_warmups(self):
        args = arguments(
            "--idle-connections",
            "4",
            "--idle-cycles",
            "3",
            "--idle-warmup-cycles",
            "2",
            "--idle-seconds",
            "0",
        )
        original = asyncio.open_connection
        with mock.patch.object(asyncio, "open_connection", wraps=original) as opened:
            result = asyncio.run(matrix.run_idle_connections("asyncio", args))
        self.assertEqual(opened.call_count, 4)
        self.assertEqual(result.operations, 12)
        self.assertEqual(result.bytes_transferred, 24)
        self.assertEqual(len(result.latency_ms), 12)
        self.assertEqual(len(result.idle_cycles), 3)
        self.assertEqual(result.benchmark_version, 2)
        self.assertGreater(result.warmup_seconds, 0)
        self.assertAlmostEqual(
            result.traffic_seconds,
            sum(c["traffic_seconds"] for c in result.idle_cycles),
        )
        for cycle in result.idle_cycles:
            self.assertLessEqual(cycle["first_ms"], cycle["p50_ms"])
            self.assertLessEqual(cycle["p50_ms"], cycle["p95_ms"])
            self.assertLessEqual(cycle["p95_ms"], cycle["all_ms"])
            self.assertLessEqual(cycle["all_ms"], cycle["traffic_seconds"] * 1000)
        self.assertEqual(
            matrix.MatrixResult(**json.loads(json.dumps(matrix.asdict(result)))), result
        )

    def test_activation_timeout_closes_connections(self):
        args = arguments(
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

        with (
            mock.patch.object(asyncio, "open_connection", side_effect=opened),
            self.assertRaises(asyncio.TimeoutError),
        ):
            asyncio.run(matrix.run_idle_connections("asyncio", args))
        self.assertTrue(writers)
        self.assertTrue(all(w.is_closing() for w in writers))

    def test_invalid_settings(self):
        for flags in [
            ("--idle-cycles", "0"),
            ("--idle-warmup-cycles", "-1"),
            ("--idle-timeout", "nan"),
            ("--idle-timeout", "0"),
            ("--idle-seconds", "inf"),
        ]:
            with self.subTest(flags=flags), self.assertRaises(SystemExit):
                arguments(*flags)

    def test_child_options_are_forwarded(self):
        args = arguments("--idle-cycles", "17", "--idle-warmup-cycles", "2")
        cmd = matrix.child_command(args, "asyncio", "idle_connections")
        self.assertEqual(cmd[cmd.index("--idle-cycles") + 1], "17")
        self.assertEqual(cmd[cmd.index("--idle-warmup-cycles") + 1], "2")

    def test_parent_alternates_fresh_process_blocks(self):
        args = arguments(
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
            with (
                mock.patch.object(matrix, "is_loop_available", return_value=(True, "")),
                mock.patch.object(matrix, "run_child", side_effect=child),
                contextlib.redirect_stdout(io.StringIO()),
            ):
                self.assertEqual(matrix.parent_main(args), 0)
            result = json.loads(args.json_output.read_text())
        self.assertEqual(calls, ["rsloop", "uvloop", "uvloop", "rsloop"] * 2)
        self.assertEqual(result[0]["measurement_mode"], "paired-cold")
        self.assertEqual(result[0]["idle_comparison"]["classification"], "inconclusive")
        self.assertEqual(len(result[0]["runs"]), 4)

    def test_inference_uses_process_runs_not_connections(self):
        for ratio, expected in [
            (0.8, "improved"),
            (1.2, "regressed"),
            (1.02, "inconclusive"),
        ]:
            result = latency_comparison([10.0] * 7, [10 * ratio] * 7, samples=200)
            self.assertEqual(result["classification"], expected)
        self.assertEqual(
            latency_comparison([10.0], [1.0], samples=200)["classification"],
            "inconclusive",
        )
        self.assertEqual(
            latency_comparison([10.0] * 8, [2.0, 40.0] * 4, samples=1000)[
                "classification"
            ],
            "inconclusive",
        )
        independent = latency_comparison(
            [10.0] * 7, [20.0] * 9, paired=False, samples=200
        )
        self.assertEqual(independent["classification"], "regressed")
        with self.assertRaises(ValueError):
            latency_comparison([1], [1, 2])
        with self.assertRaises(ValueError):
            latency_comparison([0], [1])

    def test_gate_rejects_legacy_and_v2_comparison(self):
        old = {"idle_connections": {"throughput": 100}}
        new = {"idle_connections": {"benchmark_version": 2, "settings": {}}}
        with (
            mock.patch.object(
                gate,
                "parse_args",
                return_value=argparse.Namespace(
                    baseline="old", candidate="new", require_improvement=0
                ),
            ),
            mock.patch.object(gate, "load_metrics", side_effect=[old, new]),
            contextlib.redirect_stdout(io.StringIO()),
        ):
            self.assertEqual(gate.main(), 1)

    def test_v2_gate_distinguishes_regressed_improved_and_inconclusive(self):
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
            with (
                mock.patch.object(
                    gate,
                    "parse_args",
                    return_value=argparse.Namespace(
                        baseline="old",
                        candidate="new",
                        require_improvement=0,
                        latency_regression=5,
                    ),
                ),
                mock.patch.object(gate, "load_metrics", side_effect=[old, new]),
                contextlib.redirect_stdout(io.StringIO()),
            ):
                self.assertEqual(gate.main(), expected_code)

    def test_short_sample_has_no_confidence_interval(self):
        result = latency_comparison([10.0] * 3, [1.0] * 3, samples=200)
        self.assertIsNone(result["ci95_percent"])

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
        self.assertEqual(result.benchmark_version, 1)
        self.assertEqual(result.idle_cycles, [])
