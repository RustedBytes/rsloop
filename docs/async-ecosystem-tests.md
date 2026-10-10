# Async application lifecycle tests

The downstream suite exercises actual clients, protocols, SQLite worker threads,
and event loops. It extends the existing framework smoke scripts beyond a single
successful request. HTTP clients run against loopback servers; databases use
temporary SQLite files. No public endpoint, database service, or credentials are
required at test time.

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
tests/integration/packages. This suite does not establish PostgreSQL, Redis
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
