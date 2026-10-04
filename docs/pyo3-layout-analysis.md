# PyO3 internals and callback storage

This follow-up to the [hotpath profile](hotpath-rs-profile.md) inspects PyO3
0.29.3 and measures a candidate against `58e30f4` (the version without the
`Handle` freelist). The changes target allocation size and reference ownership.

## Findings and changes

PyO3 already implements `call0()` through `PyObject_CallNoArgs`. For a Rust
one-element argument tuple, its CPython implementation calls
`PyObject_CallOneArg`; larger Rust tuples use a stack argument array and
vectorcall. rsloop's zero- and one-argument callback paths already reach these
implementations. Replacing them with handwritten FFI would duplicate the
existing optimization. Sources: [any.rs](https://github.com/PyO3/pyo3/blob/v0.29.3/src/types/any.rs),
[tuple.rs](https://github.com/PyO3/pyo3/blob/v0.29.3/src/types/tuple.rs).

PyO3 also implements `PyCallArgs` for `&Py<PyTuple>`. The many-argument callback
path and `run_in_context` cloned an owned tuple reference just to pass it into
`call1`, then released that reference after the call. They now pass the borrowed
tuple directly. This removes an increment/decrement pair for ordinary mortal
tuples while retaining the same call and exception behavior. The tuple's owner
remains alive throughout the call. Source:
[call.rs](https://github.com/PyO3/pyo3/blob/v0.29.3/src/call.rs).

The frozen `PyHandle` already stores `ReadyCallback` inline and avoids dynamic
borrow checking. Its returned reference and queued reference have independent
lifetimes: the caller can discard or cancel the returned handle before dispatch.
Those references cannot simply be removed. PyO3's freelist implementation takes
a mutex both when allocating and when freeing, including allocation attempts
that find an empty freelist. This provides an additional mechanism behind the
[previous freelist result](hotpath-rs-optimization.md), beyond an empty-list
check. Sources: [pycell layout](https://github.com/PyO3/pyo3/blob/v0.29.3/src/pycell/impl_.rs),
[freelist allocation and release](https://github.com/PyO3/pyo3/blob/v0.29.3/src/impl_/pyclass.rs).

Two layout changes reduce storage without new unsafe code:

- `ReadyCallback` stores its source tag and integer payload as separate fields.
  Its booleans can then occupy padding that belonged to the payload-bearing enum.
  `CallbackKind` and its accessor retain their existing interface. Descriptors
  remain 64-bit; a test checks every source and extreme signal/descriptor values.
- `ReadyItem` boxes the three-word TCP-reader startup payload. This pays one
  allocation at generic TCP-reader startup while reducing every queue slot.
  The descriptor and both owned `Arc`s are preserved, including the fallback
  that returns a command when local enqueue is unavailable.

Compiler-reported layouts on Linux x86-64, CPython 3.14.7,
Rust 1.100.0-nightly (`f7575a9da`):

| Type | Baseline | Candidate |
| --- | ---: | ---: |
| `ReadyItem` | 32 bytes | 24 bytes |
| `ReadyCallback` / inline `PyHandle` | 64 bytes | 56 bytes |
| PyO3 `PyStaticClassObject<PyHandle>` | 88 bytes | 80 bytes |

These are type sizes, not total allocator or process memory. For equal queue
capacity, the ready-item buffer requires 25% fewer bytes. Rust does not promise
these layouts across compilers or targets. The class-object figure includes the
Python object header and weak-reference storage on this build.

## Measurements

Both builds are uninstrumented releases on an Intel Core i9-9900K. Each
comparison uses 12 paired blocks with balanced, shuffled AB/BA order, fresh
processes, and one full warmup per process. GC is disabled during workloads;
fast streams are enabled. Timing intervals bootstrap paired process log ratios
and adjust for the eight workload/metric comparisons with a 95% family confidence
target. Peak RSS includes warmups.
The runner does not control CPU frequency or other host activity.

Initial workload shapes and results:

| Workload | Baseline median | Candidate median | Paired timing change (adjusted interval) |
| --- | ---: | ---: | ---: |
| 2,000,000 callbacks | 470.37 ms | 460.58 ms | -0.8% [-3.5%, +5.4%] |
| 300,000 tasks, batches of 10,000 | 595.55 ms | 595.59 ms | -1.0% [-3.3%, +1.1%] |
| 20,000 TCP roundtrips, 1 KiB | 322.63 ms | 327.58 ms | -0.7% [-8.8%, +3.5%] |
| HTTP, 16 connections, 2,000 requests each | 586.88 ms | 581.80 ms | -1.8% [-14.9%, +7.4%] |

The paired change is not the ratio of the two medians. Timing is inconclusive:
every interval includes zero, and the lab's speed-promotion gate did not pass.

Callback peak RSS fell from 401.15 to 355.92 MiB, a paired change of -11.28%
(adjusted interval [-11.31%, -11.25%]). Task peak RSS fell from 46.98 to
45.95 MiB (-2.08%). TCP peak RSS increased by 0.23 MiB (+0.65%); HTTP peak RSS
was effectively unchanged. The memory reduction agrees with the layout changes;
it is not evidence of a statistically established speedup.

A fresh 12-block confirmation (seed 2718) changed volume, batch size, payload,
and connection count:

| Workload | Baseline median | Candidate median | Paired timing change (adjusted interval) |
| --- | ---: | ---: | ---: |
| 3,000,000 callbacks | 711.88 ms | 690.63 ms | -2.4% [-3.7%, +0.04%] |
| 350,000 tasks, batches of 777 | 617.32 ms | 620.67 ms | -0.2% [-1.5%, +1.1%] |
| 25,000 TCP roundtrips, 8 KiB | 518.52 ms | 518.83 ms | +0.9% [-0.9%, +3.2%] |
| HTTP, 7 connections, 4,000 requests each | 512.25 ms | 516.27 ms | +0.6% [-1.1%, +2.6%] |

Callback peak RSS fell from 584.94 to 516.70 MiB (-11.67%, adjusted interval
[-11.69%, -11.64%]). Other peak-RSS changes were near zero. Both comparison
reports classify timing as **inconclusive**. Keep these changes for their
demonstrated memory benefit; do not present them as a proven speedup or as
passing the lab's speed-promotion gate. The intervals also do not rule out all
small timing regressions.

The combined experiment does not isolate the contribution of each edit. The
standard callback burst uses no arguments, so it also does not measure the
tuple-borrowing change specifically. These steady-state network workloads do
not establish the cost of boxing under heavy connection churn.

## Other data structures

Keep `VecDeque` for ready work: its FIFO operations, batch swaps, and existing
fairness behavior fit the access pattern. An unordered slab would require an
additional ordering structure. A generational arena could help Rust-only
callbacks, but Python handles still need independent Python-object identity and
weak references, so it would not remove their object allocations.

Keep the timer `BinaryHeap` pending a timer-heavy profile. It already orders by
deadline and insertion sequence. A timer wheel introduces resolution and
cancellation tradeoffs without evidence here that timer ordering is expensive.
The `PyTimerHandle` freelist is a separate experiment; callback-burst results do
not establish that it should also be removed.

Descriptor maps hold externally supplied, potentially sparse identifiers.
Indexing a vector directly by descriptor can waste memory, and changing hashing
or adding a slab-to-descriptor map needs a workload showing that lookup matters.
The earlier dispatcher profile was small, so replacing cross-thread channels is
also lower priority than callback allocation.

Context capture/enter/exit already use CPython directly. Typed `PyContext`
wrappers alone would not reduce the calls or eliminate context propagation.

## Validation and reproduction

The candidate passes 308 Rust library tests, 215 Python core tests (2 skipped),
and `cargo check --all-features --locked`. Existing tests cover callback
arguments, context capture, weak references, cancellation, socket progress,
server startup, and TLS. The added metadata test covers full-width descriptors
and signed signal values. Testing here uses GIL-enabled CPython on Linux;
Windows and free-threaded performance have not been measured.

With `RSLOOP_USE_FAST_STREAMS=0`, the compatibility and TLS suites also pass
(56 passed, 1 skipped), exercising the generic stream path affected by boxing.

```bash
cargo test --lib --locked
.venv/bin/maturin develop --release
.venv/bin/python -m pytest -q tests/test_*.py
cargo check --all-features --locked
cargo rustc --lib --locked -- -Zprint-type-sizes > target/type-sizes.txt 2>&1
```

The final command needs nightly Rust. For committed revisions, use the isolated
build and paired comparison workflow in [hotpath-lab.md](hotpath-lab.md), with
`58e30f4` as baseline and this change as candidate. This experiment used normal
`maturin develop --release` builds copied into separate package directories,
then the existing lab comparison runner. Each package manifest records its
file hashes, toolchain, baseline revision, and candidate source diff. These
manifests support comparison; they are not full `hotpath_lab.py build` artifacts
with compiler IR and archived source.

Local packages, type reports, test logs, comparison plans, raw paired samples,
and benchmark-source snapshots are under `target/pyo3-layout-experiment/`.

```bash
.venv/bin/python scripts/hotpath_lab.py compare \
  --baseline target/pyo3-layout-experiment/baseline \
  --candidate target/pyo3-layout-experiment/candidate \
  --workloads callbacks,tasks,tcp_streams,http_keepalive --primary callbacks \
  --suite training --blocks 12 --out target/pyo3-layout-experiment/paired-training
.venv/bin/python scripts/hotpath_lab.py compare \
  --baseline target/pyo3-layout-experiment/baseline \
  --candidate target/pyo3-layout-experiment/candidate \
  --workloads callbacks,tasks,tcp_streams,http_keepalive --primary callbacks \
  --suite holdout --blocks 12 --seed 2718 \
  --out target/pyo3-layout-experiment/paired-holdout
```

Choose new output paths when repeating a comparison; the runner deliberately
refuses to overwrite previous results.
