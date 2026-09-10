from __future__ import annotations

import asyncio
from typing import Any, cast

import rsloop
from sqlalchemy.ext.asyncio import async_sessionmaker, create_async_engine
from sqlmodel import Field, SQLModel, select


class Message(SQLModel, table=True):
    id: int | None = Field(default=None, primary_key=True)
    body: str


async def main() -> None:
    loop = asyncio.get_running_loop()
    loop_name = f"{type(loop).__module__}.{type(loop).__name__}"
    assert "rsloop" in loop_name, loop_name

    engine = create_async_engine("sqlite+aiosqlite:///:memory:")
    session_factory = async_sessionmaker(engine, expire_on_commit=False)

    try:
        async with engine.begin() as connection:
            await connection.run_sync(SQLModel.metadata.create_all)

        async with session_factory.begin() as session:
            session.add_all([Message(body="hello"), Message(body="from-sqlmodel")])

        async with session_factory() as session:
            result = await session.scalars(
                select(Message).order_by(cast(Any, Message.id))
            )
            assert [message.body for message in result] == [
                "hello",
                "from-sqlmodel",
            ]
    finally:
        await engine.dispose()

    print("sqlmodel ok")


if __name__ == "__main__":
    rsloop.run(main())
