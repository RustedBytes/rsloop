# Handle allocation experiment

The [hotpath-rs profile](hotpath-rs-profile.md) identified callback scheduling
as the largest Rust-side cost in a burst of 200,000 callbacks. `PyHandle` had a
PyO3 freelist of 8,192 entries. During a large pre-scheduled burst, all handles
remain live until the loop drains them, so the freelist is normally empty when
each handle is created. This experiment removes the freelist while preserving
the `Handle` type, its weak references, and cancellation behavior.

## Measurements

Baseline is commit `e8b686e` with the freelist. Candidate is that commit plus
the `PyHandle` attribute change in this branch. Both were uninstrumented release
builds made with `.venv/bin/maturin develop --release` on an Intel Core i9-9900K,
Linux x86-64, CPython 3.14.7, and Rust 1.100.0-nightly. The callback, task,
and TCP benchmark used nine fresh measured processes after two warmups per
build. Two baseline/candidate build cycles were run in sequence. Values below
are medians of all 18 measured processes per variant; lower is better.

| Workload | Baseline | Candidate | Change |
| --- | ---: | ---: | ---: |
| 200,000 callbacks | 49.19 ms | 46.10 ms | -6.3% |
| 50,000 tasks | 92.47 ms | 91.19 ms | -1.4% |
| 5,000 TCP roundtrips, 1 KiB | 80.90 ms | 77.77 ms | -3.9% |

A separate 50,000-task run with batches of only 100, intended to exercise
handle reuse, took 84.94 ms with the baseline and 85.52 ms with the candidate
(+0.7%). One sustained seven-run network matrix gave these medians:

| Scenario | Baseline | Candidate | Change |
| --- | ---: | ---: | ---: |
| HTTP keep-alive | 142.95 ms | 138.33 ms | -3.2% |
| Mixed streams | 184.41 ms | 177.31 ms | -3.8% |
| Bulk transfer | 18.66 ms | 18.31 ms | -1.9% |

The two build cycles replicated the callback gain: baseline medians were
48.81 and 49.31 ms, while candidate medians were 46.42 and 45.91 ms. These
are local, sequential measurements, not paired A/B blocks. The smaller network
differences may reflect host drift; the clearest supported conclusion is the
callback-burst improvement. No other code paths were changed.

The Rust library suite passed (307 tests), as did the Python core suite (215
passed, 2 skipped), including recycled-handle weak-reference and cancellation
checks. Both the normal release build and the all-features check passed.
