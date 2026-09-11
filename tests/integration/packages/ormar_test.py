from __future__ import annotations

import asyncio
import tempfile
from pathlib import Path

import ormar
import rsloop
import sqlalchemy


async def main() -> None:
    loop = asyncio.get_running_loop()
    loop_name = f"{type(loop).__module__}.{type(loop).__name__}"
    assert "rsloop" in loop_name, loop_name

    with tempfile.TemporaryDirectory() as directory:
        database_url = f"sqlite+aiosqlite:///{Path(directory) / 'ormar.sqlite'}"
        database = ormar.DatabaseConnection(database_url)
        metadata = sqlalchemy.MetaData()
        base_config = ormar.OrmarConfig(database=database, metadata=metadata)

        class Message(ormar.Model):
            ormar_config = base_config.copy(tablename="messages")

            id: int = ormar.Integer(primary_key=True)
            body: str = ormar.String(max_length=100)

        async with database:
            async with database.engine.begin() as connection:
                await connection.run_sync(metadata.create_all)

            await Message.objects.create(body="hello")
            await Message.objects.create(body="from-ormar")

            messages = await Message.objects.order_by("id").all()
            assert [message.body for message in messages] == [
                "hello",
                "from-ormar",
            ]

    print("ormar ok")


if __name__ == "__main__":
    rsloop.run(main())
