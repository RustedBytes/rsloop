"""Loop selection tests without optional native benchmark dependencies."""

import sys
import json
import unittest
from pathlib import Path
from types import SimpleNamespace
from unittest import mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "benches"))
import compare_event_loops as comparison
import workload_matrix as matrix


class BenchmarkLoopTests(unittest.TestCase):
    def test_child_commands_and_results_preserve_zuvloop(self):
        with mock.patch.object(sys, "argv", ["benchmark"]):
            micro_args = comparison.parse_args()
            matrix_args = matrix.parse_args()
        matrix.validate_args(matrix_args)
        command = matrix.child_command(
            matrix_args, "zuvloop", "http_keepalive", None, 1
        )
        self.assertEqual(command[command.index("--loop") + 1], "zuvloop")
        payload = {
            "loop": "zuvloop",
            "workload": "callbacks",
            "seconds": 0.1,
            "operations": 10,
            "peak_rss_bytes": 1024,
        }
        with mock.patch.object(
            comparison.subprocess,
            "run",
            return_value=SimpleNamespace(
                returncode=0, stdout=json.dumps(payload), stderr=""
            ),
        ) as run:
            result = comparison.run_child(
                "/tmp/benchmark.py", "zuvloop", "callbacks", micro_args
            )
        command = run.call_args.args[0]
        self.assertEqual(command[command.index("--loop") + 1], "zuvloop")
        self.assertEqual(result.loop, "zuvloop")

    def test_child_failure_is_not_replaced_with_another_loop(self):
        with mock.patch.object(sys, "argv", ["benchmark"]):
            args = matrix.parse_args()
        matrix.validate_args(args)
        with mock.patch.object(
            matrix.subprocess,
            "run",
            return_value=SimpleNamespace(
                returncode=1, stdout="", stderr="workload failed"
            ),
        ) as run:
            with self.assertRaisesRegex(RuntimeError, "zuvloop/http_keepalive failed"):
                matrix.run_child(args, "zuvloop", "http_keepalive")
            run.assert_called_once()

    def test_platform_and_python_defaults(self):
        for platform, backend in [
            ("linux", "uvloop"),
            ("darwin", "uvloop"),
            ("win32", "winloop"),
        ]:
            for version in [(3, 10), (3, 13), (3, 14), (3, 15)]:
                with (
                    self.subTest(platform=platform, version=version),
                    mock.patch.object(sys, "platform", platform),
                    mock.patch.object(sys, "version_info", version),
                ):
                    expected = ["asyncio", backend]
                    if version >= (3, 14):
                        expected.append("zuvloop")
                    expected.append("rsloop")
                    self.assertEqual(comparison.default_loops_csv(), ",".join(expected))
                    self.assertEqual(matrix.default_loops_csv(), ",".join(expected))

    def test_zuvloop_factory(self):
        factory = mock.Mock()
        with (
            mock.patch.object(sys, "version_info", (3, 14)),
            mock.patch.object(
                comparison.importlib,
                "import_module",
                return_value=SimpleNamespace(new_event_loop=factory),
            ) as imported,
        ):
            self.assertIs(comparison.loop_factory_for("zuvloop"), factory)
            imported.assert_called_once_with("zuvloop")
            factory.assert_not_called()

    def test_missing_zuvloop_reports_reason(self):
        with (
            mock.patch.object(sys, "version_info", (3, 14)),
            mock.patch.object(
                comparison.importlib,
                "import_module",
                side_effect=ModuleNotFoundError("no zuvloop"),
            ),
        ):
            available, reason = comparison.is_loop_available("zuvloop")
        self.assertFalse(available)
        self.assertIn("no zuvloop", reason)

    def test_older_python_rejects_before_import(self):
        with (
            mock.patch.object(sys, "version_info", (3, 13)),
            mock.patch.object(comparison.importlib, "import_module") as imported,
        ):
            available, reason = comparison.is_loop_available("zuvloop")
            self.assertFalse(available)
            self.assertIn("requires Python 3.14", reason)
            imported.assert_not_called()

    def test_both_parsers_accept_explicit_and_child_selection(self):
        for runner in (comparison, matrix):
            with (
                self.subTest(runner=runner.__name__),
                mock.patch.object(
                    sys,
                    "argv",
                    [
                        "benchmark",
                        "--loops",
                        "uvloop,zuvloop",
                        "--child",
                        "--loop",
                        "zuvloop",
                    ],
                ),
            ):
                args = runner.parse_args()
                self.assertEqual(args.loop, "zuvloop")
                self.assertEqual(
                    comparison.normalize_csv(
                        args.loops, allowed=comparison.LOOP_CHOICES, label="loops"
                    ),
                    ["uvloop", "zuvloop"],
                )
