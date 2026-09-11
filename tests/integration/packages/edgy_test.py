from __future__ import annotations

import asyncio
import tempfile
from pathlib import Path

import edgy
import rsloop


async def main() -> None:
    loop = asyncio.get_running_loop()
    loop_name = f"{type(loop).__module__}.{type(loop).__name__}"
    assert "rsloop" in loop_name, loop_name

    with tempfile.TemporaryDirectory() as directory:
        database_url = f"sqlite:///{Path(directory) / 'edgy.sqlite'}"
        models_registry = edgy.Registry(database=database_url)

        class Message(edgy.Model):
            body = edgy.CharField(max_length=100)

            class Meta:  # pyright: ignore[reportIncompatibleVariableOverride]
                registry = models_registry

        async with models_registry:
            await models_registry.create_all()
            await Message.query.create(body="hello")
            await Message.query.create(body="from-edgy")

            messages = await Message.query.order_by("id").all()
            assert [message.body for message in messages] == [
                "hello",
                "from-edgy",
            ]

    print("edgy ok")


if __name__ == "__main__":
    rsloop.run(main())
