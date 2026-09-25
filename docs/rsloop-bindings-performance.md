# Rsloop binding hot paths

This experiment targets rsloop itself, not the embedded vibeio scheduler.
The allocator-cache feature remains disabled in both benchmark builds.

## Findings and implementation

Code inspection covered callback scheduling and dispatch, task construction,
the loop-thread ready queue, and stream delivery. Several transport costs are
already addressed by pooled reads, ownership transfer, and cached protocol
callbacks. This change does not alter transport buffering, scheduler fairness,
or I/O routing.

Two avoidable costs remained in rsloop's Python bindings:

1. `PyLoop` stores only an `Arc<LoopCore>` but used PyO3's mutable-object borrow
   bookkeeping on calls. It is now a frozen Rust wrapper; `LoopCore` retains
   its existing locks and atomics. Hot callback and task paths use shared
   `get()` access. This follows PyO3's
   [frozen-class model](https://pyo3.rs/main/class#frozen-classes-opting-out-of-interior-mutability).
2. Named/context task construction created a keyword dictionary, copied it,
   and queried Python's running-loop function. Ordinary options now use the
   existing cached vectorcall keyword tuples and an explicit loop argument.
   A fixed four-pointer stack array also replaces the temporary Rust vector.
   Custom task factories and explicit `eager_start` retain their existing path.

Python subclass attributes, weak references, context capture, and reentrant
calls remain supported. The Rust API becomes shared-only for the wrapped
`PyLoop`: use `get()` or `borrow()`, not `borrow_mut()`, and change loop state
through `LoopCore`. This is a Rust source-compatibility consideration, not a
freeze of Python subclass dictionaries or of loop state.

## Measurement protocol

Host: Intel Core i9-9900K, Linux x86-64, CPython 3.14.7, pinned Rust
`nightly-2026-09-25`. Baseline: `a8b8404`. The local candidate snapshot
`be0170024921` contains the production binding changes; later edits add
benchmark coverage, compatibility-test guards, and this report.

The extension artifacts are immutable, built with the same compiler and
release flags. Each paired block runs baseline and candidate in fresh
processes, with balanced randomized order and an unmeasured warm-up. Builds
and tests do not run during measurement. The primary workload is
`task_options`; every included workload also has a 3% elapsed-time and peak-RSS
regression budget. Confidence intervals are paired bootstrap intervals with
Bonferroni adjustment across the timing/RSS family.

The new opt-in `task_options` benchmark creates explicitly named,
explicit-context tasks, each yielding once. It reuses one context for all
tasks. It is separate from ordinary `tasks`, and does not replace or alter
the six default benchmark workloads. It requires Python 3.11+.

Training used 12 pairs, seed 2001, and callbacks/tasks/task-options/TCP. Its
balanced gate passed: callback time changed -2.98%, ordinary tasks -3.12%,
and task options -10.10%; the TCP interval overlapped zero.

## Holdout results

The unchanged candidate passed the **balanced timing/RSS gate** over 24 paired
blocks (seed 2002). Holdout changed task batch sizes, work volumes, payloads,
and connection counts. Negative percentages mean less elapsed time or RSS.
Intervals below use 99.643% confidence per metric, targeting 95% across all
14 timing/RSS metrics.

| Workload | Elapsed-time change (interval) | Peak RSS change (interval) |
| --- | ---: | ---: |
| Callbacks | -3.26% [-5.65%, -0.69%] | +0.00% [-0.02%, +0.02%] |
| Ordinary tasks | -2.62% [-3.46%, -1.74%] | -0.08% [-0.35%, +0.19%] |
| Named/context tasks | -10.36% [-11.29%, -9.37%] | +0.34% [+0.19%, +0.51%] |
| TCP streams | +0.15% [-1.09%, +1.42%] | -0.15% [-0.44%, +0.18%] |
| HTTP keep-alive | -1.71% [-3.60%, +0.13%] | -0.05% [-0.49%, +0.35%] |
| Mixed streams | -1.76% [-3.34%, -0.21%] | -0.05% [-0.60%, +0.53%] |
| Bulk transfer | -0.39% [-2.03%, +1.07%] | -0.01% [-0.02%, +0.01%] |

This establishes a gain on the measured Python scheduling workloads, not a
universal network speedup or a process-memory reduction. In particular, the
task-options workload had a small RSS increase despite fewer transient
allocations. Every timing and RSS interval upper bound stayed below +3%.

## Correctness and compatibility

- All-feature Rust tests: 407 passed; all-target/all-feature Clippy passed.
- CPython 3.14.7 development install: 209 passed, 2 skipped.
- CPython 3.14.0 free-threaded: 210 passed, 1 skipped, including parallel loops
  and verification that importing the extension does not re-enable the GIL.
- CPython 3.10.18: 194 passed, 17 skipped. Explicit Task-context cases are
  skipped because that stdlib option requires Python 3.11+.
- Repository tooling: 69 passed, including the new workload-selection and
  benchmark-execution tests.

Python commands selected `not tooling and not stress and not slow_network`
(75 deselected in the final suite). This is Linux validation, not a substitute
for the cross-platform CI matrix or the excluded stress/network suites.
The new regressions cover all name/context combinations, before and during
a running loop, with debug mode on and off, plus subclass attributes, weak
references, and reentrant scheduling. Existing factory/eager-start tests also
pass. No additional unsafe Rust was introduced; `LoopCore` synchronization
is unchanged.

## Reproduction

Reproduction (use fresh output directories):

```bash
.venv/bin/python scripts/hotpath_lab.py build \
  --revision a8b8404 --out target/rsloop-bindings-baseline
# Build a committed snapshot containing this change as the candidate.
.venv/bin/python scripts/hotpath_lab.py build \
  --revision <candidate-commit> --out target/rsloop-bindings-candidate
.venv/bin/python scripts/hotpath_lab.py compare \
  --baseline target/rsloop-bindings-baseline \
  --candidate target/rsloop-bindings-candidate \
  --workloads callbacks,tasks,task_options,tcp_streams,http_keepalive,mixed_streams,bulk_transfer \
  --primary task_options --suite holdout --blocks 24 --seed 2002 \
  --out target/rsloop-bindings-holdout
```

Local source snapshots, extension hashes, execution plans, samples, and reports
are under `target/rsloop-bindings-*`. These ignored local artifacts are not
checked-in benchmark data. Results are host/workload-specific, not a guarantee
of equal gains in other applications.
