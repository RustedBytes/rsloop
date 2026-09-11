"""Keep the CI wrapper consistent with pytest's selection and exit codes."""

import sys

import pytest

from scripts import run_all_tests as all_runner
from scripts import run_python_tests as runner

pytestmark = pytest.mark.tooling


@pytest.mark.parametrize("value,expected", [(None, 60), ("1", 1), ("120", 120)])
def test_traceback_interval(monkeypatch, value, expected):
    monkeypatch.delenv("RSLOOP_TEST_TRACEBACK_INTERVAL_SECONDS", raising=False)
    if value is not None:
        monkeypatch.setenv("RSLOOP_TEST_TRACEBACK_INTERVAL_SECONDS", value)
    assert runner._traceback_interval() == expected


@pytest.mark.parametrize(
    "value,message",
    [
        ("invalid", "must be an integer"),
        ("0", "must be greater than zero"),
        ("-1", "must be greater than zero"),
    ],
)
def test_invalid_traceback_interval(monkeypatch, value, message):
    monkeypatch.setenv("RSLOOP_TEST_TRACEBACK_INTERVAL_SECONDS", value)
    with pytest.raises(SystemExit, match=message):
        runner._traceback_interval()


@pytest.fixture
def wrapper_calls(monkeypatch, mocker):
    calls = mocker.Mock()
    monkeypatch.setattr(runner.os, "chdir", calls.chdir)
    monkeypatch.setattr(sys, "path", sys.path.copy())
    monkeypatch.setattr(
        sys, "argv", ["run_python_tests.py", "tests/test_run.py", "-k", "stats"]
    )
    monkeypatch.setenv("RSLOOP_TEST_TRACEBACK_INTERVAL_SECONDS", "17")
    monkeypatch.setattr(runner.faulthandler, "enable", calls.enable)
    monkeypatch.setattr(runner.faulthandler, "dump_traceback_later", calls.start)
    monkeypatch.setattr(
        runner.faulthandler, "cancel_dump_traceback_later", calls.cancel
    )
    monkeypatch.setattr(runner.pytest, "main", calls.pytest)
    return calls


@pytest.mark.parametrize(
    "exit_code",
    [
        pytest.ExitCode.OK,
        pytest.ExitCode.TESTS_FAILED,
        pytest.ExitCode.NO_TESTS_COLLECTED,
    ],
)
def test_wrapper_forwards_arguments_and_exit_code(wrapper_calls, exit_code, mocker):
    wrapper_calls.pytest.return_value = exit_code
    assert runner.main() == int(exit_code)
    wrapper_calls.assert_has_calls(
        [
            mocker.call.chdir(runner.ROOT_DIR),
            mocker.call.enable(),
            mocker.call.start(17, repeat=True),
            mocker.call.pytest(
                [
                    "-p",
                    "no:faulthandler",
                    "tests/test_run.py",
                    "-k",
                    "stats",
                ]
            ),
            mocker.call.cancel(),
        ]
    )
    assert sys.path[0] == str(runner.ROOT_DIR)


def test_wrapper_cancels_watchdog_on_runner_error(wrapper_calls):
    wrapper_calls.pytest.side_effect = RuntimeError("runner failed")
    with pytest.raises(RuntimeError, match="runner failed"):
        runner.main()
    wrapper_calls.cancel.assert_called_once_with()


@pytest.mark.parametrize("exit_codes,expected", [((0, 0), 0), ((0, 5), 5)])
def test_combined_runner_waits_for_both_suites(exit_codes, expected, mocker):
    rust = mocker.Mock()
    rust.wait.return_value = exit_codes[0]
    python = mocker.Mock()
    python.wait.return_value = exit_codes[1]
    popen = mocker.patch.object(
        all_runner.subprocess, "Popen", side_effect=[rust, python]
    )

    assert all_runner.main() == expected
    assert popen.call_count == 2
    rust.wait.assert_called_once_with()
    python.wait.assert_called_once_with()
