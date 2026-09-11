from __future__ import annotations

import rsloop
from faststream import FastStream, TestApp
from faststream.nats import NatsBroker, TestNatsBroker


broker = NatsBroker()
app = FastStream(broker)
received: list[str] = []


@broker.subscriber("rsloop.in")
async def handle(message: str) -> str:
    received.append(message)
    return f"echo:{message}"


async def main() -> None:
    async with TestNatsBroker(broker, connect_only=True):
        async with TestApp(app):
            response = await broker.request("hello", "rsloop.in")

    assert response.body == b"echo:hello", response.body
    assert received == ["hello"], received
    print("faststream ok")


if __name__ == "__main__":
    rsloop.run(main())
