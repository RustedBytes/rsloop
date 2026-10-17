"""rsloop-perf: SQLAlchemy async pool churn under a Granian-shaped request driver.

usage: python pool_churn.py <asyncio|uvloop|rsloop> <dsn> [threads=8] [seconds=10] [queries=2]

A small engine (pool_size=4, max_overflow=2, pool_pre_ping=True, pool_timeout=10) on one
loop; N driver threads each post a request through loop.call_soon_threadsafe (as Granian's ASGI
scheduler does) and wait for its reply, closed-loop like oha at c=N. Prints requests/s, process
CPU per request and pool connects per 1k requests (a connect = a new Postgres session + SCRAM).
"""

import asyncio
import json
import os
import queue
import resource
import sys
import threading
import time

from sqlalchemy import event, text
from sqlalchemy.ext.asyncio import async_sessionmaker, create_async_engine

LOOP, DSN = sys.argv[1], sys.argv[2]
THREADS = int(sys.argv[3]) if len(sys.argv) > 3 else 8
SECONDS = float(sys.argv[4]) if len(sys.argv) > 4 else 10
QUERIES = int(sys.argv[5]) if len(sys.argv) > 5 else 2


def new_loop():
    if LOOP == "uvloop":
        import uvloop

        return uvloop.new_event_loop()
    if LOOP == "rsloop":
        import rsloop

        return rsloop.new_event_loop()
    return asyncio.new_event_loop()


def cpu():
    r = resource.getrusage(resource.RUSAGE_SELF)
    return r.ru_utime + r.ru_stime


STATS = {}


def trace(engine):
    """Count why the pool creates/discards: put into a full queue, get from an empty one."""
    from collections import Counter

    pool = engine.sync_engine.pool
    stats = STATS.setdefault("c", Counter())
    orig_ret, orig_create = pool._do_return_conn, pool._create_connection

    def ret(record):
        q = pool._pool._queue
        stats[
            f"return qsize={q.qsize()} getters={len(q._getters)} overflow={pool._overflow}"
        ] += 1
        return orig_ret(record)

    def create():
        q = pool._pool._queue
        stats[
            f"create qsize={q.qsize()} getters={len(q._getters)} overflow={pool._overflow}"
        ] += 1
        return orig_create()

    pool._do_return_conn, pool._create_connection = ret, create


def main():
    loop = new_loop()
    asyncio.set_event_loop(loop)
    engine = create_async_engine(
        DSN,
        pool_pre_ping=True,
        pool_size=int(os.environ.get("POOL_SIZE", "4")),
        max_overflow=int(os.environ.get("POOL_OVERFLOW", "2")),
        pool_timeout=10,
    )
    connects = [0]
    event.listen(
        engine.sync_engine,
        "connect",
        lambda *a: connects.__setitem__(0, connects[0] + 1),
    )
    sessions = async_sessionmaker(engine, expire_on_commit=False)
    if os.environ.get("CHURN_TRACE"):
        trace(engine)

    async def request(reply, posted):
        started = time.perf_counter()
        async with sessions() as session:
            for _ in range(QUERIES):
                await session.execute(text("select 1"))
        reply.put((posted, started, time.perf_counter()))

    def start(reply, posted):
        loop.create_task(request(reply, posted))

    stop = threading.Event()
    counts = [0] * THREADS
    measuring = threading.Event()

    phases = [0.0, 0.0, 0.0]

    def driver(i):
        reply = queue.SimpleQueue()
        while not stop.is_set():
            loop.call_soon_threadsafe(start, reply, time.perf_counter())
            posted, started, ended = reply.get()
            resumed = time.perf_counter()
            if measuring.is_set():
                counts[i] += 1
                phases[0] += started - posted
                phases[1] += ended - started
                phases[2] += resumed - ended

    async def run():
        await asyncio.sleep(2)  # warm-up
        c0, n0, t0 = cpu(), connects[0], time.perf_counter()
        measuring.set()
        await asyncio.sleep(SECONDS)
        measuring.clear()
        return cpu() - c0, connects[0] - n0, time.perf_counter() - t0

    threads = [
        threading.Thread(target=driver, args=(i,), daemon=True) for i in range(THREADS)
    ]
    for t in threads:
        t.start()
    cpu_s, conns, wall = loop.run_until_complete(run())
    stop.set()
    loop.run_until_complete(asyncio.sleep(0.5))
    loop.run_until_complete(engine.dispose())
    n = sum(counts)
    print(
        json.dumps(
            {
                "bench": f"pool_churn_t{THREADS}_q{QUERIES}",
                "loop": LOOP,
                "ops": n,
                "us_per_op": round(wall / n * 1e6, 1),
                "cpu_us_per_op": round(cpu_s / n * 1e6, 1),
                "connects_per_1k": round(conns / n * 1000, 2),
                "post_to_start_us": round(phases[0] / n * 1e6),
                "service_us": round(phases[1] / n * 1e6),
                "reply_to_resume_us": round(phases[2] / n * 1e6),
                "ctxsw_per_op": 0,
                "load1": round(os.getloadavg()[0], 2),
            }
        ),
        flush=True,
    )
    if STATS:
        for k, v in sorted(STATS["c"].items(), key=lambda kv: -kv[1])[:14]:
            print(f"   {v:7d} {k}", flush=True)
    os._exit(0)


main()
