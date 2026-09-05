from __future__ import annotations

import asyncio
import tempfile
from pathlib import Path

import rsloop
from tortoise import Tortoise, fields
from tortoise.models import Model


class Message(Model):
    id = fields.IntField(primary_key=True)
    body = fields.CharField(max_length=100)


async def main() -> None:
    loop = asyncio.get_running_loop()
    loop_name = f"{type(loop).__module__}.{type(loop).__name__}"
    assert "rsloop" in loop_name, loop_name

    with tempfile.TemporaryDirectory() as directory:
        database_path = Path(directory) / "tortoise.sqlite"
        await Tortoise.init(
            db_url=f"sqlite://{database_path}",
            modules={"models": [__name__]},
        )
        try:
            await Tortoise.generate_schemas()
            await Message.create(body="hello")
            await Message.create(body="from-tortoise")

            bodies = await Message.all().order_by("id").values_list("body", flat=True)
            assert bodies == ["hello", "from-tortoise"]
        finally:
            await Tortoise.close_connections()

    print("tortoise-orm ok")


if __name__ == "__main__":
    rsloop.run(main())
