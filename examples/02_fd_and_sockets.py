import asyncio
import socket

import rsloop


async def demo_fd_readiness() -> None:
    loop = asyncio.get_running_loop()
    left, right = socket.socketpair()
    left.setblocking(False)
    right.setblocking(False)

    events: list[str] = []
    ready = loop.create_future()

    def on_writer() -> None:
        loop.remove_writer(left.fileno())
        right.send(b"ping")
        events.append("writer-ready")

    def on_reader() -> None:
        data = left.recv(4)
        loop.remove_reader(left.fileno())
        events.append(f"reader:{data.decode()}")
        if not ready.done():
            ready.set_result(None)

    loop.add_writer(left.fileno(), on_writer)
    loop.add_reader(left.fileno(), on_reader)
    await asyncio.wait_for(ready, 1.0)
    print("fd readiness:", events)

    left.close()
    right.close()


async def demo_raw_socket_helpers() -> None:
    loop = asyncio.get_running_loop()
    left, right = socket.socketpair()
    left.setblocking(False)
    right.setblocking(False)

    await loop.sock_sendall(left, b"abcdef")
    payload = await loop.sock_recv(right, 6)
    print("sock_sendall/sock_recv:", payload.decode())

    await loop.sock_sendall(left, b"xyz")
    buf = bytearray(3)
    count = await loop.sock_recv_into(right, buf)
    print("sock_recv_into:", bytes(buf[:count]).decode())

    left.close()
    right.close()


async def demo_accept_and_connect() -> None:
    loop = asyncio.get_running_loop()

    listener = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    listener.bind(("127.0.0.1", 0))
    listener.listen(1)
    listener.setblocking(False)

    host, port = listener.getsockname()[:2]
    client = socket.socket(socket.AF_INET, socket.SOCK_STREAM)
    client.setblocking(False)

    accept_future = loop.sock_accept(listener)
    await loop.sock_connect(client, (host, port))
    server_conn, peer = await asyncio.wait_for(accept_future, 1.0)

    await loop.sock_sendall(client, b"hello")
    received = await loop.sock_recv(server_conn, 5)
    print("sock_accept/sock_connect:", peer, received.decode())

    addrinfo = await loop.getaddrinfo("127.0.0.1", port)
    nameinfo = await loop.getnameinfo(listener.getsockname(), 0)
    print("getaddrinfo count:", len(addrinfo))
    print("getnameinfo:", nameinfo)

    client.close()
    server_conn.close()
    listener.close()


async def main() -> None:
    await demo_fd_readiness()
    await demo_raw_socket_helpers()
    await demo_accept_and_connect()


if __name__ == "__main__":
    rsloop.run(main())
