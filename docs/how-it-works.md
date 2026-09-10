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
- `profiler.rs`: temporary no-op compatibility macros for former profiling scopes
- `rust_async.rs`: public Rust/Python async interop helpers for downstream extensions
- `async_event.rs`, `blocking.rs`, `python_names.rs`: support code used by the public pieces
- `platform/windows_vibeio.rs`: Windows-specific runtime support

You do not need to understand every file before using the project. For a first
pass, `lib.rs`, `bindings/loop_api.rs`, and `engine/loop_core.rs` are the most
useful entry points.

## Runtime model

Each loop currently uses two related execution contexts:

- a per-loop `vibeio` runtime lives on the thread running the Python event loop;
  it drives direct I/O while the loop is parked
- a dedicated Rust coordination thread dispatches loop commands, timers, and
  compatibility paths through a separate `vibeio` runtime

- Python tasks and callbacks still execute on the Python side
- plain TCP / Unix reads and non-TLS accepts run directly on `vibeio`
- generic descriptor watches and some TLS, write, and older transport paths
  still use helper threads

The separate coordination thread is transitional infrastructure. This hybrid
model explains why some paths run directly through the loop-thread reactor while
other paths still cross threads or use helper workers.

Stream transports pause their socket reader when the pending inbound queue
reaches 1 MiB and resume below 256 KiB. Buffered writes default to a 64 MiB
safety cap (`RSLOOP_MAX_WRITE_BUFFER_BYTES`), while TLS servers default to 256
simultaneous handshakes (`RSLOOP_MAX_PENDING_TLS_HANDSHAKES`). Set either
environment variable before importing `rsloop` to tune the limit.

## Compatibility goal

The project tries to feel close to standard `asyncio`.

That is why the repository contains:

- compatibility logic in `_loop_compat.py`
- many tests for behavior that should match normal `asyncio`
- examples that use standard Python async patterns instead of a custom API style

## Current limitations

Some important limitations are already known:

- TLS support is narrower than CPython's OpenSSL-based `ssl` support
- encrypted private keys are not supported yet
- some TLS and transport paths still rely on helper threads
- `preexec_fn` for subprocesses is unsupported
- Unix sockets and signal handlers are naturally Unix-only

These are good things to know before using the project in production.
