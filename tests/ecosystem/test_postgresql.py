"""Real PostgreSQL contracts; opt in with a dedicated RSLOOP_POSTGRES_DSN."""

import asyncio
import os
import uuid
from contextlib import asynccontextmanager

import pytest

asyncpg = pytest.importorskip("asyncpg")
sa = pytest.importorskip("sqlalchemy")
sa_async = pytest.importorskip("sqlalchemy.ext.asyncio")
pytestmark = pytest.mark.ecosystem


@pytest.fixture
def postgres_dsn():
    dsn = os.environ.get("RSLOOP_POSTGRES_DSN")
    if not dsn:
        pytest.skip("set RSLOOP_POSTGRES_DSN to an isolated test database")
    return dsn


class Client:
    """Same observable contracts through each driver's public API."""

    def __init__(self, dsn, driver, size=1):
        self.dsn, self.driver, self.size = dsn, driver, size

    async def open(self):
        if self.driver == "asyncpg":
            self.pool = await asyncpg.create_pool(
                self.dsn, min_size=self.size, max_size=self.size
            )
        else:
            self.pool = sa_async.create_async_engine(
                self.dsn.replace("postgresql://", "postgresql+asyncpg://", 1),
                pool_size=self.size,
                max_overflow=0,
                pool_timeout=0.05,
            )
        return self

    @asynccontextmanager
    async def connection(self):
        if self.driver == "asyncpg":
            async with self.pool.acquire() as connection:
                yield connection
        else:
            async with self.pool.connect() as connection:
                yield connection

    async def scalar(self, connection, query):
        if self.driver == "asyncpg":
            return await connection.fetchval(query)
        return await connection.scalar(sa.text(query))

    def transaction(self, connection):
        if self.driver == "asyncpg":
            return connection.transaction()
        return connection.begin()

    async def close(self):
        if self.driver == "asyncpg":
            await asyncio.wait_for(self.pool.close(), 5)
        else:
            await asyncio.wait_for(self.pool.dispose(), 5)


async def wait_for_lock(observer, pid):
    # Observe the server state, not an assumed duration of query execution.
    while not await observer.fetchval(
        "SELECT EXISTS (SELECT FROM pg_stat_activity "
        "WHERE pid=$1 AND wait_event_type='Lock')",
        pid,
    ):
        await asyncio.sleep(0)


@pytest.mark.parametrize("driver", ["asyncpg", "sqlalchemy"])
@pytest.mark.parametrize("interruption", ["cancel", "timeout", "exception"])
def test_postgres_interrupted_transaction(loop, postgres_dsn, driver, interruption):
    async def main():
        client = await Client(postgres_dsn, driver).open()
        observer = await asyncpg.connect(postgres_dsn)
        table = "rsloop_" + uuid.uuid4().hex
        key = uuid.uuid4().int % (2**63)
        task = None
        pid_ready = asyncio.Future()
        try:
            await observer.execute(f"CREATE TABLE {table} (value integer)")
            await observer.execute(f"SELECT pg_advisory_lock({key})")

            async def transaction():
                async with (
                    client.connection() as connection,
                    client.transaction(connection),
                ):
                    await client.scalar(
                        connection,
                        f"INSERT INTO {table} VALUES (1) RETURNING value",
                    )
                    pid_ready.set_result(
                        await client.scalar(connection, "SELECT pg_backend_pid()")
                    )
                    if interruption == "exception":
                        raise ValueError("application failure")
                    await client.scalar(
                        connection, f"SELECT pg_advisory_xact_lock({key})"
                    )

            task = asyncio.create_task(transaction())
            pid = await pid_ready
            if interruption == "exception":
                with pytest.raises(ValueError, match="application failure"):
                    await task
            else:
                await wait_for_lock(observer, pid)
                if interruption == "cancel":
                    task.cancel()
                    with pytest.raises(asyncio.CancelledError):
                        await task
                else:
                    with pytest.raises(asyncio.TimeoutError):
                        await asyncio.wait_for(task, 0)
            assert await observer.fetchval(f"SELECT count(*) FROM {table}") == 0
            # Acquiring the only slot proves pool return. Query proves protocol recovery.
            async with (
                client.connection() as connection,
                client.transaction(connection),
            ):
                assert (
                    await client.scalar(
                        connection,
                        f"INSERT INTO {table} VALUES (2) RETURNING value",
                    )
                    == 2
                )
            assert await observer.fetchval(f"SELECT sum(value) FROM {table}") == 2
        finally:
            if task is not None:
                task.cancel()
                await asyncio.gather(task, return_exceptions=True)
            await observer.execute("SELECT pg_advisory_unlock_all()")
            await client.close()
            await observer.execute(f"DROP TABLE IF EXISTS {table}")
            await observer.close()

    loop.run_until_complete(asyncio.wait_for(main(), 20))


