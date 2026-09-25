# Scheduler batch allocation reuse

Measured on 2026-09-25. This change uses the stabilized allocator API available
in the pinned `nightly-2026-09-25` compiler, without an `allocator_ext` feature
gate. It does **not** claim compatibility with an older stable-channel compiler.

The cache is **opt-in** through `scheduler-batch-cache`, not enabled in default
builds or wheels. Its scheduler-turn improvement comes with a confirmed TCP
slowdown, so the general-purpose event loop retains its original allocation
and drain path.

## What changes

`Runtime::block_on` previously allocated and freed its ready-task vector on
every entry. With the feature enabled, the runtime owns a single-slot, exact-layout
`System` allocation cache, used through `Allocator` and `Vec::with_capacity_in`.
Successive calls reuse that storage; batches within a call keep the vector's
capacity. Tests verify one system allocation across repeated runtime turns,
including root-future unwinding and reuse afterward.

The usual retained allocation is 2 KiB on 64-bit systems, with a hard 16 KiB
ceiling per runtime. Runtime destruction frees it. This reduces allocator
traffic, not the live size of tasks or a guaranteed amount of process RSS.
No mutex, allocator `Arc` clone, size-class search, or public configuration is
introduced. The cache sits outside `RuntimeInner` to preserve the hot shared
scheduler state's layout.

Allocator-aware `Vec::drain` is not in the stabilized subset. A private full
drain uses stable vector primitives, retains the allocation, preserves FIFO
order, and drops unconsumed tasks on unwind. Strict-provenance Miri tests cover
partial consumption, destructor panics, zero-sized elements, forgotten drains,
alignment, growth, zeroing, disjoint simultaneous allocations, and runtime
panic cleanup. Kani's older compiler retains an ordinary-vector fallback;
its proofs do not verify the custom allocator.

The previous shared size-class allocator remains rejected. Early versions of
this change also regressed busy turns when replacing the vector each batch or
using optional task slots. Neither implementation is retained.

## Native scheduler measurement

Host: Intel Core i9-9900K, Linux x86-64; Rust 1.100.0-nightly
(`f7575a9da`, LLVM 23.1.1). Identical `runtime_turns.rs` source and lockfile were
built with the same compiler and release profile, against `d4f56f1` and the
new implementation with `--features scheduler-batch-cache`. No other builds
or tests ran during timing. CPU affinity
was unrestricted; these are local results, not a cross-platform guarantee.

Each comparison used 24 randomized, balanced baseline/candidate process pairs,
10,000 warm-up turns per process, and paired bootstrap 95% confidence intervals.
Negative percentages mean less elapsed time.

| Workload | Turns per sample | Baseline median | Candidate median | Change (95% interval) |
| --- | ---: | ---: | ---: | ---: |
| Idle scheduler entry/exit | 7,000,000 | 0.76260 s | 0.64408 s | -15.41% [-15.87%, -14.88%] |
| 64 continuously ready tasks | 500,000 | 1.12497 s | 1.12506 s | +0.30% [-0.48%, +1.14%] |

The idle-turn gain passed the declared 1% timing threshold; the busy result is
inconclusive and within the 3% regression budget at this confidence level.
These measurements isolate scheduler turns, not Python or network throughput.

Reproduce with `scripts/compare_runtime_turns.py` as described in
[Development](development.md). Use `--iterations 7000000 --tasks 0 --seed 999`
for idle turns and `--iterations 500000 --tasks 64 --seed 1000` for busy turns.
Both use `--blocks 24` and separate output directories.

Local raw plans, binary hashes, paired samples, and reports are in
`target/allocator-batch-optin-idle` and `target/allocator-batch-optin-active`.
These ignored directories are local experiment artifacts, not checked-in data.

## Python application workloads

The six-workload holdout used CPython 3.14.7, 24 paired process blocks,
seed 997, and `tcp_streams` as the predeclared primary. Compiler versions,
non-PGO flags, benchmark source, and artifact hashes were checked by the
hot-path harness. Each interval below uses 99.583% confidence (Bonferroni
adjustment across six timing and six RSS metrics).

