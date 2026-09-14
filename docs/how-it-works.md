# How It Works

This page explains the architecture without going too deep into Rust details.

## The short version

`rsloop` is a hybrid project:

- Python gives the package interface that users import
- Rust implements the core event loop and transport machinery
- PyO3 connects both sides

## Request flow

A simple mental model is:

1. Your Python code calls `rsloop.run(...)` or uses `rsloop.Loop`.
2. The Python wrapper creates or manages a native loop object.
3. The Rust extension schedules timers, callbacks, I/O, and transport work.
4. Your Python callbacks and coroutines still run as Python code.

So the project is not "Python replaced by Rust". It is "Python application code on top of a Rust event loop".

## The Python layer

The Python package lives in `python/rsloop/`.

Important files:

- `__init__.py`: exports the public API
- `_run.py`: defines `run(...)`, `new_event_loop()`, and the installable event
  loop policy
- `_loop_compat.py`: compatibility helpers and monkeypatches
- `_bootstrap.py`: startup helpers, including Windows DLL and SSL-related setup

This layer is a thin adapter. It keeps the user-facing API pleasant while the heavy lifting happens in Rust.

## The Rust layer

The Rust code lives in `src/`.

Important files:

- `lib.rs`: extension module entry point
- `bindings/loop_api.rs`: exposes Rust functionality as Python classes and functions
- `bindings/loop_api/methods.rs`: the single `#[pymethods]` block listing every loop method Python can call
- `bindings/loop_api/`: one module per group of loop methods (`servers.rs`, `connections.rs`, `tasks.rs`, `process_spawn.rs`, and so on)
- `engine/loop_core.rs`: core loop state and loop-thread execution
- `engine/commands.rs`: commands shared by the loop and runtime dispatcher
- `engine/dispatcher.rs`: coordination-thread runtime work
- `engine/callbacks.rs`: callback handles and scheduling helpers
- `transport/stream/mod.rs`: the transport and server core types every stream module shares
- `transport/stream/`: one module per concern (`socket_transport.rs`, `tls_transport.rs`, `accept.rs`, `reader.rs`, `writer.rs`, `fast.rs`, and so on)
- `transport/process/mod.rs`: the subprocess core type and the messages its threads exchange
- `transport/process/`: one module per concern (`spawn.rs`, `worker.rs`, `core_protocol.rs`, and so on)
- `transport/tls/`: TLS configuration and certificate material
- `platform/fd/`: lower-level cross-platform descriptor work
- `context.rs`: running-loop and context management helpers
- `errors.rs`: shared error types
- `rust_async.rs`: public Rust/Python async interop helpers for downstream extensions
- `async_event.rs`, `blocking.rs`, `python_names.rs`: support code used by the public pieces
- `platform/windows_vibeio.rs`: Windows-specific runtime support

You do not need to understand every file before using the project. For a first
pass, `lib.rs`, `bindings/loop_api.rs`, and `engine/loop_core.rs` are the most
useful entry points.

## Runtime model

Each loop combines a coordination runtime with a loop-thread I/O runtime:

- the coordination thread handles commands, timers, and cross-thread work
- on Unix, generic TCP protocol readers run on the Python loop thread;
  native fast streams and Unix-domain readers retain coordination-thread I/O
- non-TLS accept loops use `vibeio` on the thread that starts them
- bounded ready-callback turns service loop-thread I/O even when Python tasks
  continually yield with `sleep(0)`
- Windows TCP transports, including custom `asyncio.Protocol` implementations,
  start in IOCP completion mode and rebind to readiness mode before `start_tls`
  synchronously reclaims a socket
- generic `add_reader` / `add_writer` descriptors use cancellable OS-poll
  workers because `vibeio` does not expose arbitrary raw-descriptor registration
- some transport paths still fall back to helper threads, especially TLS I/O,
  TLS server accept, and parts of the legacy transport write path

Python tasks and callbacks still execute on the Python side. The runtime
dependency is now unified, but the codebase has not finished eliminating every
helper thread yet.

Transport overload safeguards use conservative defaults: inbound reads pause
at 1 MiB of pending data per connection and resume below 256 KiB, buffered
writes are capped at 64 MiB, and a TLS server admits at most 256 simultaneous
handshakes. The last two limits can be adjusted before importing `rsloop` with
`RSLOOP_MAX_WRITE_BUFFER_BYTES` and `RSLOOP_MAX_PENDING_TLS_HANDSHAKES`.

## Compatibility goal

The project tries to feel close to standard `asyncio`.

That is why the repository contains:

- compatibility logic in `_loop_compat.py`
- many tests for behavior that should match normal `asyncio`
- examples that use standard Python async patterns instead of a custom API style

## Current limitations

These gaps are visible in the current implementation:

- TLS uses a `rustls` backend with a narrower compatibility surface than
  CPython's OpenSSL-backed `ssl` module. In particular, encrypted private keys
  are not supported yet, and the fast-stream monkeypatch still falls back to
  standard-library helpers whenever `ssl` is enabled. TLS transport internals
  also still use helper-thread paths instead of the runtime-thread `vibeio`
  socket path.
- Subprocess support still has one notable gap: `preexec_fn` remains unsupported
  because running arbitrary Python between `fork()` and `exec()` is unsafe in
  this runtime model.
- Unix-specific APIs remain Unix-specific: `create_unix_server`,
  `create_unix_connection`, `add_signal_handler`, and `remove_signal_handler`.
- Several subprocess options such as `pass_fds`, `user`, `group`, and `umask`
  remain specific to Unix process spawning.
- The transport runtime model is still in transition: protocol readers on Unix
  avoid a coordination-thread hop, but native streams, generic descriptor
  watches, and TLS-heavy paths do not share one single-threaded I/O path.
