"""rsloop-perf: opt-in (RSPERF_TRACE_POOL=1) SQLAlchemy pool trace inside the service workers.

Counts every check-in and connection creation by (queue size, waiting getters, overflow) and
why connections close, printing the counts to stderr every 15 s as one "rsperf-pool" line.
"""

import os

if os.environ.get("RSPERF_TRACE_POOL") == "1":
    import atexit
    import json
    import sys
    import threading
    import time
    from collections import Counter

    _stats = Counter()

    def _install():
        from sqlalchemy.pool import QueuePool

        orig_ret, orig_create = QueuePool._do_return_conn, QueuePool._create_connection

        def _state(pool):
            q = getattr(pool._pool, "_queue", None)
            getters = len(getattr(q, "_getters", ())) if q is not None else -1
            return f"qsize={pool._pool.qsize()}/{pool._pool.maxsize} getters={getters} overflow={pool._overflow}"

        def ret(self, record):
            _stats["return " + _state(self)] += 1
            return orig_ret(self, record)

        def create(self):
            _stats["create " + _state(self)] += 1
            return orig_create(self)

        QueuePool._do_return_conn, QueuePool._create_connection = ret, create

        from sqlalchemy import event
        from sqlalchemy.pool import Pool

        event.listen(Pool, "invalidate", lambda *a: _stats.update(["event invalidate"]))
        event.listen(
            Pool, "soft_invalidate", lambda *a: _stats.update(["event soft_invalidate"])
        )
        event.listen(Pool, "close", lambda *a: _stats.update(["event close"]))
        event.listen(
            Pool, "close_detached", lambda *a: _stats.update(["event close_detached"])
        )

    def _dump():
        top = dict(_stats)
        print(
            "rsperf-pool "
            + json.dumps({"pid": os.getpid(), "t": round(time.time()), "counts": top}),
            file=sys.stderr,
            flush=True,
        )

    def _loop():
        while True:
            time.sleep(15)
            _dump()

    try:
        _install()
        threading.Thread(target=_loop, daemon=True).start()
        atexit.register(_dump)
    except Exception as exc:  # noqa: BLE001 - never break the service
        print(f"rsperf-pool trace not installed: {exc!r}", file=sys.stderr, flush=True)
