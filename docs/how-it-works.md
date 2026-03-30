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
- `_run.py`: defines `run(...)` and `new_event_loop()`
- `_loop_compat.py`: compatibility helpers and monkeypatches
- `_bootstrap.py`: startup helpers, including Windows DLL and SSL-related setup
- `_profile.py`: small Python wrappers around the profiler API

This layer is a thin adapter. It keeps the user-facing API pleasant while the heavy lifting happens in Rust.

## The Rust layer

The Rust code lives in `src/`.

Important files:

- `lib.rs`: extension module entry point
- `python_api.rs`: exposes Rust functionality as Python classes and functions
- `loop_core.rs`: core loop state and commands
- `runtime.rs`: runtime coordination work
- `callbacks.rs`: callback handles and scheduling helpers
- `stream_transport.rs`: stream transports and servers
- `process_transport.rs`: subprocess and pipe transport support
- `fast_streams.rs`: optimized stream helpers used by patched `asyncio` APIs
- `tls.rs`: TLS support
- `fd_ops.rs`: lower-level file descriptor work
- `context.rs`: running-loop and context management helpers
- `errors.rs`: shared error types
- `profiler.rs`: Tracy profiler support
- `async_event.rs`, `blocking.rs`, `python_names.rs`: support code used by the public pieces
- `windows_vibeio.rs`: Windows-specific support

You do not need to understand every file before using the project. For a first pass, `lib.rs`, `python_api.rs`, and `loop_core.rs` are the most useful entry points.

## Runtime model

The project currently uses a hybrid runtime model.

The simple explanation is:

- there is a dedicated Rust runtime thread for loop coordination
- Python tasks and callbacks still execute on the Python side
- some I/O paths are already handled directly by the runtime thread
- some paths still use helper threads, especially around TLS and older transport code

That is important because it explains why the project is fast in some areas while still being a work in progress in others.

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
