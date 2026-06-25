from __future__ import annotations

import asyncio

import rsloop
from django.conf import settings
from django.http import JsonResponse
from django.urls import path

from _smoke import run_uvicorn_app


async def index(request: object) -> JsonResponse:
    loop = asyncio.get_running_loop()
    return JsonResponse(
        {
            "ok": "django-rsloop",
            "loop": f"{type(loop).__module__}.{type(loop).__name__}",
        }
    )


urlpatterns = [path("", index)]


def configure_django() -> None:
    if not settings.configured:
        settings.configure(
            SECRET_KEY="rsloop-test",
            ROOT_URLCONF=__name__,
            ALLOWED_HOSTS=["127.0.0.1", "localhost"],
            INSTALLED_APPS=[],
            MIDDLEWARE=[],
        )

    import django

    django.setup()


async def main() -> None:
    configure_django()

    from django.core.asgi import get_asgi_application

    app = get_asgi_application()
    await run_uvicorn_app(
        app,
        b"django-rsloop",
        name="django-asgi",
        lifespan="off",
    )


if __name__ == "__main__":
    rsloop.run(main())
