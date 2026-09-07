# Embedded runtime checks

This unpublished crate includes `src/vibeio/lib.rs` directly, without copying
runtime code or compiling the Python extension and TLS dependencies.

[Executable examples](EXAMPLES.md) cover timers, buffer ownership, pipes, and copy.
They are included as crate documentation and run as doctests with `cargo test`.
Pipe examples execute on Unix when the `pipe` feature is enabled; use
`--all-features` to run them alongside the unconditional examples.

```sh
cargo fmt --manifest-path tools/vibeio-check/Cargo.toml --all -- --check
cargo clippy --manifest-path tools/vibeio-check/Cargo.toml --all-targets --all-features --locked -- -D warnings
cargo test --manifest-path tools/vibeio-check/Cargo.toml --locked
cargo test --manifest-path tools/vibeio-check/Cargo.toml --all-features --locked
cargo check --manifest-path tools/vibeio-check/Cargo.toml --all-targets --all-features --locked --target x86_64-pc-windows-gnu
cargo check --manifest-path tools/vibeio-check/Cargo.toml --all-targets --all-features --locked --target aarch64-apple-darwin
```

Install cross-compilation targets with `rustup target add` first. A successful
`cargo check` verifies compilation, not execution. Native Linux, Windows, and
macOS jobs in `.github/workflows/vibeio-native.yml` run formatting, documentation,
strict Clippy under the root crate's risk-focused policy, and native tests
(including doctests). The main Tests workflow reuses this workflow.

## Run native cleanup verification in CI

After pushing the workflow, open **Actions → Vibeio native tests → Run workflow**
and select the branch to test. The manual defaults run the all-feature suite
three times on Linux, Windows and macOS, plus separate lint/test runs for each
feature. Choose 1, 3 or 10 repetitions; isolated-feature checks can be disabled
for a quicker all-feature run. No Python setup or extension build is required.

With GitHub CLI:

```sh
gh workflow run vibeio-native.yml --ref master -f repetitions=3 -F isolated_features=true
```

Each platform uploads a `vibeio-native-*` artifact containing the commit and
compiler details, native test inventory, and full test output (including skip
messages). Failures propagate through log capture, jobs have a 30-minute timeout,
and one platform's failure does not cancel the others. Inspect the logs for
capability-based skips before treating a green run as coverage of a specific
kernel operation. Existing platform-gated IOCP/AFD, process callback, kqueue,
buffer and cancellation regressions execute where enabled; this does not add
fault injection for every remaining exceptional lifecycle path.

Both workflows are manually dispatched; editing them does not run CI.

Keep Clippy policy, dependency versions and feature wiring aligned with the root
manifest when updating them. The initial lockfile was resolved from the root lockfile. This
harness intentionally does not enable the root build script's platform syscall
cfgs, so Linux harness tests also exercise the portable `accept` fallback;
the root Rust tests cover the normal `accept4` path. Production pipe creation
uses the Rust standard library's platform-specific close-on-exec handling.
