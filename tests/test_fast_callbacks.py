from __future__ import annotations

import contextvars
import gc
import threading
import unittest
import weakref

import rsloop


class FastCallbackTests(unittest.TestCase):
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
            loop.call_soon(callback, 1, 2, 3)
            loop.call_soon(callback=callback)
            loop.call_soon(callback, context=contextvars.Context())
            loop.call_soon(callback, context=None)
            variable.set("changed")
            loop.call_soon(loop.stop)
            loop.run_forever()
            self.assertEqual(
                events,
                [
                    ((), "captured"),
                    ((None,), "captured"),
                    ((1, 2, 3), "captured"),
                    ((), "captured"),
                    ((), "default"),
                    ((), "captured"),
                ],
            )
        finally:
            loop.close()

    def test_invalid_argument_combinations(self):
        loop = rsloop.new_event_loop()
        try:
            for schedule in (loop.call_soon, loop.call_soon_threadsafe):
                with self.assertRaises(TypeError):
                    schedule()
                with self.assertRaises(TypeError):
                    schedule(context=None)
                with self.assertRaises(TypeError):
                    schedule(lambda: None, callback=lambda: None)
                with self.assertRaises(TypeError):
                    schedule(lambda: None, unsupported=True)
        finally:
            loop.close()

    def test_unbound_descriptor_rejects_wrong_receiver_and_accepts_subclass(self):
        descriptor = rsloop.Loop.call_soon
        with self.assertRaises(TypeError):
            descriptor(object(), lambda: None)

        class SubLoop(rsloop.Loop):
            pass

        loop = SubLoop()
        events = []
        try:
            descriptor(loop, events.append, "subclass")
            descriptor(loop, loop.stop)
            loop.run_forever()
            self.assertEqual(events, ["subclass"])
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
            self.assertIsNone(reference())
            for _ in range(3):
                for value in range(100):
                    loop.call_soon(events.append, value)
                loop.call_soon(loop.stop)
                loop.run_forever()
            self.assertEqual(events, list(range(100)) * 3)
            self.assertIsNone(reference())
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
            self.assertEqual(events, list(range(1000)))
        finally:
            thread.join()
            loop.close()
