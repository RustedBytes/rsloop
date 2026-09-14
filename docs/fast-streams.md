# Fast Streams

Importing `rsloop` patches `asyncio.open_connection()` and
`asyncio.start_server()` by default.

That import-time behavior is controlled by `RSLOOP_USE_FAST_STREAMS` and can be
disabled with:

```bash
export RSLOOP_USE_FAST_STREAMS=0
```

The native fast-stream path is used only when:

- the running loop is an `rsloop.Loop`
- `ssl` is unset or `None`

Otherwise `rsloop` falls back to the standard-library `asyncio.streams`
helpers.

On that path the reader handed to your code is the native
`PyFastStreamReader` rather than `asyncio.StreamReader`. It implements the
reading surface protocols actually use:

- `read(n=-1)`, `readexactly(n)`
- `readline()`, `readuntil(separator=b"\n")`, including the tuple-of-separators
  form CPython 3.13+ accepts
- `at_eof()`, `exception()`, `feed_data()`, `feed_eof()`, `set_exception()`

These match `asyncio.StreamReader` down to the exception types and their
attributes — `IncompleteReadError.partial`, `LimitOverrunError.consumed`, the
`ValueError` that `readline()` raises on limit overrun — and down to what is
left in the buffer afterwards. `tests/test_stream_reader.py` pins that behavior
by driving the same feed scripts through both readers and comparing the
results.

The implementation lives in `src/transport/stream/fast.rs` and is backed by
the lower-level transport code in `src/transport/stream/mod.rs`.

See [Getting Started](getting-started.md#import-time-behavior) for the other
setup performed when `rsloop` is imported.
