from __future__ import annotations

import asyncio

import rsloop
from picows import (  # pyright: ignore[reportMissingImports]
    WSFrame,
    WSListener,
    WSMsgType,
    WSTransport,
    WSUpgradeRequest,
    ws_create_server,
)


class ServerClientListener(WSListener):
    def on_ws_connected(self, transport: WSTransport) -> None:
        print("New client connected")

    def on_ws_frame(self, transport: WSTransport, frame: WSFrame) -> None:
        if frame.msg_type == WSMsgType.CLOSE:
            transport.send_close(frame.get_close_code(), frame.get_close_message())
            transport.disconnect()
        else:
            transport.send(frame.msg_type, frame.get_payload_as_memoryview())


async def main() -> None:
    def listener_factory(_request: WSUpgradeRequest) -> ServerClientListener:
        # Routing can be implemented here by analyzing request content
        return ServerClientListener()

    server: asyncio.Server = await ws_create_server(listener_factory, "127.0.0.1", 9001)
    for server_socket in server.sockets:
        print(f"Server started on {server_socket.getsockname()}")

    async with server:
        await server.serve_forever()


if __name__ == "__main__":
    rsloop.run(main())