@pytest.mark.parametrize("driver", ["asyncpg", "sqlalchemy"])
def test_postgres_pool_exhaustion_and_reuse(loop, postgres_dsn, driver):
    async def main():
        client = await Client(postgres_dsn, driver).open()
        try:
            async with client.connection() as held:
                pid = await client.scalar(held, "SELECT pg_backend_pid()")
                if driver == "asyncpg":
                    with pytest.raises(asyncio.TimeoutError):
                        await client.pool.acquire(timeout=0.05)
                else:
                    with pytest.raises(sa.exc.TimeoutError):
                        async with client.connection():
                            pytest.fail("exhausted pool returned a connection")
                assert await client.scalar(held, "SELECT 42") == 42
            async with client.connection() as reused:
                assert await client.scalar(reused, "SELECT pg_backend_pid()") == pid
        finally:
            await client.close()

    loop.run_until_complete(asyncio.wait_for(main(), 20))


@pytest.mark.parametrize("driver", ["asyncpg", "sqlalchemy"])
def test_postgres_concurrent_transactions(loop, postgres_dsn, driver):
    async def main():
        client = await Client(postgres_dsn, driver, size=2).open()
        observer = await asyncpg.connect(postgres_dsn)
        table = "rsloop_" + uuid.uuid4().hex
        ready = [asyncio.Event(), asyncio.Event()]
        release = asyncio.Event()
        tasks = []
        try:
            await observer.execute(f"CREATE TABLE {table} (value integer)")

            async def writer(index):
                async with (
                    client.connection() as connection,
                    client.transaction(connection),
                ):
                    await client.scalar(
                        connection,
                        f"INSERT INTO {table} VALUES ({index}) RETURNING value",
                    )
                    ready[index].set()
                    await release.wait()

            tasks = [asyncio.create_task(writer(i)) for i in range(2)]
            await asyncio.gather(*(event.wait() for event in ready))
            assert await observer.fetchval(f"SELECT count(*) FROM {table}") == 0
            release.set()
            await asyncio.gather(*tasks)
            assert await observer.fetchval(
                f"SELECT array_agg(value ORDER BY value) FROM {table}"
            ) == [0, 1]
        finally:
            release.set()
            for task in tasks:
                task.cancel()
            await asyncio.gather(*tasks, return_exceptions=True)
            await client.close()
            await observer.execute(f"DROP TABLE IF EXISTS {table}")
            await observer.close()

    loop.run_until_complete(asyncio.wait_for(main(), 20))


@pytest.mark.parametrize("driver", ["asyncpg", "sqlalchemy"])
def test_postgres_tcp_disconnect_and_pool_recovery(loop, postgres_dsn, driver):
    async def main():
        client = await Client(postgres_dsn, driver).open()
        observer = await asyncpg.connect(postgres_dsn)
        key = uuid.uuid4().int % (2**63)
        task = None
        try:
            await observer.execute(f"SELECT pg_advisory_lock({key})")
            async with client.connection() as connection:
                pid = await client.scalar(connection, "SELECT pg_backend_pid()")
                task = asyncio.create_task(
                    client.scalar(connection, f"SELECT pg_advisory_xact_lock({key})")
                )
                await wait_for_lock(observer, pid)
                # Kill only our backend: the real server closes the TCP socket
                # while the client is receiving a response. No private transport API.
                assert await observer.fetchval("SELECT pg_terminate_backend($1)", pid)
                expected = (
                    asyncpg.PostgresError if driver == "asyncpg" else sa.exc.DBAPIError
                )
                with pytest.raises(expected):
                    await task
                if driver == "sqlalchemy":
                    await connection.rollback()
            async with client.connection() as replacement:
                assert (
                    await client.scalar(replacement, "SELECT pg_backend_pid()") != pid
                )
                assert await client.scalar(replacement, "SELECT 42") == 42
        finally:
            if task is not None:
                task.cancel()
                await asyncio.gather(task, return_exceptions=True)
            await observer.execute("SELECT pg_advisory_unlock_all()")
            await client.close()
            await observer.close()

    loop.run_until_complete(asyncio.wait_for(main(), 20))
