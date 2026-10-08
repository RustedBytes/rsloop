# Python API reference

This page describes the package exported by `python/rsloop/__init__.py` at
source revision `e42b8132730c70b1f1d79eaf1403eb3c3c7b66cf` (version 0.1.60).
See [Verified Surface Area](supported-features.md) for loop methods and
[Fast Streams](fast-streams.md) for reader and writer interfaces.

## Entry points

| API | Contract |
| --- | --- |
| `run(main, *, loop_factory=new_event_loop, debug=None)` | Runs an async entry point, returning its result and propagating its exception. The factory must return an `rsloop.Loop`. `debug` is a boolean or `None`, passed to the runner. |
| `new_event_loop() -> Loop` | Creates a stopped loop. Does not make it the current loop. The caller owns its cleanup. |
| `Loop()` | Native loop class, also exposed as `rsloop._loop.PyLoop`; prefer the public name. |
| `EventLoopPolicy()` | An asyncio default-policy subclass whose `new_event_loop()` creates rsloop loops. |
| `install() -> None` | Saves the active policy and installs `EventLoopPolicy`. Repeated calls while that policy is active do nothing. |
| `uninstall() -> None` | Restores the saved policy only if rsloop's policy is still active; otherwise preserves the replacement policy. Clears the saved policy. Without a saved policy, does nothing. |
| `__version__: str` | Native package version, derived from `Cargo.toml`. |

`run()` cannot be nested inside a running event loop: it raises `RuntimeError`.
A factory that returns another loop implementation leads to `TypeError` when
the wrapper runs. The typed contract accepts a coroutine. On Python 3.10–3.11,
non-coroutine input raises `ValueError`; on 3.12+, the wrapper awaits `main`
and delegates runner management to `asyncio.run()`.

On exit, the runner cancels remaining tasks, shuts down asynchronous generators
and the default executor, and closes the loop. User resources such as writers
and subprocesses still need explicit cleanup. On 3.12+, additional runtime
keyword arguments are forwarded to `asyncio.run()`; they are version-dependent
and are not part of rsloop's typed interface.

`install()` changes process-wide policy state. Prefer `run()` when selecting a
loop for one entry point. Import-time compatibility patches remain installed
after `uninstall()`; it only restores policy state.

## Explicit lifecycle

```python
import asyncio
import rsloop


async def main() -> str:
    return "done"


loop = rsloop.new_event_loop()
asyncio.set_event_loop(loop)
try:
    print(loop.run_until_complete(main()))
finally:
    loop.run_until_complete(loop.shutdown_asyncgens())
    loop.run_until_complete(loop.shutdown_default_executor())
    asyncio.set_event_loop(None)
    loop.close()
```

This example creates no background tasks. If your manual lifecycle creates
additional tasks, cancel and await them before shutting down generators and
the executor. `close()` is not a substitute for completing asynchronous cleanup.
Callbacks and Python tasks run on the thread driving the loop. Use
`call_soon_threadsafe(callback, *args)` to submit callbacks from other threads;
see [Free-Threaded CPython](free-threading.md) before using multiple loops.

## Build diagnostics

`build_info() -> dict[str, str | bool]` returns a new dictionary:

| Keys | Meaning |
| --- | --- |
| `version`, `profile` | Package version and `debug` or `release` build. |
| `target_os`, `target_arch`, `minimum_os` | Compile-time platform and declared runtime minimum. |
| `free_threaded` | Whether the extension was built for a free-threaded interpreter; does not report the current process GIL state. |
| `reactor`, `runtime_profile`, `tls_backend` | Target reactor label, `rsloop` runtime profile, and `rustls`. Individual operations may use helper threads. |
| `hotpath_profile`, `hotpath_alloc_profile` | Whether the corresponding Cargo profiling features were compiled in. |

```python
import rsloop

print(rsloop.__version__)
print(rsloop.build_info())
```

## Transport diagnostics

`transport_stats() -> dict[str, int | bool]` returns a snapshot of native
extension-wide counters, shared by loops using that extension:

| Key | Meaning |
| --- | --- |
| `enabled` | Whether transport instrumentation is enabled. |
| `read_events`, `read_bytes` | Counted read events and payload bytes. |
| `read_wakeups`, `python_read_drains` | Reader wakeups and Python-thread drains. |
| `staged_writes`, `direct_write_attempts` | Staged write operations and direct write attempts. |
| `poll_rebinds` | Completion-to-poll transport rebindings. |

Set `RSLOOP_TRANSPORT_STATS=1` before import; see
[Configuration](configuration.md). `reset_transport_stats() -> None` resets
all numeric counters without changing whether collection is enabled.
Counters use independent relaxed atomic reads and writes: snapshots and resets
are not a transactional measurement boundary while I/O is active. These are
implementation counters, not application request counts or latency measurements.

## Compatibility boundaries

Stream helpers return native reader and writer objects, so use their documented
methods rather than relying on `isinstance(reader, asyncio.StreamReader)`.
`server.sockets` is a tuple-valued property, not a method. Closing a server
stops listening; close client writers separately and await their `wait_closed()`.
`server.wait_closed()` waits for the closed state, accept tasks, active
connections, and pending TLS handshakes. It can keep waiting if a client
connection remains open.
TLS uses rustls and does not implement every OpenSSL `SSLContext` behavior.
Unix sockets and Unix signal handlers require Unix; `preexec_fn` is unsupported.
See [How It Works](how-it-works.md#current-limitations) for the remaining limits.
