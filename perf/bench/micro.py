"""rsloop-perf microbenchmarks: isolate the costs suspected in a Granian-served service's c16 gap.

usage: python micro.py <asyncio|uvloop|rsloop> <bench> [scale]
Prints one JSON line: wall per op, process CPU per op (all threads), context switches per op.
"""

import asyncio
import json
import multiprocessing
import os
import queue
import resource
import socket
import sys
import threading
import time
from typing import cast

LOOP = sys.argv[1]
BENCH = sys.argv[2]
SCALE = float(sys.argv[3]) if len(sys.argv) > 3 else 1.0


def new_loop():
    if LOOP == "uvloop":
        import uvloop

        return uvloop.new_event_loop()
    if LOOP == "rsloop":
        import rsloop

        return rsloop.new_event_loop()
    return asyncio.new_event_loop()


def usage():
    r = resource.getrusage(resource.RUSAGE_SELF)
    return r.ru_utime + r.ru_stime, r.ru_nvcsw + r.ru_nivcsw


# ---------------------------------------------------------------- echo peer (separate process)
def _echo_server(port_q, delay_us):
    srv = socket.socket()
    srv.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
    srv.bind(("127.0.0.1", 0))
    srv.listen(256)
    port_q.put(srv.getsockname()[1])

    def serve(conn):
        conn.setsockopt(socket.IPPROTO_TCP, socket.TCP_NODELAY, 1)
        while True:
            data = conn.recv(65536)
            if not data:
                return
            if delay_us:
                time.sleep(delay_us / 1e6)
            conn.sendall(data)

    while True:
        conn, _ = srv.accept()
        threading.Thread(target=serve, args=(conn,), daemon=True).start()


def start_echo(delay_us=0):
    ctx = multiprocessing.get_context("fork")
    q = ctx.Queue()
    p = ctx.Process(target=_echo_server, args=(q, delay_us), daemon=True)
    p.start()
    return p, q.get()


class Client(asyncio.Protocol):
    def __init__(self, loop):
        self.loop = loop
        self.waiter = None
        self.buf = 0
        self.want = 0

    def connection_made(self, transport):
        self.transport = cast(asyncio.Transport, transport)

    def data_received(self, data):
        self.buf += len(data)
        if self.buf >= self.want and self.waiter is not None and not self.waiter.done():
            self.waiter.set_result(None)

    async def roundtrip(self, payload):
        self.buf, self.want = 0, len(payload)
        self.waiter = self.loop.create_future()
        self.transport.write(payload)
        await self.waiter


# ---------------------------------------------------------------- benches
def xthread_pingpong(loop):
    """Foreign thread posts call_soon_threadsafe and waits for the callback (Granian request start)."""
    n = int(20000 * SCALE)
    done = threading.Event()
    stop = loop.create_future()

    def cb():
        done.set()

    def producer():
        for _ in range(n):
            done.clear()
            loop.call_soon_threadsafe(cb)
            done.wait()
        loop.call_soon_threadsafe(stop.set_result, None)

    t = threading.Thread(target=producer)
    t.start()
    loop.run_until_complete(stop)
    t.join()
    return n


def xthread_task_pingpong(loop):
    """Granian ASGI shape: thread schedules a task start; the task answers through a thread-safe queue."""
    n = int(20000 * SCALE)
    replies = queue.SimpleQueue()
    stop = loop.create_future()

    async def handler(i):
        await asyncio.sleep(0)
        replies.put(i)

    def start(i):
        loop.create_task(handler(i))

    def producer():
        for i in range(n):
            loop.call_soon_threadsafe(start, i)
            replies.get()
        loop.call_soon_threadsafe(stop.set_result, None)

    t = threading.Thread(target=producer)
    t.start()
    loop.run_until_complete(stop)
    t.join()
    return n


def xthread_burst(loop):
    """4 foreign threads each post 50k callbacks back to back."""
    per, threads = int(50000 * SCALE), 4
    left = [per * threads]
    stop = loop.create_future()

    def cb():
        left[0] -= 1
        if left[0] == 0:
            stop.set_result(None)

    def producer():
        for _ in range(per):
            loop.call_soon_threadsafe(cb)

    ts = [threading.Thread(target=producer) for _ in range(threads)]
    for t in ts:
        t.start()
    loop.run_until_complete(stop)
    for t in ts:
        t.join()
    return per * threads


def soon_chain(loop):
    n = int(500000 * SCALE)
    left = [n]
    stop = loop.create_future()

    def cb():
        left[0] -= 1
        if left[0]:
            loop.call_soon(cb)
        else:
            stop.set_result(None)

    loop.call_soon(cb)
    loop.run_until_complete(stop)
    return n


def timers_cancel(loop):
    """asyncio.timeout around an already-resolved await: call_at + cancel per op (SQLAlchemy/asyncpg shape)."""
    n = int(200000 * SCALE)

    async def main():
        for _ in range(n):
            async with asyncio.timeout(30):
                await asyncio.sleep(0)

    loop.run_until_complete(main())
    return n


def resolve_name(loop):
    """loop.getaddrinfo() of a host name, one at a time (asyncpg/nats-py connect shape)."""
    n = int(5000 * SCALE)

    async def main():
        for _ in range(n):
            await loop.getaddrinfo("localhost", 4222, type=socket.SOCK_STREAM)

    loop.run_until_complete(main())
    return n


