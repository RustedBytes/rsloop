# Development

This page is for contributors and readers of the codebase.

## Build the project

Local development uses Python 3.14.7, pinned in `.python-version`. Install that
interpreter before running the `uv` commands below. This development pin does
not change the package's Python 3.10+ support or the multi-version test matrix.

Local builds and build/test CI use Rust `nightly-2026-09-25`, pinned in
[`rust-toolchain.toml`](https://github.com/RustedBytes/rsloop/blob/master/rust-toolchain.toml).
Rustup selects it automatically
inside this repository. LLVM tools remain optional for PGO builds.

Quick Rust check:

```bash
cargo check
```

The nightly pin supplies the allocator API merged in
[rust-lang/rust#156882](https://github.com/rust-lang/rust/pull/156882). Rsloop
contains a tested internal prototype for a bounded, `System`-backed recycler:
only blocks up to 4 KiB with alignment up to 64 bytes are cached, no bin retains
more than 32 blocks, and total retained memory is capped at 256 KiB. Its first
ready-queue, timer, task, and transport rollout was not enabled: the balanced
holdout gate found no statistically supported primary speedup and did find
workload regressions. Stream payload buffers therefore keep their existing
purpose-built pools, and production collections still use the global allocator.
The Python and public Rust APIs do not expose allocator selection. The remaining
`allocator_ext` feature gate can be removed once the prototype is retired or its
allocator-aware collections stabilize.

Build the extension and install it into the current environment:

```bash
cargo build --release
uv run --with maturin maturin develop --release
```

Build release wheels into `dist/wheels`:

```bash
scripts/build-wheels.sh
```

[`scripts/build-wheels.sh`](https://github.com/RustedBytes/rsloop/blob/master/scripts/build-wheels.sh)
currently defaults to
CPython `3.10 3.11 3.12 3.13 3.14 3.14t 3.15`, and uses
`uv python install` / `uv python find` to locate interpreters.

### Profile-guided wheel builds

Optionally build wheels with profile-guided optimization (PGO):

```bash
rustup component add llvm-tools-preview
scripts/build-pgo-wheels.sh
```

For each requested Python ABI, the PGO wrapper creates an instrumented wheel,
trains it on sustained HTTP, TLS, WebSocket, mixed-stream, bulk-transfer,
idle-connection, callback, task, and TCP workloads, merges the resulting LLVM
profiles, and builds that ABI's final wheel with its matching profile. Per-ABI
training avoids discarding counters when PyO3's generated control flow differs
between Python versions or free-threaded builds. The target must be native
because the instrumented extension runs during training.

Set `RSLOOP_PGO_SCENARIOS` to override the comma-separated network scenarios.
The **Wheels** CI workflow disables PGO by default: tagged releases and ordinary
manual runs use the normal release-wheel builder. To opt in, enable the `pgo`
checkbox when manually running the workflow. LLVM tools are installed only for
PGO runs; source-distribution and publishing steps are unchanged.

When enabled, PGO is used on every supported platform except Windows ARM64.
Rust profile-generation binaries currently crash on that target
([rust-lang/rust#156675](https://github.com/rust-lang/rust/issues/156675)), so it
temporarily falls back to the normal fat-LTO release build.

## Run the Python test matrix

```bash
scripts/test-supported-pythons.sh --debug
```

The matrix explicitly installs the `test` dependency group, including the
WebSocket dependency used by the TLS tests. Optional framework scripts in
`tests/integration/packages` use the separate `integration` group. Install that
group explicitly when working on integrations; it is intentionally excluded
from the default `dev` environment because some frameworks have native
dependencies that do not support every Python interpreter.

The default matrix covers CPython 3.10 through 3.15 plus the free-threaded
CPython 3.14 build (`3.14t`). Until Python 3.15 is final, uv resolves `3.15` to
the latest available prerelease standalone build.

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

On Windows, the runner keeps Cargo artifacts under an interpreter-specific
directory such as `target/rust-tests/cp314` or `target/rust-tests/cp314t`.
This avoids a full relink when switching between regular, free-threaded, or
newer Python environments. The first run for each ABI builds its own cache.

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

The complete recipe runs the independent Rust and Python suites concurrently
after generating their shared TLS fixtures.

For a shorter local feedback loop, skip high-repetition and real-network slow
tests. The normal suite uses merge-gating repetition counts; the stress recipe
reruns marked tests with their original high counts:

```bash
uv run just test-fast
uv run just test-python
uv run just test-stress
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
Repository benchmark and runner tests are kept out of the OS/Python matrix; run
them with `uv run just test-tooling`. Optional framework/database smoke scripts
remain under `just test-frameworks` and `just test-databases`; pytest does not
collect them by default.

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

Python 3.15 includes a low-overhead external sampling profiler that can run
rsloop without a Cargo feature, special build, or in-process instrumentation.
Generate an interactive flame graph with:

```bash
uv run --python 3.15 --with maturin maturin develop --release
uv run --python 3.15 python -m profiling.sampling run \
  --all-threads --native --flamegraph \
  -o rsloop-profile.html examples/01_basics.py
```

`--all-threads` includes rsloop's runtime thread. `--native` adds synthetic
native-boundary markers; it does not unwind and name individual Rust frames.
The profiler and target must use the same Python 3.15 interpreter. Python 3.15
does not allow these options together with `--async-aware`; use a separate
async-aware pass when coroutine reconstruction is more important than native
and multi-thread visibility.

For repeatable workload profiles, use `--profile-rsloop-dir` with either
benchmark runner. Profile passes are unmeasured and produce HTML flame graphs.

See Python's [special-frame documentation](https://docs.python.org/3.15/library/profiling.sampling.html#special-frames).
Use a native stack profiler when attributing CPU time to individual Rust
functions.

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
