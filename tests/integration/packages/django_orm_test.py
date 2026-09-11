from __future__ import annotations

import asyncio
import tempfile
from pathlib import Path
from typing import Any, cast

import rsloop
from asgiref.sync import sync_to_async
from django.conf import settings
from django.db import connection, models


def configure_django(database_path: Path) -> type[models.Model]:
    settings.configure(
        SECRET_KEY="rsloop-test",
        DATABASES={
            "default": {
                "ENGINE": "django.db.backends.sqlite3",
                "NAME": database_path,
            }
        },
        INSTALLED_APPS=[],
    )

    import django

    django.setup()

    class Message(models.Model):
        body = models.CharField(max_length=100)

        class Meta:
            app_label = "rsloop_test"

    return Message


async def main() -> None:
    loop = asyncio.get_running_loop()
    loop_name = f"{type(loop).__module__}.{type(loop).__name__}"
    assert "rsloop" in loop_name, loop_name

    with tempfile.TemporaryDirectory() as directory:
        message = cast(Any, configure_django(Path(directory) / "django.sqlite"))

        def create_table() -> None:
            with connection.schema_editor() as editor:
                editor.create_model(message)

        await sync_to_async(create_table, thread_sensitive=True)()
        await message.objects.acreate(body="hello")
        await message.objects.acreate(body="from-django")

        bodies = [
            body
            async for body in message.objects.order_by("id").values_list(
                "body", flat=True
            )
        ]
        assert bodies == ["hello", "from-django"]

    print("django-orm ok")


if __name__ == "__main__":
    rsloop.run(main())
