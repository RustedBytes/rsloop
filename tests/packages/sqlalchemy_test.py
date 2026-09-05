from __future__ import annotations

import asyncio

import rsloop
from sqlalchemy import String, select
from sqlalchemy.ext.asyncio import AsyncAttrs, async_sessionmaker, create_async_engine
from sqlalchemy.orm import DeclarativeBase, Mapped, mapped_column


class Base(AsyncAttrs, DeclarativeBase):
    pass


class Message(Base):
    __tablename__ = "message"

    id: Mapped[int] = mapped_column(primary_key=True)
    body: Mapped[str] = mapped_column(String(100))


async def main() -> None:
    loop = asyncio.get_running_loop()
    loop_name = f"{type(loop).__module__}.{type(loop).__name__}"
    assert "rsloop" in loop_name, loop_name

    engine = create_async_engine("sqlite+aiosqlite:///:memory:")
    session_factory = async_sessionmaker(engine, expire_on_commit=False)

    try:
        async with engine.begin() as connection:
            await connection.run_sync(Base.metadata.create_all)

        async with session_factory.begin() as session:
            session.add_all([Message(body="hello"), Message(body="from-sqlalchemy")])

        async with session_factory() as session:
            result = await session.scalars(select(Message).order_by(Message.id))
            assert [message.body for message in result] == [
                "hello",
                "from-sqlalchemy",
            ]
    finally:
        await engine.dispose()

    print("sqlalchemy ok")


if __name__ == "__main__":
    rsloop.run(main())
