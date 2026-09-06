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
macOS jobs in `.github/workflows/tests.yml` run formatting, strict Clippy under
the root crate's risk-focused policy, and default/all-feature tests (including
doctests). That workflow is manually dispatched; editing it does not run CI.

Keep Clippy policy, dependency versions and feature wiring aligned with the root
manifest when updating them. The initial lockfile was resolved from the root lockfile. This
harness intentionally does not enable the root build script's platform syscall
cfgs, so Linux harness tests also exercise the portable `accept` fallback;
the root Rust tests cover the normal `accept4` path. Production pipe creation
uses the Rust standard library's platform-specific close-on-exec handling.
