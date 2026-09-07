"""Loop selection tests without optional native benchmark dependencies."""

import json
import sys
from pathlib import Path
from types import SimpleNamespace
from unittest import mock

import pytest

sys.path.insert(0, str(Path(__file__).resolve().parents[1] / "benches"))
import compare_event_loops as comparison
import workload_matrix as matrix


class TestBenchmarkLoop:
    def test_child_commands_and_results_preserve_zuvloop(self):
        with mock.patch.object(sys, "argv", ["benchmark"]):
            micro_args = comparison.parse_args()
            matrix_args = matrix.parse_args()
        matrix.validate_args(matrix_args)
        command = matrix.child_command(
            matrix_args, "zuvloop", "http_keepalive", None, 1
        )
        assert command[command.index("--loop") + 1] == "zuvloop"
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
        assert command[command.index("--loop") + 1] == "zuvloop"
        assert result.loop == "zuvloop"

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
            with pytest.raises(RuntimeError, match="zuvloop/http_keepalive failed"):
                matrix.run_child(args, "zuvloop", "http_keepalive")
            run.assert_called_once()

    @pytest.mark.parametrize(
        "platform,backend",
        [
            ("linux", "uvloop"),
            ("darwin", "uvloop"),
            ("win32", "winloop"),
        ],
    )
    @pytest.mark.parametrize("version", [(3, 10), (3, 13), (3, 14), (3, 15)])
    def test_platform_and_python_defaults(self, platform, backend, version):
        with (
            mock.patch.object(sys, "platform", platform),
            mock.patch.object(sys, "version_info", version),
        ):
            expected = ["asyncio", backend]
            if version >= (3, 14):
                expected.append("zuvloop")
            expected.append("rsloop")
            assert comparison.default_loops_csv() == ",".join(expected)
            assert matrix.default_loops_csv() == ",".join(expected)

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
            assert comparison.loop_factory_for("zuvloop") is factory
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
        assert not available
        assert "no zuvloop" in reason

    def test_older_python_rejects_before_import(self):
        with (
            mock.patch.object(sys, "version_info", (3, 13)),
            mock.patch.object(comparison.importlib, "import_module") as imported,
        ):
            available, reason = comparison.is_loop_available("zuvloop")
            assert not available
            assert "requires Python 3.14" in reason
            imported.assert_not_called()

    @pytest.mark.parametrize(
        "runner", [comparison, matrix], ids=lambda runner: runner.__name__
    )
    def test_both_parsers_accept_explicit_and_child_selection(self, runner):
        with mock.patch.object(
            sys,
            "argv",
            ["benchmark", "--loops", "uvloop,zuvloop", "--child", "--loop", "zuvloop"],
        ):
            args = runner.parse_args()
            assert args.loop == "zuvloop"
            assert comparison.normalize_csv(
                args.loops, allowed=comparison.LOOP_CHOICES, label="loops"
            ) == ["uvloop", "zuvloop"]
