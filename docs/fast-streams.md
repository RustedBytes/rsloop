# Fast Streams

Importing `rsloop` patches `asyncio.open_connection()` and
`asyncio.start_server()`, including their `asyncio.streams` entry points.
On an `rsloop.Loop`, these helpers always use native fast streams for both
plaintext TCP and TLS. There is no stream-mode option or automatic stdlib
fallback on rsloop; the former `RSLOOP_USE_FAST_STREAMS` environment variable
has no effect. Connection and TLS errors propagate to the caller.

The helpers remain coroutine functions and select the running loop when
awaited, so they can be created before `run()` and passed to `create_task()`.
Calls running on other event loops continue to use those loops' standard
asyncio stream helpers.

The reader handed to your rsloop code is the native
`PyFastStreamReader` rather than `asyncio.StreamReader`. It implements the
reading surface protocols actually use:

- `read(n=-1)`, `readexactly(n)`
- `readline()`, `readuntil(separator=b"\n")`, including the tuple-of-separators
  form CPython 3.13+ accepts
- `at_eof()`, `exception()`, `feed_data()`, `feed_eof()`, `set_exception()`

The differential tests cover matching `asyncio.StreamReader` exception types and
attributes — `IncompleteReadError.partial`, `LimitOverrunError.consumed`, the
`ValueError` that `readline()` raises on limit overrun — and down to what is
left in the buffer afterwards. `tests/test_stream_reader.py` pins that behavior
by driving the same feed scripts through both readers and comparing the
results.

Native writers support `write`, `writelines`, `drain`, `close`, `wait_closed`,
`is_closing`, `get_extra_info`, `write_eof`, and `can_write_eof`. TLS transports
do not support half-close; TLS peer EOF closes the stream instead of leaving
its write side open. TLS negotiation continues to use rsloop's TLS transport.

The implementation lives in `src/transport/stream/fast.rs` and is backed by
the lower-level transport code in `src/transport/stream/mod.rs`.

See [Getting Started](getting-started.md#import-time-behavior) for the other
setup performed when `rsloop` is imported.

## Upgrading an existing connection to TLS

The native writer supports the Python 3.11+ stream API:

```python
await writer.start_tls(
    ssl_context,
    server_hostname="example.com",  # client side
    ssl_handshake_timeout=10,
    ssl_shutdown_timeout=5,
)
```

The method drains pending writes, upgrades the existing connection, and returns
`None`. It updates the writer, reader and protocol to the TLS transport. The
server side is inferred from the stream protocol; server callbacks pass their
server SSLContext without `server_hostname`. The callback is not restarted on
upgrade. After upgrade, `get_extra_info("sslcontext")` describes TLS and
`can_write_eof()` is false.

A failed/cancelled handshake closes the stream and completes its close waiters.
A blocking handshake may finish after cancellation; any late transport is
aborted. Server connection accounting is retained across the handoff and released
on handshake failure or eventual connection closure, so `server.wait_closed()`
can finish correctly.