def resolve_name_c16(loop):
    """16 concurrent loop.getaddrinfo() calls of a host name."""
    n = int(5000 * SCALE)

    async def one():
        for _ in range(n // 16):
            await loop.getaddrinfo("localhost", 4222, type=socket.SOCK_STREAM)

    async def main():
        await asyncio.gather(*(one() for _ in range(16)))

    loop.run_until_complete(main())
    return n // 16 * 16


def resolve_name_busy(loop):
    """loop.getaddrinfo() of a host name while another task keeps the loop thread busy in Python
    (1 ms slices between awaits, like a loaded service). A resolver that needs the GIL on its
    worker waits for the loop thread to drop it."""
    n = int(1000 * SCALE)
    done = [False]

    async def busy():
        while not done[0]:
            end = time.perf_counter() + 0.001
            while time.perf_counter() < end:
                pass
            await asyncio.sleep(0)

    async def main():
        spinner = loop.create_task(busy())
        await asyncio.sleep(0)
        for _ in range(n):
            await loop.getaddrinfo("localhost", 4222, type=socket.SOCK_STREAM)
        done[0] = True
        await spinner

    loop.run_until_complete(main())
    return n


def executor_noop(loop):
    """loop.run_in_executor(None, ...) of a no-op, one at a time."""
    n = int(5000 * SCALE)

    async def main():
        for _ in range(n):
            await loop.run_in_executor(None, int)

    loop.run_until_complete(main())
    return n


def connect_by_name(loop):
    """asyncio.open_connection() by host name, one request/reply, close (a health probe's NATS check)."""
    proc, port = start_echo()
    n = int(2000 * SCALE)

    async def main():
        for _ in range(n):
            reader, writer = await asyncio.open_connection("localhost", port)
            writer.write(b"PING\r\n")
            await reader.readexactly(6)
            writer.close()
            await writer.wait_closed()

    try:
        loop.run_until_complete(main())
    finally:
        proc.kill()
    return n


def _tcp(loop, conns, rounds, size=64, delay_us=0):
    proc, port = start_echo(delay_us)
    payload = b"x" * size

    async def one():
        tr, pr = await loop.create_connection(lambda: Client(loop), "127.0.0.1", port)
        tr.get_extra_info("socket").setsockopt(
            socket.IPPROTO_TCP, socket.TCP_NODELAY, 1
        )
        for _ in range(rounds):
            await pr.roundtrip(payload)
        tr.close()

    async def main():
        await asyncio.gather(*(one() for _ in range(conns)))

    try:
        loop.run_until_complete(main())
    finally:
        proc.kill()
    return conns * rounds


def tcp_pingpong(loop):
    return _tcp(loop, 1, int(20000 * SCALE))


def tcp_pingpong_c8(loop):
    return _tcp(loop, 8, int(5000 * SCALE))


def granian_like(loop):
    """8 'requests' in flight: thread posts call_soon_threadsafe -> task does 2 DB round trips
    under asyncio.timeout -> replies through a thread-safe queue (c16 over 2 workers)."""
    proc, port = start_echo(200)
    n_threads, per = 8, int(1500 * SCALE)
    pool = []

    async def setup():
        for _ in range(n_threads):
            tr, pr = await loop.create_connection(
                lambda: Client(loop), "127.0.0.1", port
            )
            tr.get_extra_info("socket").setsockopt(
                socket.IPPROTO_TCP, socket.TCP_NODELAY, 1
            )
            pool.append(pr)

    loop.run_until_complete(setup())
    stop = loop.create_future()
    remaining = [n_threads]

    async def handler(conn, reply):
        for _ in range(2):
            async with asyncio.timeout(30):
                await conn.roundtrip(b"q" * 200)
        reply.put(None)

    def start(conn, reply):
        loop.create_task(handler(conn, reply))

    def finished():
        remaining[0] -= 1
        if not remaining[0]:
            stop.set_result(None)

    def producer(conn):
        reply = queue.SimpleQueue()
        for _ in range(per):
            loop.call_soon_threadsafe(start, conn, reply)
            reply.get()
        loop.call_soon_threadsafe(finished)

    ts = [threading.Thread(target=producer, args=(c,)) for c in pool]
    for t in ts:
        t.start()
    try:
        loop.run_until_complete(stop)
    finally:
        for t in ts:
            t.join()
        proc.kill()
    return n_threads * per


def main():
    loop = new_loop()
    asyncio.set_event_loop(loop)
    fn = globals()[BENCH]
    # warm-up at 5% scale is folded into the timing-free path
    cpu0, cs0 = usage()
    t0 = time.perf_counter()
    ops = fn(loop)
    wall = time.perf_counter() - t0
    cpu1, cs1 = usage()
    loop.close()
    print(
        json.dumps(
            {
                "bench": BENCH,
                "loop": LOOP,
                "ops": ops,
                "us_per_op": round(wall / ops * 1e6, 3),
                "cpu_us_per_op": round((cpu1 - cpu0) / ops * 1e6, 3),
                "ctxsw_per_op": round((cs1 - cs0) / ops, 3),
                "load1": round(os.getloadavg()[0], 2),
            }
        ),
        flush=True,
    )


if __name__ == "__main__":
    main()
