"""Transaction ownership and pool recovery through SQLAlchemy and aiosqlite."""

import asyncio
from typing import Any, cast

import pytest

sqlalchemy = pytest.importorskip("sqlalchemy")
pytest.importorskip("aiosqlite")
sqlalchemy_asyncio = pytest.importorskip("sqlalchemy.ext.asyncio")
URL = sqlalchemy.URL
text = sqlalchemy.text
async_sessionmaker = sqlalchemy_asyncio.async_sessionmaker
create_async_engine = sqlalchemy_asyncio.create_async_engine

pytestmark = pytest.mark.ecosystem


@pytest.mark.parametrize("interruption", ["cancel", "timeout", "exception"])
def test_interrupted_transaction_rolls_back_and_returns_connection(
    loop, tmp_path, interruption
):
    async def main():
        engine = create_async_engine(
            URL.create("sqlite+aiosqlite", database=str(tmp_path / "transactions.db")),
            pool_size=1,
            max_overflow=0,
            pool_timeout=2,
        )
        sessions = async_sessionmaker(engine)
        inserted = asyncio.Event()
        blocked = asyncio.Event()

        async def transaction():
            async with sessions.begin() as session:
                await session.execute(
                    text("INSERT INTO messages VALUES (1, 'discard')")
                )
                inserted.set()
                if interruption == "exception":
                    raise ValueError("application failed after insert")
                await blocked.wait()

        task = None
        try:
            async with engine.begin() as connection:
                await connection.execute(
                    text("CREATE TABLE messages (id INTEGER PRIMARY KEY, body TEXT)")
                )
            task = asyncio.create_task(transaction())
            await inserted.wait()
            if interruption == "exception":
                with pytest.raises(ValueError, match="application failed"):
                    await task
            elif interruption == "cancel":
                task.cancel()
                with pytest.raises(asyncio.CancelledError):
                    await task
            else:
                with pytest.raises(asyncio.TimeoutError):
                    await asyncio.wait_for(task, 0)
            assert cast(Any, engine.pool).checkedout() == 0
            async with sessions.begin() as session:
                assert await session.scalar(text("SELECT count(*) FROM messages")) == 0
                await session.execute(text("INSERT INTO messages VALUES (2, 'commit')"))
            async with sessions() as session:
                result = await session.execute(text("SELECT id, body FROM messages"))
                assert result.all() == [(2, "commit")]
        finally:
            if task is not None:
                task.cancel()
                await asyncio.gather(task, return_exceptions=True)
            await asyncio.wait_for(engine.dispose(), 5)

    loop.run_until_complete(asyncio.wait_for(main(), 20))


def test_concurrent_sessions_stream_results_and_release_pool(loop, tmp_path):
    async def main():
        engine = create_async_engine(
            URL.create("sqlite+aiosqlite", database=str(tmp_path / "readers.db")),
            pool_size=2,
            max_overflow=0,
            pool_timeout=5,
        )
        sessions = async_sessionmaker(engine)
        tasks = []
        try:
            async with engine.begin() as connection:
                await connection.execute(text("CREATE TABLE numbers (value INTEGER)"))
                await connection.execute(
                    text("INSERT INTO numbers VALUES (:value)"),
                    [{"value": value} for value in range(128)],
                )

            async def read(offset):
                # Each concurrent task owns its own session and result stream.
                async with sessions() as session:
                    result = await session.stream(
                        text(
                            "SELECT value FROM numbers WHERE value >= :offset ORDER BY value"
                        ),
                        {"offset": offset},
                        execution_options={"yield_per": 7},
                    )
                    try:
                        values = [row[0] async for row in result]
                        assert values == list(range(offset, 128))
                    finally:
                        await result.close()
                return len(values)

            tasks = [asyncio.create_task(read(offset)) for offset in range(8)]
            assert await asyncio.gather(*tasks) == [128 - offset for offset in range(8)]
            assert cast(Any, engine.pool).checkedout() == 0
        finally:
            for task in tasks:
                task.cancel()
            await asyncio.gather(*tasks, return_exceptions=True)
            await asyncio.wait_for(engine.dispose(), 5)

    loop.run_until_complete(asyncio.wait_for(main(), 20))


@pytest.mark.parametrize("interruption", ["cancel", "timeout"])
def test_aiosqlite_cancel_inflight_thread_operation_then_reuse(loop, interruption):
    import threading

    aiosqlite = pytest.importorskip("aiosqlite")

    async def main():
        entered = asyncio.Event()
        release = threading.Event()

        def blocked_query():
            loop.call_soon_threadsafe(entered.set)
            if not release.wait(5):
                raise RuntimeError("test did not release the SQLite worker")
            return 7

        async with aiosqlite.connect(":memory:") as connection:
            await connection.create_function("blocked_query", 0, blocked_query)
            task = asyncio.ensure_future(connection.execute("SELECT blocked_query()"))
            try:
                await entered.wait()
                assert not task.done()
                if interruption == "cancel":
                    task.cancel()
                    with pytest.raises(asyncio.CancelledError):
                        await task
                else:
                    with pytest.raises(asyncio.TimeoutError):
                        await asyncio.wait_for(task, 0)
                release.set()
                # The cancelled operation completes on its worker first. Its
                # late result must not poison the next queued operation.
                async with connection.execute("SELECT 42") as cursor:
                    assert await cursor.fetchone() == (42,)
            finally:
                release.set()
                task.cancel()
                await asyncio.gather(task, return_exceptions=True)

    loop.run_until_complete(asyncio.wait_for(main(), 20))
