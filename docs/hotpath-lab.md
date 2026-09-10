# Rust / LLVM hot-path experiments

`scripts/hotpath_lab.py` makes local optimization experiments reproducible without
installing wheels, replacing `python/rsloop/_loop.*`, switching Python versions,
or rewriting the working tree. It uses committed source snapshots. Uncommitted
Rust changes are deliberately **not** included. Keep experiment directories under
the ignored `target/` tree. Existing output directories are never overwritten.

Run it with the existing project interpreter (`.venv/Scripts/python.exe` on
Windows, `.venv/bin/python` on Unix). The harness requires Python 3.10+ and Rust's
matching LLVM tools (`rustup component add llvm-tools-preview`). The workload
suite uses the project's Python dependencies. TLS workloads additionally need
`python scripts/generate_test_tls_certs.py tests/fixtures/tls`.

## Workflow

In these examples `python` means the interpreter above. Build commands use a
shared Cargo cache, so **do not run builds concurrently**, and never benchmark
while compiling, testing, or running another benchmark on the same machine.

```text
python scripts/hotpath_lab.py build --revision HEAD --out target/hotpath-lab/base
python scripts/hotpath_lab.py build --source-artifact target/hotpath-lab/base --instrument --out target/hotpath-lab/instrumented
python scripts/hotpath_lab.py profile --artifact target/hotpath-lab/instrumented --out target/hotpath-lab/training
python scripts/hotpath_lab.py packet --artifact target/hotpath-lab/base --function process_batch --source-file src/vibeio/driver/iocp.rs --profile target/hotpath-lab/training --out target/hotpath-lab/iocp-review
```

The packet contains original Rust, selected optimized LLVM function definitions,
per-workload execution counts, a review prompt, and build provenance. Use the
packet to propose **one** falsifiable Rust change, including edge-case tests.
Inspect the preserved assembly in `compiler/*.s` as well. A function absent from
the IR may have been inlined: select its caller rather than treating absence as
proof that no machine code exists.

`opt -passes=verify` checks the **full** emitted IR module before packet creation.
`selected.ll` is analysis-only: globals, declarations and debug metadata
are omitted; profile weights and referenced attributes are retained. It is not
independently compilable. Neither verification nor an
LLM review establishes semantic equivalence. Raw IR edits, unsafe assumptions,
floating-point reassociation, altered atomic ordering, and changed Python
refcount/exception/cancellation behavior require separate justification.

For compiler-directed optimization, use the same training profile in a PGO build:

```text
python scripts/hotpath_lab.py build --source-artifact target/hotpath-lab/base --pgo target/hotpath-lab/training --out target/hotpath-lab/pgo
python scripts/hotpath_lab.py test --artifact target/hotpath-lab/base --log target/hotpath-lab/base-tests.log
python scripts/hotpath_lab.py test --artifact target/hotpath-lab/pgo --log target/hotpath-lab/pgo-tests.log
python scripts/hotpath_lab.py compare --baseline target/hotpath-lab/base --candidate target/hotpath-lab/pgo --primary tcp_streams --blocks 12 --out target/hotpath-lab/pgo-holdout
```

For a source candidate, build its commit with `--revision <candidate-commit>`.
Use a dedicated worktree/branch if you need to preserve ongoing development.
This harness does not apply model output or create commits automatically.

## What is controlled

- Artifacts preserve source contents, commit, Python executable/version, compiler
  versions, exact Cargo command/flags, extension, LLVM IR, assembly, and PDB when
  emitted. SHA-256 inventories detect changed source/package/compiler outputs.
- Every benchmark child imports the selected artifact first and checks the
  actual native extension path and hash. The development installation is not
  used. `PYTHONHASHSEED=0`, fast streams enabled, and GC disabled during both
  measured and warmup workloads are explicit harness choices.
- PGO builds use an explicit native Cargo `--target` so build scripts and proc
  macros are not instrumented. Training sets `LLVM_PROFILE_FILE` only in workload
  children, with a new directory per workload and `%p-%m` shard filenames.
  Source/toolchain/ABI and merged-profile hash must match for profile use.