| Workload | Time change (interval) | Peak RSS change (interval) |
| --- | ---: | ---: |
| Callbacks | -0.14% [-0.70%, +0.36%] | +0.00% [-0.01%, +0.02%] |
| Tasks | -0.64% [-1.50%, +0.14%] | -0.19% [-0.41%, +0.01%] |
| TCP streams | +2.10% [+0.64%, +3.56%] | +0.00% [-0.23%, +0.23%] |
| HTTP keep-alive | -0.30% [-6.67%, +3.78%] | +0.01% [-0.28%, +0.32%] |
| Mixed streams | -0.71% [-2.98%, +1.33%] | +0.03% [-0.29%, +0.38%] |
| Bulk transfer | -0.43% [-2.01%, +0.82%] | -0.00% [-0.02%, +0.01%] |

The application-wide gate is **inconclusive**, not passed: the primary did
not improve, TCP showed a small slowdown, and the TCP/HTTP upper timing bounds
exceed the 3% budget. Peak RSS stayed well within budget, but there is no
established application-wide memory reduction or throughput improvement.

An independent 40-pair TCP confirmation (seed 998) found **+2.75%** elapsed time,
with a 97.5% interval of **[+1.79%, +3.85%]**. RSS changed +0.04%
[-0.14%, +0.24%]. This confirmed the tradeoff and is why the cache is opt-in.
Raw confirmation data is in `target/allocator-batch-final-tcp-confirmation`.

The exact baseline is `d4f56f1`; the local cache-enabled measurement snapshot is
`a87092ec214b6daa9288818109f58cb6034846d3`. It predates the final feature guard:
these application results evaluate the cache integration, not the final default
build (which uses the original path). Artifact directories
are `target/allocator-batch-baseline-final` and `target/allocator-batch-final`;
the complete plan, samples, manifests and report are under
`target/allocator-batch-final-holdout`.

The following command records the cache-enabled snapshot comparison. To repeat
the experiment on later revisions, use a cache-enabled candidate build; an
ordinary default build no longer enables the cache. Use fresh output directories.

```bash
.venv/bin/python scripts/hotpath_lab.py compare \
  --baseline target/allocator-batch-baseline-final \
  --candidate target/allocator-batch-final \
  --primary tcp_streams --suite holdout --blocks 24 --seed 997 \
  --out target/allocator-batch-final-holdout
```

## Correctness checks

- Root Rust suites: 307 passed with default features; 407 with all features.
- Embedded runtime all-feature suite: 283 passed; 16 doctests passed.
- Strict-provenance Miri: 10 allocator/drain/runtime regression tests passed.
- Kani: all 31 `merge_` harnesses passed, using the ordinary-vector fallback.
- Clippy: both crates, all targets/features, warnings denied.
- Python: 192 passed and 2 skipped on **each** isolated extension; tooling,
  stress, and slow-network markers were excluded (73 deselected).
- Final default development install: the same 192 Python tests passed, with
  2 skipped and 73 deselected.
- Repository tooling: 67 passed. The new comparison runner also passes Ruff.

Relevant commands:

```bash
python3 scripts/run_rust_tests.py --all-features
cargo test --manifest-path tools/vibeio-check/Cargo.toml --all-features --locked
MIRIFLAGS=-Zmiri-strict-provenance cargo miri test \
  --manifest-path tools/vibeio-check/Cargo.toml --features scheduler-batch-cache batch_
cargo kani --harness merge_ -j 2 --output-format terse
PYTEST_ADDOPTS="-m 'not tooling and not stress and not slow_network'" \
  .venv/bin/python scripts/hotpath_lab.py test \
  --artifact target/allocator-batch-final --python-child
```

The Python command was also run against `target/allocator-batch-baseline-final`.
These checks do not replace cross-platform CI or the excluded stress/network
suites.
