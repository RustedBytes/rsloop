from __future__ import annotations

import rsloop
from picows import (  # pyright: ignore[reportMissingImports]
    WSCloseCode,
    WSFrame,
    WSListener,
    WSMsgType,
    WSTransport,
    ws_connect,
)


class ClientListener(WSListener):
    def on_ws_connected(self, transport: WSTransport) -> None:
        transport.send(WSMsgType.TEXT, b"Hello world")

    def on_ws_frame(self, transport: WSTransport, frame: WSFrame) -> None:
        print(f"Echo reply: {frame.get_payload_as_ascii_text()}")
        transport.send_close(WSCloseCode.OK)
        transport.disconnect()


async def main() -> None:
    transport, _ = await ws_connect(ClientListener, "ws://127.0.0.1:9001")
    await transport.wait_disconnected()


if __name__ == "__main__":
    rsloop.run(main())
