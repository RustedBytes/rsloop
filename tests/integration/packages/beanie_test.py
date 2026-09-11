from __future__ import annotations

import asyncio
import os
import uuid

import rsloop
from beanie import Document, init_beanie
from pymongo import AsyncMongoClient


class Message(Document):
    body: str


async def main() -> None:
    loop = asyncio.get_running_loop()
    loop_name = f"{type(loop).__module__}.{type(loop).__name__}"
    assert "rsloop" in loop_name, loop_name

    mongodb_url = os.environ.get("RSLOOP_MONGODB_URL", "mongodb://127.0.0.1:27017")
    database_name = f"rsloop_test_{uuid.uuid4().hex}"
    client = AsyncMongoClient(mongodb_url, serverSelectionTimeoutMS=5_000)
    initialized = False

    try:
        await init_beanie(
            database=client[database_name],
            document_models=[Message],
        )
        initialized = True
        await Message(body="hello").insert()
        await Message(body="from-beanie").insert()

        messages = await Message.find_all().sort("body").to_list()
        assert [message.body for message in messages] == [
            "from-beanie",
            "hello",
        ]
    finally:
        if initialized:
            await client.drop_database(database_name)
        await client.close()

    print("beanie ok")


if __name__ == "__main__":
    rsloop.run(main())
