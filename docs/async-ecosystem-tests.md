# Async application lifecycle tests

The downstream suite exercises actual clients, protocols, SQLite worker threads,
and event loops. It extends the existing framework smoke scripts beyond a single
successful request. HTTP clients run against loopback servers; databases use
temporary SQLite files. The SQLite/HTTP suite requires no external service. PostgreSQL tests opt in
with `RSLOOP_POSTGRES_DSN` pointing to an isolated disposable database.

## Selected projects and contracts

The selection targets common integration surfaces in Python async applications:
HTTP pools, streaming protocols, cancellation scopes, database transactions,
and cross-thread result delivery. It builds on dependencies already used by the
repository's integration scripts and adds direct HTTPX coverage.

| Project | Application scenario | Assertions |
| --- | --- | --- |
| HTTPX / HTTPcore / AnyIO | Cancel or time out an HTTP/HTTPS download while the only pool connection is occupied | Stream closes; the same client can open a replacement connection and complete another request |
| HTTPX | Pool exhaustion while another response is held open | PoolTimeout; no extra server connection; completing the first response permits keep-alive reuse |
| aiohttp | Cancel or time out a request queued behind a limit-one connector | The queued request never reaches the server; the active response remains readable; the connection is reused |
| HTTPX and aiohttp | Streaming upload and response with Unicode bytes split across arbitrary chunks | Exact payload preservation in both directions |
| HTTPX and aiohttp | Upstream closes before Content-Length bytes arrive | Library-specific truncation error; a subsequent request succeeds |
| websockets | Cancel recv while a fragmented message is incomplete, with and without compression | The next recv returns the entire message; subsequent binary messages remain ordered |
| websockets | Concurrent receivers and graceful close | Second receiver is rejected; the pending receiver observes normal closure |
| SQLAlchemy / aiosqlite | Cancel, time out, or raise after an insert in a transaction | Rollback removes the uncommitted row; the single pool slot is returned; the next transaction commits |
| SQLAlchemy / aiosqlite | Concurrent independent sessions stream rows through a two-connection pool | Complete ordered results; cursors and pool slots released |
| aiosqlite | Cancel a query while its SQLite worker is still executing | Late cross-thread completion does not corrupt the cancelled Future or the next query |

Contract sources:

