import asyncio
import threading

import rsloop


def demo_run_forever() -> None:
    loop = rsloop.new_event_loop()
    events: list[str] = []

    def record_soon(label: str) -> None:
        events.append(f"call_soon:{label}")

    def stop_later() -> None:
        events.append("call_later:stop")
        loop.stop()

    loop.call_soon(record_soon, "alpha")
    loop.call_later(0.05, stop_later)
    loop.run_forever()
    print("run_forever events:", events)
    loop.close()


async def demo_async_features() -> None:
    loop = asyncio.get_running_loop()

    future = loop.create_future()
    loop.call_soon(future.set_result, "future-ready")

    threadsafe_future = loop.create_future()

    def signal_from_thread() -> None:
        loop.call_soon_threadsafe(threadsafe_future.set_result, "threadsafe-ready")

    worker = threading.Thread(target=signal_from_thread, name="example-threadsafe")
    worker.start()

    async def child_task() -> str:
        await asyncio.sleep(0.01)
        return "task-ready"

    task = loop.create_task(child_task(), name="demo-task")
    executor_value = await loop.run_in_executor(None, sum, [1, 2, 3, 4])

    print("create_future:", await future)
    print("call_soon_threadsafe:", await threadsafe_future)
    print("create_task:", await task)
    print("run_in_executor:", executor_value)

    worker.join()
    await loop.shutdown_asyncgens()
    await loop.shutdown_default_executor()


if __name__ == "__main__":
    demo_run_forever()
    rsloop.run(demo_async_features())
