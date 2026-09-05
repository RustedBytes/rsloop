from __future__ import annotations

import asyncio
import tempfile
from pathlib import Path

import rsloop
from piccolo.columns import Varchar
from piccolo.engine.sqlite import SQLiteEngine
from piccolo.table import Table


async def main() -> None:
    loop = asyncio.get_running_loop()
    loop_name = f"{type(loop).__module__}.{type(loop).__name__}"
    assert "rsloop" in loop_name, loop_name

    with tempfile.TemporaryDirectory() as directory:
        database = SQLiteEngine(path=str(Path(directory) / "piccolo.sqlite"))

        class Message(Table, db=database):
            body = Varchar(length=100)

        await Message.create_table()
        await Message.insert(
            Message(body="hello"),
            Message(body="from-piccolo"),
        )

        rows = await Message.select(Message.body).order_by(Message.id)
        assert rows == [
            {"body": "hello"},
            {"body": "from-piccolo"},
        ]

    print("piccolo ok")


if __name__ == "__main__":
    rsloop.run(main())
