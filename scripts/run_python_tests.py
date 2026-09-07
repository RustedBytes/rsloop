"""Run pytest with recurring diagnostics for tests that stop making progress."""

from __future__ import annotations

import faulthandler
import os
import sys
from pathlib import Path

import pytest

ROOT_DIR = Path(__file__).resolve().parents[1]


def _traceback_interval() -> int:
    raw_value = os.environ.get("RSLOOP_TEST_TRACEBACK_INTERVAL_SECONDS", "60")
    try:
        interval = int(raw_value)
    except ValueError as exc:
        raise SystemExit(
            "RSLOOP_TEST_TRACEBACK_INTERVAL_SECONDS must be an integer"
        ) from exc
    if interval <= 0:
        raise SystemExit(
            "RSLOOP_TEST_TRACEBACK_INTERVAL_SECONDS must be greater than zero"
        )
    return interval


def main() -> int:
    os.chdir(ROOT_DIR)
    sys.path.insert(0, str(ROOT_DIR))
    faulthandler.enable()
    faulthandler.dump_traceback_later(_traceback_interval(), repeat=True)
    try:
        # Own the recurring watchdog: pytest's faulthandler plugin otherwise
        # cancels it after each test. Forward selectors and other pytest flags.
        return int(pytest.main(["-v", "-s", "-p", "no:faulthandler", *sys.argv[1:]]))
    finally:
        faulthandler.cancel_dump_traceback_later()


if __name__ == "__main__":
    sys.exit(main())
