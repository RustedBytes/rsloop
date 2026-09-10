from __future__ import annotations

import asyncio
from typing import Any, cast

import rsloop
from mangum import Mangum


async def app(scope: dict[str, Any], receive: Any, send: Any) -> None:
    assert scope["type"] == "http"
    loop_name = f"{type(asyncio.get_running_loop()).__module__}"
    await send(
        {
            "type": "http.response.start",
            "status": 200,
            "headers": [(b"content-type", b"text/plain")],
        }
    )
    await send(
        {
            "type": "http.response.body",
            "body": f"mangum-{loop_name}".encode(),
        }
    )


class LambdaContext:
    function_name = "rsloop-smoke"
    function_version = "$LATEST"
    invoked_function_arn = "arn:aws:lambda:local:0:function:rsloop-smoke"
    memory_limit_in_mb = 128
    aws_request_id = "rsloop-request"
    log_group_name = "rsloop"
    log_stream_name = "smoke"

    def get_remaining_time_in_millis(self) -> int:
        return 30_000


EVENT = {
    "version": "2.0",
    "routeKey": "GET /",
    "rawPath": "/",
    "rawQueryString": "",
    "headers": {"host": "example.test"},
    "requestContext": {
        "http": {
            "method": "GET",
            "path": "/",
            "protocol": "HTTP/1.1",
            "sourceIp": "127.0.0.1",
        },
        "routeKey": "GET /",
        "stage": "$default",
        "time": "01/Jan/1970:00:00:00 +0000",
        "timeEpoch": 0,
    },
    "isBase64Encoded": False,
}


def main() -> None:
    rsloop.install()
    try:
        response = cast(Any, Mangum)(app, lifespan="off")(
            EVENT, cast(Any, LambdaContext())
        )
        assert response["statusCode"] == 200, response
        assert "mangum-rsloop" in response["body"], response
        print("mangum ok")
    finally:
        rsloop.uninstall()


if __name__ == "__main__":
    main()
