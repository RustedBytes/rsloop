from __future__ import annotations

import asyncio
import contextvars
import gc
import sys
import threading
import weakref
from typing import Any, cast

import pytest
import rsloop


class TestFastCallback:
    def test_loop_shell_preserves_subclass_state_and_reentrant_calls(self):
        class SubLoop(rsloop.Loop):
            events: list[bool]

        loop = SubLoop()
        reference = weakref.ref(loop)
        loop.events = []

        def callback(active_loop):
            active_loop.events.append(active_loop.get_debug())
            active_loop.set_debug(True)
            active_loop.call_soon(active_loop.events.append, active_loop.get_debug())
            active_loop.call_soon(active_loop.stop)

        try:
            loop.call_soon(callback, loop)
            loop.run_forever()
            assert loop.events == [False, True]
            assert reference() is loop
        finally:
            loop.close()
        del loop
        gc.collect()
        assert reference() is None

    @pytest.mark.parametrize("running", [False, True])
    @pytest.mark.parametrize("named", [False, True])
    @pytest.mark.parametrize("debug", [False, True])
    @pytest.mark.parametrize(
        "explicit_context",
        [
            False,
            pytest.param(
                True,
                marks=pytest.mark.skipif(
                    sys.version_info < (3, 11),
                    reason="asyncio.Task context requires Python 3.11+",
                ),
            ),
        ],
    )
    def test_task_constructor_option_layouts(
        self, running, named, debug, explicit_context
    ):
        variable = contextvars.ContextVar("task_option_probe", default="ambient")
        context = contextvars.Context()
        context.run(variable.set, "explicit")
        loop = rsloop.new_event_loop()
        loop.set_debug(debug)

        async def probe():
            return variable.get(), asyncio.get_running_loop()

        def create():
            options = {}
            if named:
                options["name"] = "named-task"
            if explicit_context:
                options["context"] = context
            task = loop.create_task(probe(), **options)
            assert task.get_loop() is loop
            if named:
                assert task.get_name() == "named-task"
            if debug:
                assert getattr(task, "_source_traceback", None)
            return task

        async def while_running():
            return await create()

        try:
            value, owning_loop = loop.run_until_complete(
                while_running() if running else create()
            )
            assert value == ("explicit" if explicit_context else "ambient")
            assert owning_loop is loop
        finally:
            loop.close()

    def test_arguments_keywords_and_context_capture(self):
        loop = rsloop.new_event_loop()
        events = []
        variable = contextvars.ContextVar("callback_probe", default="default")

        def callback(*args):
            events.append((args, variable.get()))

        try:
            variable.set("captured")
            loop.call_soon(callback)
            loop.call_soon(callback, None)
            loop.call_soon(callback, (1, 2))
            loop.call_soon(callback, 1, 2, 3)
            loop.call_soon(callback=callback)
            loop.call_soon(callback, context=contextvars.Context())
            loop.call_soon(callback, context=None)
            variable.set("changed")
            loop.call_soon(loop.stop)
            loop.run_forever()
            assert events == [
                ((), "captured"),
                ((None,), "captured"),
                (((1, 2),), "captured"),
                ((1, 2, 3), "captured"),
                ((), "captured"),
                ((), "default"),
                ((), "captured"),
            ]
        finally:
            loop.close()

    @pytest.mark.parametrize("running", [False, True])
    def test_invalid_argument_combinations(self, running):
        loop = rsloop.new_event_loop()

        def check():
            for schedule in (loop.call_soon, loop.call_soon_threadsafe):
                schedule = cast(Any, schedule)
                with pytest.raises(TypeError):
                    schedule()
                with pytest.raises(TypeError):
                    schedule(context=None)
                with pytest.raises(TypeError):
                    schedule(lambda: None, callback=lambda: None)
                with pytest.raises(TypeError):
                    schedule(lambda: None, unsupported=True)

        async def while_running():
            check()
            # An error in the fast entry must not leave a pending exception.
            future = loop.create_future()
            loop.call_soon(future.set_result, "ok")
            assert await future == "ok"

        try:
            if running:
                loop.run_until_complete(while_running())
            else:
                check()
        finally:
            loop.close()

    def test_unbound_descriptor_rejects_wrong_receiver_and_accepts_subclass(self):
        descriptor = cast(Any, rsloop.Loop.call_soon)
        with pytest.raises(TypeError):
            descriptor(object(), lambda: None)

        class SubLoop(rsloop.Loop):
            pass

        loop = SubLoop()
        events = []
        try:
            descriptor(loop, events.append, "subclass")
            descriptor(loop, loop.stop)
            loop.run_forever()
            assert events == ["subclass"]
        finally:
            loop.close()

    def test_recycled_handle_does_not_retain_weakrefs_or_cancellation(self):
        loop = rsloop.new_event_loop()
        events = []
        try:
            handle = loop.call_soon(events.append, "cancelled")
            reference = weakref.ref(handle)
            handle.cancel()
            del handle
            loop.call_soon(loop.stop)
            loop.run_forever()
            gc.collect()
            assert reference() is None
            for _ in range(3):
                for value in range(100):
                    loop.call_soon(events.append, value)
                loop.call_soon(loop.stop)
                loop.run_forever()
            assert events == list(range(100)) * 3
            assert reference() is None
        finally:
            loop.close()

    def test_threadsafe_descriptor_preserves_producer_order(self):
        loop = rsloop.new_event_loop()
        events = []

        def producer():
            for value in range(1000):
                loop.call_soon_threadsafe(events.append, value)
            loop.call_soon_threadsafe(loop.stop)

        thread = threading.Thread(target=producer)
        try:
            thread.start()
            loop.run_forever()
            thread.join()
            assert events == list(range(1000))
        finally:
            thread.join()
            loop.close()