- [HTTPX async streaming and closing](https://www.python-httpx.org/async/)
- [HTTPX timeouts](https://www.python-httpx.org/advanced/timeouts/)
- [aiohttp pool lifecycle](https://docs.aiohttp.org/en/stable/client_advanced.html)
- [aiohttp queue tracing](https://docs.aiohttp.org/en/stable/tracing_reference.html)
- [websockets recv cancellation and concurrency](https://websockets.readthedocs.io/en/stable/reference/asyncio/connection.html)
- [SQLAlchemy async sessions and connection disposal](https://docs.sqlalchemy.org/en/20/orm/extensions/asyncio.html)
- [aiosqlite worker-thread design](https://aiosqlite.omnilib.dev/en/stable/)

Existing Redis/hiredis and AnyIO CI jobs remain useful complements. Framework
smokes for FastAPI, Starlette, Uvicorn and other packages remain under
tests/integration/packages. This suite does not establish Redis
cluster, HTTP/2, proxy, or production load compatibility.

## Run the suite

From the repository root:

```bash
uv sync --locked --no-default-groups --group ecosystem --no-install-project
uv run --no-sync maturin develop --locked
uv run --no-sync python scripts/generate_test_tls_certs.py
uv run --no-sync python scripts/run_python_tests.py tests/ecosystem -m ecosystem
```

The ecosystem marker is excluded from the default core test run. Its dedicated
workflow installs the locked dependency group, requires all downstream imports,
prints dependency versions and runs the suite on Linux/Python 3.10 and 3.14,
macOS/Python 3.14, and Windows/Python 3.14. Every available platform compares
asyncio with rsloop; Unix also compares uvloop. Windows intentionally skips the
uvloop variants. Downstream native extension support is separate from rsloop's
core free-threaded matrix.

Optional HTTP/database modules skip collection when their dependencies are
absent in a core-only environment. The dedicated workflow's import check makes
missing dependencies a job failure rather than an apparently successful suite.

## Test design

Events establish the interrupted state before cancellation. aiohttp's public
queue trace confirms a request is waiting for a pool slot. A WebSocket ping/pong
round trip accompanies an incomplete fragmented message. A threading event holds
a real SQLite worker until its caller has been cancelled. Timeout tests hold the
resource until the timeout occurs; they do not rely on the scheduler beating a
sleep of a particular duration.

Network assertions operate on accumulated bytes, not TCP packet boundaries.
Handlers propagate their failures to the parent test. Sessions, responses,
cursors, sockets and engines are closed; task and loop-exception checks detect
orphaned work. Per-scenario deadlines bound waits.

Initial Linux validation on CPython 3.14.7 passed all 66 parameterized cases
against asyncio, rsloop and uvloop with the locked dependencies: aiohttp 3.14.3,
HTTPX 0.28.1, HTTPcore 1.0.9, AnyIO 4.14.2, websockets 17.0.1, SQLAlchemy 2.0.52,
aiosqlite 0.22.1 and uvloop 0.23.0. These cases did not reproduce a new rsloop
defect; no production implementation change was needed. See the PR checks for
platform-specific execution results.

## Greenlet re-entry and transport ownership regressions

`tests/ecosystem/test_greenlet_reentry.py` covers the two-run lifecycle used by
ASGI servers: start background tasks, then re-enter the loop from a deeper C
stack. It tests a minimal greenlet bridge and SQLAlchemy's `greenlet_spawn` at
depths 0, 2, 4, 8, 50 and 100. Each case runs in a subprocess with a deadline and
faulthandler, verifies progress by both contending tasks, and verifies cancellation
completes. No database is required. The active ready queue lives on the heap so
greenlet stack copying cannot invalidate its thread-local pointer.

`tests/test_transport_protocol_release.py` checks that closed TCP and subprocess
transports release protocols that retain their transport, including when
`connection_lost` raises. It also checks release of `StreamReaderProtocol` and its
cached reader. These tests use weak references and garbage collection rather than
RSS, which can vary with allocator behavior. Stream teardown clears the protocol,
cached bound methods, fast-reader references and saved context after delivering
`connection_lost`; `get_protocol()` then returns `None`. This breaks the closed
connection cycle without making native transports cyclic-GC types.

Run both regression groups with:

```sh
uv run --no-sync python scripts/run_python_tests.py \
  tests/test_transport_protocol_release.py tests/ecosystem/test_greenlet_reentry.py -m ''
```

These regressions reproduce the scheduling and ownership mechanisms reported in
[issue #115](https://github.com/RustedBytes/rsloop/issues/115) and
[issue #116](https://github.com/RustedBytes/rsloop/issues/116). They do not replace
end-to-end Granian/PostgreSQL load or shutdown testing.

## PostgreSQL contracts

`tests/ecosystem/test_postgresql.py` adds 36 cases: asyncpg and SQLAlchemy
AsyncEngine, each compared on asyncio, rsloop and uvloop. Transactions are
interrupted by cancellation, timeout or application exceptions; rollback is
verified from an independent connection before a new transaction commits.
Concurrent transactions remain invisible until both are released by an event.
A held limit-one pool guarantees exhaustion until acquisition times out;
backend PIDs prove subsequent connection reuse.

For in-flight interruption, an observer holds a PostgreSQL advisory lock and
polls `pg_stat_activity` until the target backend reports a lock wait. Thus
cancellation occurs during real database I/O, without arbitrary sleeps. The
20-second scenario deadline bounds server-state polling. Pool timeouts expire
while the sole slot is held; no scheduler speed assumption is required.

TCP recovery terminates only the test backend with `pg_terminate_backend`
while its query is blocked. This tests a real server-side socket close and
replacement connection, not packet loss, a TCP RST proxy, or a full server
restart. The test user must be able to inspect and terminate its own backends.
Unique table names and advisory keys isolate cases; tables, locks, connections,
pools and tasks are cleaned up. Use only a disposable test database.

```bash
export RSLOOP_POSTGRES_DSN=postgresql://rsloop:rsloop@127.0.0.1:5432/rsloop_test
uv run --no-sync python scripts/run_python_tests.py tests/ecosystem/test_postgresql.py -m ecosystem -ra
```

A separate Linux GitHub Actions job provisions PostgreSQL 16 with a health
check and runs Python 3.10/3.14 with all three loops. It requires imports and
records Python, platform, client and server versions. Existing SQLite and
HTTP/WebSocket OS jobs remain in place; without the DSN PostgreSQL cases skip.
PostgreSQL execution on macOS/Windows is not claimed.

Local validation for this addition: Linux, CPython 3.14.7, release rsloop build;
66 existing ecosystem cases passed, 36 PostgreSQL cases skipped (no runnable
PostgreSQL service; container UID restrictions prevent starting the downloaded
server). Ruff and Pyright passed for the added test module. Skips do not establish PostgreSQL compatibility.

[GitHub Actions run 38062646332](https://github.com/RustedBytes/rsloop/actions/runs/38062646332)
executed the added PostgreSQL suite at code commit `40a9ad8`:

| Actual environment | Result |
| --- | --- |
| Linux x86_64, CPython 3.10.22, PostgreSQL 16.15 | 36 passed (asyncio, rsloop, uvloop) |
| Linux x86_64, CPython 3.14.8, PostgreSQL 16.15 | 36 passed (asyncio, rsloop, uvloop) |

Both jobs used asyncpg 0.32.0, SQLAlchemy 2.0.52 and uvloop 0.23.0.
These scenarios reproduced no rsloop implementation defect; no production-code
change was needed. PostgreSQL ran in CI, not locally. This is lifecycle
compatibility evidence under the listed conditions, not a production-load test.

Primary contracts: [asyncpg pools and transactions](https://magicstack.github.io/asyncpg/current/api/index.html),
[SQLAlchemy asyncio](https://docs.sqlalchemy.org/en/20/orm/extensions/asyncio.html),
[PostgreSQL activity monitoring](https://www.postgresql.org/docs/16/monitoring-stats.html),
[backend termination](https://www.postgresql.org/docs/16/functions-admin.html).
