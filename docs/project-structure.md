# Project Structure

This page is a map of the repository.

## Top-level files

- `pyproject.toml`: Python package metadata and `maturin` build configuration
- `Cargo.toml`: Rust crate metadata and dependencies
- `README.md`: main repository overview
- `build.rs`: Rust build-time script
- `justfile`: useful development commands if you use `just`
- `uv.lock`: locked Python dependency data for `uv`

## Python package

Path: `python/rsloop/`

This is the Python-facing side of the project.

- `__init__.py`: public exports
- `_run.py`: friendly helpers for starting the loop
- `_loop_compat.py`: behavior that makes `rsloop` fit into `asyncio`
- `_bootstrap.py`: import-time environment setup
- `_profile.py`: profiler helpers

Think of this directory as the "Python wrapper layer".

## Rust extension

Path: `src/`

This is the engine room of the project.

- `lib.rs`: registers the Python extension module
- `python_api.rs`: large bridge between Python and Rust
- `loop_core.rs`: loop commands and state
- `runtime.rs`: runtime orchestration
- `callbacks.rs`: scheduled callback support
- `stream_transport.rs`: stream, server, and transport logic
- `process_transport.rs`: subprocess and pipe logic
- `tls.rs`: TLS handling
- `fast_streams.rs`: fast versions of common stream helpers

If you want to understand behavior changes, this directory is usually where the real implementation lives.

## Examples

Path: `examples/`

This directory shows the supported feature areas in runnable form.

- basics
- sockets
- streams
- Unix sockets
- accepted sockets
- pipes
- signals
- subprocesses

Examples are a good first stop before reading tests.

## Tests

Path: `tests/`

The tests tell you what behavior the project promises today.

- `test_run.py`: basic lifecycle and common operations
- `test_compat.py`: `asyncio` compatibility behaviors
- `test_tls.py`: TLS-related behavior

When you are unsure whether a feature is expected to work, check the tests.

## Demo

Path: `demo/`

This is an example FastAPI service used to compare event loops in a more realistic application.

It is helpful when you want to:

- see `rsloop` in a web-service shape
- compare it with stdlib `asyncio` and `uvloop`
- exercise the fast stream path

## Benchmarks

Path: `benchmarks/`

This directory contains performance comparison tools.

It compares:

- stdlib `asyncio`
- `uvloop`
- `rsloop`

Use this directory when you want numbers, not just correctness.

## Scripts

Path: `scripts/`

These are support scripts for maintainers:

- building wheels
- testing supported Python versions
- generating TLS test certificates
- listing Python versions used by the build scripts

You usually do not need these for normal package usage, but they matter for release and CI work.
