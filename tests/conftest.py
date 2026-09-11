"""Shared pytest controls for fast and stress-level repetition."""

from __future__ import annotations

from collections.abc import Callable

import pytest


def pytest_addoption(parser: pytest.Parser) -> None:
    parser.addoption(
        "--stress-iterations",
        action="store_true",
        help="use high repetition counts for tests marked as stress tests",
    )


@pytest.fixture
def iteration_count(request: pytest.FixtureRequest) -> Callable[[int, int], int]:
    use_stress_counts = bool(request.config.getoption("--stress-iterations"))

    def select(normal: int, stress: int) -> int:
        return stress if use_stress_counts else normal

    return select
