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

## Run the Python test matrix

```bash
scripts/test-supported-pythons.sh --debug
```

The matrix explicitly installs the `test` dependency group, including the
WebSocket dependency used by the TLS tests. Optional framework scripts in
`tests/packages` use the separate `integration` group, which is also included
in `dev`. Those frameworks may have native dependencies that do not support
free-threaded Python; they are not required for the core pytest suite.

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
uv run python -m pytest
```

The `just` recipe also regenerates the TLS fixtures before running the suite:

```bash
uv run just test
```

To focus on one area, pass a test file or use pytest's `-k` selector:

```bash
uv run python -m pytest tests/test_run.py
uv run python -m pytest tests/test_compat.py
uv run python -m pytest tests/test_tls.py -k start_tls
```

CI and `just test` use `scripts/run_python_tests.py`, which forwards pytest
arguments and prints recurring stack dumps when tests stop making progress.
For the same diagnostics locally, run `uv run python scripts/run_python_tests.py`.
Optional framework/database smoke scripts remain under `just test-frameworks`
and `just test-databases`; pytest does not collect them by default.

Use pytest's `monkeypatch` fixture for attribute and environment overrides, and
pytest-mock's `mocker` fixture for mocks, spies, and call assertions. Use
`monkeypatch.context()` when a shared function must be restored before the test
finishes (for example, import machinery or warning handling).

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

Use Python 3.15's external sampling profiler. It requires no Cargo feature or
instrumented build:

```bash
uv run --python 3.15 --with maturin maturin develop --release
uv run --python 3.15 python -m profiling.sampling run \
  --all-threads --native --flamegraph \
  -o rsloop-profile.html examples/01_basics.py
```

For repeatable workload profiles, use `--profile-rsloop-dir` with either
benchmark runner. Profile passes are unmeasured and produce HTML flame graphs.

`--native` adds synthetic native-boundary markers; it does not unwind and name
individual Rust frames. See Python's [special-frame documentation](https://docs.python.org/3.15/library/profiling.sampling.html#special-frames).
Use a native stack profiler when attributing CPU time to individual Rust functions.

For isolated Rust/LLVM optimization experiments, runtime-only PGO training,
LLM review packets, and randomized paired comparisons, see the
[hot-path experiment harness](hotpath-lab.md).

## Current state of the project

This is still an alpha-stage project.

It already covers a lot of `asyncio` surface area, but some areas are still evolving:

- TLS compatibility
- transport internals
- helper-thread removal in older paths
- platform-specific behavior differences

That makes the repository a good place to learn from, but also a project where careful testing matters.