@pytest.mark.parametrize("factory", [asyncio.new_event_loop, rsloop.new_event_loop])
@pytest.mark.parametrize("threadsafe", [False, True])
def test_empty_callback_contexts_keep_snapshots_tokens_and_explicit_identity(
    factory, threadsafe
):
    def scenario():
        loop = factory()
        variable = contextvars.ContextVar("empty_callback_probe", default="default")
        events = []
        tokens = []
        errors = []
        schedule = loop.call_soon_threadsafe if threadsafe else loop.call_soon
        shared = contextvars.Context()

        def first():
            events.append(variable.get())
            tokens.append(variable.set("first"))
            schedule(lambda: events.append(variable.get()))

        def sibling():
            events.append(variable.get())
            # A token retained by a completed callback still belongs to that
            # callback's private context, even after its handle is destroyed.
            with pytest.raises(ValueError):
                variable.reset(tokens[0])

        def fail():
            variable.set("failed")
            raise RuntimeError("context callback failure")

        try:
            loop.set_exception_handler(
                lambda _loop, info: errors.append(info["exception"])
            )
            schedule(first)
            schedule(sibling)
            schedule(variable.set, "shared", context=shared)
            schedule(lambda: events.append(variable.get()), context=shared)
            schedule(fail)
            schedule(lambda: events.append(variable.get()))
            cancelled = schedule(events.append, "cancelled")
            cancelled.cancel()
            variable.set("ambient")
            loop.run_until_complete(asyncio.sleep(0))
            assert events == ["default", "default", "shared", "default", "first"]
            assert variable.get() == "ambient"
            assert shared.get(variable) == "shared"
            assert len(errors) == 1
            assert isinstance(errors[0], RuntimeError)
        finally:
            loop.close()

    # Other tests and tracing frameworks can populate the ambient context.
    contextvars.Context().run(scenario)


@pytest.mark.parametrize("running", [False, True])
def test_handle_destruction_preserves_finalizers_and_weakrefs(running):
    loop = rsloop.new_event_loop()
    events = []
    refs = []

    class Callback:
        def __call__(self):
            events.append("called")

        def __del__(self):
            events.append("finalized")
            if running:
                loop.call_soon(events.append, "reentrant")

    def queue():
        callback = Callback()
        refs.append(weakref.ref(callback))
        handle = loop.call_soon(callback)
        refs.append(weakref.ref(handle, lambda _: events.append("weakref")))
        if not running:
            handle.cancel()
        return handle

    async def drive():
        queue()
        for _ in range(3):
            await asyncio.sleep(0)

    try:
        if running:
            loop.run_until_complete(drive())
            assert events == ["called", "finalized", "weakref", "reentrant"]
        else:
            handle = queue()
            loop.run_until_complete(asyncio.sleep(0))
            assert events == []
            # The queue released the cancelled handle, but this Python
            # reference keeps it alive until outside the lifecycle frame.
            del handle
            assert events == ["finalized", "weakref"]
        assert all(ref() is None for ref in refs)
    finally:
        loop.close()
