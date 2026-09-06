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

## Run Rust lints

Clippy uses a risk-focused policy in `Cargo.toml`: `correctness` is denied,
and `suspicious`, `perf`, and `complexity` are warnings. Safety documentation,
pointer alignment, and holding locks or `RefCell` borrows across await points
are explicitly checked. Blanket `style` and `pedantic` checks are disabled;
`nursery` and `restriction` are not enabled as groups.

The same policy covers the embedded vibeio runtime, without a blanket Clippy
suppression. Exceptions should be scoped to the affected item and explain why
the rule does not fit (for example, retaining inline driver storage). Existing
vibeio allowances for Rust compiler compatibility are separate from this policy.

CI and the development command treat every enabled warning as an error:

```bash
uv run just clippy
```

The equivalent Cargo command is:

```bash
cargo clippy --all-targets --all-features -- -D warnings
```

CI also passes `--locked`. Use rustfmt for formatting:

```bash
cargo fmt --all --check
```

## Run tests

Run Rust library tests with the repository's Python linking configuration:

```bash
uv run python scripts/run_rust_tests.py
uv run python scripts/run_rust_tests.py --all-features
```

CI runs both configurations. Networking and timers are always compiled; embedded
`fs`, `process`, `signal`, `pipe`, `stdio`, `splice`, and `blocking-default`
modules are opt-in Cargo features. They do not change the default wheel build.
To check an individual module, use `--features fs` (or another feature name)
with the test runner. Some modules have platform-specific implementations.

Run the Python compatibility suite:

```bash
uv run python -m unittest discover -s tests
```

The `just` recipe also regenerates the TLS fixtures before running the suite:

```bash
uv run just test
```

If you want to focus on one area, use `unittest` discovery with a filename
pattern:

```bash
uv run python -m unittest discover -s tests -p 'test_run.py'
uv run python -m unittest discover -s tests -p 'test_compat.py'
uv run python -m unittest discover -s tests -p 'test_tls.py'
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
5. `src/bindings/loop_api.rs`
6. `src/engine/loop_core.rs`

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
Published wheels do not currently include this feature. Local development
builds must enable it explicitly.

Example build:

```bash
uv run --with maturin maturin develop --release --features profiler
```

The Python API then exposes:

- `rsloop.profile()`
- `rsloop.profiler_compiled()`
- `rsloop.profiler_running()`
- `rsloop.start_profiler()`
- `rsloop.stop_profiler()`

Use `rsloop.profiler_compiled()` to check whether the installed build includes
Tracy support before starting a profiling session.

## Current state of the project

This is still an alpha-stage project.

It already covers a lot of `asyncio` surface area, but some areas are still evolving:

- TLS compatibility
- transport internals
- helper-thread removal in older paths
- platform-specific behavior differences

That makes the repository a good place to learn from, but also a project where careful testing matters.
