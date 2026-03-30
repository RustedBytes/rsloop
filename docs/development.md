# Development

This page is for contributors and readers of the codebase.

## Build the project

Quick Rust check:

```bash
cargo check
```

Build the extension and install it into the current environment:

```bash
uv run --with maturin maturin develop --release
```

## Run tests

```bash
uv run pytest
```

If you want to focus on one area:

```bash
uv run pytest tests/test_run.py
uv run pytest tests/test_compat.py
uv run pytest tests/test_tls.py
```

## Build the docs

With MkDocs installed:

```bash
mkdocs serve
mkdocs build
```

Or with `uv` without adding a permanent dependency:

```bash
uvx --from mkdocs mkdocs serve
uvx --from mkdocs mkdocs build
```

## Good places to start reading

If you are new to the project, this order works well:

1. `python/rsloop/__init__.py`
2. `python/rsloop/_run.py`
3. `python/rsloop/_loop_compat.py`
4. `src/lib.rs`
5. `src/python_api.rs`
6. `src/loop_core.rs`

This order moves from simple Python wrappers to the larger Rust internals.

## How to think about changes

When you add or debug a feature, it helps to ask:

1. Is this a Python wrapper issue or a Rust implementation issue?
2. Does the behavior need to match standard `asyncio` exactly?
3. Is the feature cross-platform, Unix-only, or Windows-specific?
4. Do the tests already describe the expected behavior?

Those four questions usually point you to the right part of the codebase.

## Profiling

Profiling support exists behind the Rust `profiler` feature and uses Tracy.

Example build:

```bash
uv run --with maturin maturin develop --release --features profiler
```

The Python API then exposes:

- `rsloop.profile()`
- `rsloop.start_profiler()`
- `rsloop.stop_profiler()`

## Current state of the project

This is still an alpha-stage project.

It already covers a lot of `asyncio` surface area, but some areas are still evolving:

- TLS compatibility
- transport internals
- helper-thread removal in older paths
- platform-specific behavior differences

That makes the repository a good place to learn from, but also a project where careful testing matters.
