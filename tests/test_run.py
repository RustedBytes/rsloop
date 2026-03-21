import asyncio
import unittest

import rsloop


class RunTests(unittest.TestCase):
    def test_set_event_loop_accepts_rsloop_loop(self) -> None:
        loop = rsloop.new_event_loop()
        try:
            asyncio.set_event_loop(loop)
            self.assertIs(asyncio.get_event_loop(), loop)
        finally:
            asyncio.set_event_loop(None)
            loop.close()

    def test_run_executes_coroutine(self) -> None:
        async def main() -> str:
            return "ok"

        self.assertEqual(rsloop.run(main()), "ok")


if __name__ == "__main__":
    unittest.main()
