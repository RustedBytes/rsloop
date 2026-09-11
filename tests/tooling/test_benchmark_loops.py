"""Loop selection tests without optional native benchmark dependencies."""

import json
import sys
from pathlib import Path
from types import SimpleNamespace

import pytest

pytestmark = pytest.mark.tooling

sys.path.insert(0, str(Path(__file__).resolve().parents[2] / "benches"))
import compare_event_loops as comparison
import workload_matrix as matrix


class TestBenchmarkLoop:
    def test_python315_profiler_command_writes_native_thread_flamegraph(
        self, tmp_path, monkeypatch, mocker
    ):
        monkeypatch.setattr(comparison.sys, "version_info", (3, 15))
        mocker.patch.object(
            comparison.importlib.util, "find_spec", return_value=object()
        )
        output = tmp_path / "callbacks.profile"

        command = comparison.sampling_profiler_command(
            [sys.executable, "benchmark.py", "--child"], output
        )

        assert command[:4] == [
            sys.executable,
            "-m",
            "profiling.sampling",
            "run",
        ]
        assert "--all-threads" in command
        assert "--native" in command
        assert "--flamegraph" in command
        assert command[command.index("-o") + 1] == str(
            output.resolve().with_suffix(".html")
        )
        assert command[-2:] == ["benchmark.py", "--child"]

    def test_profiler_command_rejects_python_before_315(self, monkeypatch):
        monkeypatch.setattr(comparison.sys, "version_info", (3, 14))

        with pytest.raises(RuntimeError, match="Python 3.15 or newer"):
            comparison.sampling_profiler_command(
                [sys.executable, "benchmark.py"], Path("profile.html")
            )

    def test_matrix_profile_wraps_child_and_parses_output(
        self, tmp_path, monkeypatch, mocker
    ):
        monkeypatch.setattr(sys, "argv", ["benchmark"])
        args = matrix.parse_args()
        matrix.validate_args(args)
        wrapped_command = [sys.executable, "-m", "profiling.sampling", "run"]
        wrapper = mocker.patch.object(
            matrix, "sampling_profiler_command", return_value=wrapped_command
        )
        payload = {
            "loop": "rsloop",
            "scenario": "http_keepalive",
            "seconds": 0.1,
            "operations": 10,
            "bytes_transferred": 100,
            "latency_ms": [1.0],
        }
        run = mocker.patch.object(
            matrix.subprocess,
            "run",
            return_value=SimpleNamespace(
                returncode=0,
                stdout=f"{json.dumps(payload)}\nFlamegraph saved\n",
                stderr="",
            ),
        )
        output = tmp_path / "matrix.html"

        result = matrix.run_child(args, "rsloop", "http_keepalive", output)

        assert result.scenario == "http_keepalive"
        wrapper.assert_called_once()
        assert wrapper.call_args.args[1] == output
        assert run.call_args.args[0] == wrapped_command

    def test_child_commands_and_results_preserve_zuvloop(self, monkeypatch, mocker):
        monkeypatch.setattr(sys, "argv", ["benchmark"])
        micro_args = comparison.parse_args()
        matrix_args = matrix.parse_args()
        matrix.validate_args(matrix_args)
        command = matrix.child_command(matrix_args, "zuvloop", "http_keepalive", 1)
        assert command[command.index("--loop") + 1] == "zuvloop"
        payload = {
            "loop": "zuvloop",
            "workload": "callbacks",
            "seconds": 0.1,
            "operations": 10,
            "peak_rss_bytes": 1024,
        }
        run = mocker.patch.object(
            comparison.subprocess,
            "run",
            return_value=SimpleNamespace(
                returncode=0, stdout=json.dumps(payload), stderr=""
            ),
        )
        result = comparison.run_child(
            "/tmp/benchmark.py", "zuvloop", "callbacks", micro_args
        )
        command = run.call_args.args[0]
        assert command[command.index("--loop") + 1] == "zuvloop"
        assert result.loop == "zuvloop"

    def test_child_failure_is_not_replaced_with_another_loop(self, monkeypatch, mocker):
        monkeypatch.setattr(sys, "argv", ["benchmark"])
        args = matrix.parse_args()
        matrix.validate_args(args)
        run = mocker.patch.object(
            matrix.subprocess,
            "run",
            return_value=SimpleNamespace(
                returncode=1, stdout="", stderr="workload failed"
            ),
        )
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
    def test_platform_and_python_defaults(
        self, platform, backend, version, monkeypatch
    ):
        with monkeypatch.context() as patch:
            patch.setattr(sys, "platform", platform)
            patch.setattr(sys, "version_info", version)
            expected = ["asyncio", backend]
            if version >= (3, 14):
                expected.append("zuvloop")
            expected.append("rsloop")
            assert comparison.default_loops_csv() == ",".join(expected)
            assert matrix.default_loops_csv() == ",".join(expected)

    def test_zuvloop_factory(self, monkeypatch, mocker):
        factory = mocker.Mock()
        imported = mocker.Mock(return_value=SimpleNamespace(new_event_loop=factory))
        with monkeypatch.context() as patch:
            patch.setattr(sys, "version_info", (3, 14))
            patch.setattr(comparison.importlib, "import_module", imported)
            assert comparison.loop_factory_for("zuvloop") is factory
            imported.assert_called_once_with("zuvloop")
            factory.assert_not_called()

    def test_missing_zuvloop_reports_reason(self, monkeypatch, mocker):
        imported = mocker.Mock(side_effect=ModuleNotFoundError("no zuvloop"))
        with monkeypatch.context() as patch:
            patch.setattr(sys, "version_info", (3, 14))
            patch.setattr(comparison.importlib, "import_module", imported)
            available, reason = comparison.is_loop_available("zuvloop")
        assert not available
        assert reason is not None
        assert "no zuvloop" in reason

    def test_older_python_rejects_before_import(self, monkeypatch, mocker):
        imported = mocker.Mock()
        with monkeypatch.context() as patch:
            patch.setattr(sys, "version_info", (3, 13))
            patch.setattr(comparison.importlib, "import_module", imported)
            available, reason = comparison.is_loop_available("zuvloop")
            assert not available
            assert reason is not None
            assert "requires Python 3.14" in reason
            imported.assert_not_called()

    @pytest.mark.parametrize(
        "runner", [comparison, matrix], ids=lambda runner: runner.__name__
    )
    def test_both_parsers_accept_explicit_and_child_selection(
        self, runner, monkeypatch
    ):
        monkeypatch.setattr(
            sys,
            "argv",
            ["benchmark", "--loops", "uvloop,zuvloop", "--child", "--loop", "zuvloop"],
        )
        args = runner.parse_args()
        assert args.loop == "zuvloop"
        assert comparison.normalize_csv(
            args.loops, allowed=comparison.LOOP_CHOICES, label="loops"
        ) == ["uvloop", "zuvloop"]