- Builds and training are separate from timing. **Instrumentation counts are
  frequencies, not CPU time.** Top internal counts may come from a cheap inner
  initialization loop. They nominate code for investigation, not CPU hotspots.
  Python 3.15's `--native` flag supplies synthetic boundary markers, not a Rust
  stack unwinder ([Python documentation](https://docs.python.org/3.15/library/profiling.sampling.html#special-frames)).
  PDBs alone cannot turn those markers into Rust function-level CPU samples.
- `plan.json` is written before measurement: primary workload, thresholds, data
  sizes, suite, seed, artifact provenance and benchmark hashes. Each block has
  a fresh baseline process and a fresh candidate process. AB/BA order is balanced
  and shuffled; workload order is also shuffled between blocks. By default each
  process performs one full warmup and one measured workload.
- `samples.jsonl` is flushed after every process. It includes binary identity,
  elapsed time, process CPU time, peak RSS, and per-process latency summaries.
  Warmups are retained but excluded from inference. Peak RSS includes warmups;
  process CPU time includes event-loop setup/teardown beyond the workload timer.
  `harness/` retains the exact runner and benchmark sources for audit (restore
  their usual repository layout when rerunning them).
- Confidence intervals resample **paired process log-time ratios**, never
  individual requests. The workload-family intervals use a Bonferroni-adjusted
  95% confidence target. Negative percentage changes mean faster. Twelve paired
  blocks are a minimum, not a promise of adequate statistical power.
- The default gate requires the primary interval to be entirely below -1%, and
  **every** workload's interval upper bound to be at most +3%. Any lower bound
  above +3% rejects the candidate. Short runs (<0.25 s), fewer than 12 blocks,
  or uncertain bounds cannot pass. The bulk workload is 64–128 MiB per connection,
  not the old very short 2 MiB transfer.
- Holdout changes concurrency, payloads, task batch size, and work volume.
  `tls_http` and `websocket_messages` can be added with `--workloads` for entirely
  untrained scenario families. Default profile training uses the six core
  workloads; holdout is the default comparison suite.

## Interpreting results honestly

Run an A/A comparison (`--baseline` and `--candidate` pointing to the same
artifact) to check harness/host noise. Keep background load stable; use OS CPU
affinity/power controls where appropriate and record them externally. This
harness does not pin CPUs on Windows or control thermal state. Child timeouts
fail the experiment; incomplete results are retained, not promoted.

`performance_gate_passed` is **not automatic acceptance**. Require both test
logs, inspect diagnostic latency/RSS regressions, and run a fresh confirmation
experiment. Rust tests use a debug build of the preserved source; Python tests
exercise the exact release extension. The current repository's Python tests are
used for both revisions. No result establishes correctness for all unsafe code.

Bootstrap intervals are approximate. Serial correlation, machine drift,
benchmark selection, and repeated searches over candidates can inflate apparent
confidence. Do not repeatedly tune against the holdout or stop as soon as an
interval looks favorable. Choose the primary and sample budget in advance,
retain rejected/inconclusive experiments, and use a new confirmation set for
any selected winner. This tool does not automate external LLM calls or upload code.

Earlier `target/llm-profiles/*baseline.json` / `*candidate.json` experiments
collected revisions in separate blocks. Pairing those files by run index was
not justified. Some sustained matrix repeats also shared a process; they were
not independent process replications. Treat those speedups and paired intervals
as exploratory, not validated evidence. This harness does not reuse them.

The PGO workflow follows the [Rust compiler documentation](https://doc.rust-lang.org/rustc/profile-guided-optimization.html),
including matching code-generation settings, explicit native targets, and
missing-profile diagnostics. IR/assembly output is provided by rustc's
[`--emit` option](https://doc.rust-lang.org/rustc/command-line-arguments.html#--emit-specifies-the-types-of-output-files-to-generate).
