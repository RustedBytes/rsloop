"""Contracts for reproducible, complete hotpath collection."""

import importlib.util
import json
import subprocess
import sys
from pathlib import Path
from types import SimpleNamespace

import pytest

pytestmark = pytest.mark.tooling
SPEC = importlib.util.spec_from_file_location(
    "profile_with_hotpath",
    Path(__file__).resolve().parents[2] / "scripts/profile_with_hotpath.py",
)
assert SPEC and SPEC.loader
profile = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(profile)


@pytest.mark.parametrize(
    "option,value",
    [
        ("--callbacks", "0"),
        ("--probe-iterations", "-1"),
        ("--time-sampling-rate", "nan"),
        ("--time-sampling-rate", "1.1"),
        ("--timeout", "inf"),
    ],
)
def test_invalid_profile_parameters(option, value):
    with pytest.raises(SystemExit):
        profile.parse_args(["callbacks", "--output", "report.json", option, value])


def test_environment_cannot_silently_filter_or_redirect_reports(monkeypatch):
    monkeypatch.setenv("HOTPATH_OUTPUT_PATH", "wrong.json")
    monkeypatch.setenv("HOTPATH_FUNCTIONS_LIMIT", "2")
    monkeypatch.setenv("HOTPATH_FOCUS", "irrelevant")
    monkeypatch.setenv("HOTPATH_OUTPUT_FORMAT", "none")
    env = profile.profile_environment(SimpleNamespace(focus="tls"))
    assert env["HOTPATH_LIMIT"] == "0"
    assert env["HOTPATH_FOCUS"] == "tls"
    assert "HOTPATH_OUTPUT_PATH" not in env
    assert "HOTPATH_FUNCTIONS_LIMIT" not in env
    assert "HOTPATH_OUTPUT_FORMAT" not in env


def test_child_preserves_module_arguments_and_profiler_settings(tmp_path):
    args = profile.parse_args(
        [
            "module",
            "--output",
            str(tmp_path / "profile.json"),
            "--module",
            "pytest",
            "--focus",
            "stream",
            "--time-sampling-rate",
            "0.2",
            "--module-args",
            "-q",
            "tests/test_tls.py",
        ]
    )
    command = profile.child_command(args, "module", args.output)
    assert profile.__file__ is not None
    assert command[:2] == [sys.executable, str(Path(profile.__file__).resolve())]
    parsed = profile.parse_args(command[2:])
    assert parsed.child
    assert parsed.time_sampling_rate == 0.2
    assert parsed.module_args == ["-q", "tests/test_tls.py"]


def test_coverage_keeps_unobserved_and_excluded_paths():
    entries = [
        {"name": "rsloop::seen", "status": "instrumented"},
        {"name": "rsloop::other_platform", "status": "instrumented"},
        {"name": "rsloop::signal_handler", "status": "ffi_callback"},
    ]
    report = {
        "functions_timing": {
            "total_count": 2,
            "included_count": 2,
            "data": [{"name": "rsloop"}, {"name": "rsloop::seen"}],
        }
    }
    result = profile.coverage(entries, [report])
    assert result["observed_names"] == 1
    assert result["instrumented_names"] == 2
    assert [r["observed"] for r in result["functions"]] == [True, False, False]
    assert not result["unmapped_observed_names"]
    assert json.loads(json.dumps(result)) == result


@pytest.mark.parametrize(
    "section", ["functions_timing", "functions_alloc", "futures", "threads"]
)
def test_truncated_reports_are_rejected(section):
    with pytest.raises(ValueError, match="truncated"):
        profile.report_names({section: {"total_count": 100, "included_count": 50}})


def test_existing_output_is_preserved(tmp_path):
    output = tmp_path / "report.json"
    output.write_text("existing")
    with pytest.raises(SystemExit, match="refusing to overwrite"):
        profile.main(["callbacks", "--output", str(output)])
    assert output.read_text() == "existing"


def test_suite_retains_failures_and_successful_reports(tmp_path, monkeypatch):
    monkeypatch.setattr(profile, "WORKLOADS", ("callbacks", "tasks"))
    monkeypatch.setattr(profile, "inventory", list)
    monkeypatch.setattr(profile.subprocess, "check_output", lambda *a, **k: "revision")

    def run(command, **kwargs):
        if command[2] == "callbacks":
            return SimpleNamespace(returncode=1, stdout="", stderr="workload failed")
        output = Path(command[command.index("--output") + 1])
        output.write_text(
            json.dumps(
                {
                    "functions_timing": {
                        "total_count": 1,
                        "included_count": 1,
                        "data": [{"name": "rsloop::tasks"}],
                    }
                }
            )
        )
        return SimpleNamespace(returncode=0, stdout="", stderr="")

    monkeypatch.setattr(profile.subprocess, "run", run)
    output = tmp_path / "suite"
    with pytest.raises(SystemExit, match="profiling failed: callbacks"):
        profile.main(["all", "--output", str(output)])
    manifest = json.loads((output / "manifest.json").read_text())
    assert [r["status"] for r in manifest["runs"]] == ["failed", "ok"]
    assert "workload failed" in (output / "callbacks.log").read_text()
    assert (output / "tasks.json").exists()
    assert (output / "coverage.json").exists()
    assert "callbacks | failed" in (output / "summary.md").read_text()


def test_timeout_retains_child_output(tmp_path, monkeypatch):
    monkeypatch.setattr(profile, "inventory", list)
    monkeypatch.setattr(profile.subprocess, "check_output", lambda *a, **k: "revision")

    def run(command, **kwargs):
        raise subprocess.TimeoutExpired(
            command, 1, output=b"partial output", stderr=b"error"
        )

    monkeypatch.setattr(profile.subprocess, "run", run)
    output = tmp_path / "timeout.json"
    with pytest.raises(SystemExit, match="profiling failed"):
        profile.main(["callbacks", "--output", str(output)])
    log = output.with_suffix(".log").read_text()
    assert "partial output" in log and "timed out" in log
