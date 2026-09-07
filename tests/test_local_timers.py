from __future__ import annotations

import asyncio

import rsloop


class TestLocalTimer:
    def test_timer_scheduled_by_stopping_callback_survives_next_run(self):
        loop = rsloop.new_event_loop()
        events = []

        def second():
            events.append("second")
            loop.stop()

        def first():
            events.append("first")
            loop.call_later(0, second)
            loop.stop()

        try:
            loop.call_later(0, first)
            loop.run_forever()
            assert events == ["first"]
            loop.run_forever()
            assert events == ["first", "second"]
        finally:
            loop.close()

    def test_cancelled_timer_finalizer_can_schedule_another_timer(self):
        loop = rsloop.new_event_loop()
        events = []

        def finish():
            events.append("finished")
            loop.stop()

        class Callback:
            def __call__(self):
                events.append("unexpected")

            def __del__(self):
                events.append("released")
                loop.call_later(0, finish)

        try:
            handle = loop.call_later(0, Callback())
            handle.cancel()
            watchdog = loop.call_later(5, loop.stop)
            loop.run_forever()
            watchdog.cancel()
            assert events == ["released", "finished"]
        finally:
            loop.close()

    def test_close_releases_future_timer_and_rejects_new_timers(self):
        loop = rsloop.new_event_loop()
        events = []

        class Callback:
            def __call__(self):
                events.append("unexpected")

            def __del__(self):
                events.append("released")
                try:
                    loop.call_later(0, lambda: None)
                except RuntimeError:
                    events.append("closed")

        loop.call_later(3600, Callback())
        loop.close()
        assert events == ["released", "closed"]

    def test_positive_timers_progress_with_a_busy_ready_queue(self):
        async def main():
            loop = asyncio.get_running_loop()
            done = loop.create_future()
            loop.call_later(0.005, done.set_result, "expired")
            spins = 0
            deadline = loop.time() + 5
            while not done.done() and loop.time() < deadline:
                spins += 1
                await asyncio.sleep(0)
            assert done.done(), "timer starved behind ready callbacks"
            assert spins > 0
            assert done.result() == "expired"

        rsloop.run(main())
