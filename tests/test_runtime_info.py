"""Runtime diagnostics must describe a loop instance, not a build target."""

import asyncio

import rsloop


def test_runtime_info_lifecycle_and_independent_loops():
    first = rsloop.new_event_loop()
    second = rsloop.new_event_loop()
    try:
        assert first.runtime_info() == {"reactor": None}
        assert second.runtime_info() == {"reactor": None}

        async def snapshot():
            assert asyncio.get_running_loop() is first
            assert second.runtime_info() == {"reactor": None}
            return first.runtime_info()

        info = first.run_until_complete(snapshot())
        assert info["reactor"] in {"io_uring", "mio", "iocp", "kqueue"}
        assert first.runtime_info() == info
        # A caller cannot mutate the stored diagnostics through the dictionary.
        info["reactor"] = "changed"
        assert first.run_until_complete(snapshot())["reactor"] != "changed"
    finally:
        first.close()
        second.close()
    assert first.runtime_info() == {"reactor": None}
    assert second.runtime_info() == {"reactor": None}
