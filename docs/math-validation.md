# Time arithmetic and timer invariants

This review uses master `5f5027ccf44b47389290138102f53d572909ed5c`
(after PR #109) as its baseline. Its scope is duration conversion, deadline
saturation, interval phase arithmetic, timer ordering/cancellation and stream
watermark arithmetic. The changes concern executor shutdown and IOCP rounding;
the additional model tests exercise the existing interval and heap algorithms.

## Requirements and independent checks

| Requirement | Oracle / test | Outcome |
| --- | --- | --- |
| A positive finite executor timeout must not panic during conversion | Public Python API at the binary64 predecessor of `2**64`, `2**64` and `sys.float_info.max` | Baseline: one pass, two `RustPanic` failures. Fixed: all pass. |
| An oversized timeout must remain cancellable | Block the shutdown worker with an event, cancel its awaitable, then release and observe worker completion | Pass; one waiting shutdown call, no spurious nonwaiting fallback. |
| Positive finite IOCP waits must not become zero-timeout polls | Integer bounds on every nanosecond from 0 through 2,000,000, plus cap/overflow boundaries | Baseline fails at 1 ns; fixed passes. |
| Interval remainder preserves phase across the full Duration representation | Euclidean bounds and divisibility on 56 boundary pairs; 8,256 whole-period translation cases | Pass, including `Duration::MAX`; zero divisor is excluded by the caller. |
| Indexed heap preserves minimum, tie order and registration identity | Ordered-map oracle on 64 fixed xorshift seeds, 512 operations each | Pass; 32,768 operations with insert, cancellation, stale handles and expiry. |
| Heap/slab indices remain a bijection and parent keys never exceed child keys | Check both directions, size and order after every model operation | Pass. |

The heap oracle uses independent insertion IDs and a `BTreeMap`, rather than
another implementation of the four-ary heap. Each trace ends by draining every
remaining timer and comparing the exact wake order. Seeds are integers 1–64;
deadlines are offsets 0–31 ns, deliberately producing ties. Clock progress does
not determine expected results.

## Numerical risk ledger

**Executor conversion (fixed).** Binary64 can represent finite seconds far
beyond `Duration`'s range. The positive-finite branch now uses fallible
conversion and saturates overflow to `Duration::MAX`. The locked async-io 2
implementation uses `Instant::checked_add` and a never-firing timer when the
deadline cannot be represented. Worker completion and cancellation still win;
this does not stop a running worker thread. Ordinary rounding and existing
None/infinity/nonpositive/NaN policies are unchanged.

**IOCP quantization (fixed).** Let `n` be integer nanoseconds, `q = 1,000,000`
and `M = 2**32 - 2`. The encoded finite timeout is
`m = min(ceil(n / q), M)` milliseconds. For `n <= M*q`,
`n <= m*q < n + q`; positive inputs therefore stay positive. Above the cap,
the wait is deliberately shortened to `M` ms and the scheduler must recheck
the actual deadline. `None` alone uses `2**32 - 1` (INFINITE).
`Duration::MAX.as_nanos() < 2**94`, so the calculation fits `u128`.
The conversion remains allocation-free and constant-time.

[GetQueuedCompletionStatusEx documentation](https://learn.microsoft.com/en-us/windows/win32/api/ioapiset/nf-ioapiset-getqueuedcompletionstatusex)
defines zero as an immediate timeout and INFINITE as an unbounded wait. Rounding
up avoids converting a requested short sleep into polling. The less-than-1-ms
bound describes conversion error, **not** Windows wake latency: OS scheduling
can add delay. No Windows CPU or latency benchmark was run for this review.

**Interval arithmetic (verified cases, no production change).** A remainder
`r` is uniquely characterized by `0 <= r < p`, `r <= d` and divisibility of
`d-r` by positive `p`. Adding whole periods preserves phase. Tests use exact
integer arithmetic; no floating-point epsilon is needed.

**Heap identity (bounded testing).** The model exercises slab reuse and stale
cancellations, not all possible operation traces or `u64` generation wraparound.
Passing these checks is not a general proof of concurrency or memory safety.

## Executed validation

Linux x86_64, CPython 3.14.7, Rust nightly-2026-09-25:

- Release extension build, root and runtime-harness Clippy with all targets/all
  features and warnings denied, Rust formatting and changed Python lint/format:
  passed.
- `python scripts/run_rust_tests.py`: 331 passed, 15 failed.
- `python scripts/run_rust_tests.py --all-features`: 427 passed, 19 failed.
  All failures report EPERM from unavailable io_uring/Unix-socket operations.
- Release Python suite with a local Redis server: 477 passed, 2 Unix-socket
  EPERM failures, 3 skipped. Redis/hiredis tests ran on asyncio, uvloop and rsloop.
- Tooling suite: 98 passed. Focused Pyright on `python/rsloop`, `tests/typing`
  and `tests/test_executor_timeout_bounds.py`: no errors.
- Hotpath audit: 2079 definitions, zero missing hooks; its three tests passed.

Full Rust/Python suites are not green in this restricted environment. The
Python skips require Winsock, free-threaded Python and a profile extension.
Windows, macOS, free-threaded Python and a working io_uring backend were not
executed locally. The IOCP arithmetic itself is compiled and tested on Linux
through its existing `cfg(any(windows, test))` configuration.

## CI follow-up: free-threaded stream connection handoff

The initial PR CI failed on Linux / Python 3.15t in
`test_close_flushes_coalesced_server_writes`. The smol worker assembled the
`open_connection` result by borrowing its protocol while the event loop could
hold a mutable borrow during data/EOF delivery (`PyBorrowError` in
`fast_open_connection_result`). This was reproduced locally on CPython
3.15.0rc3 free-threaded, with `PYTHON_GIL=0`.

Result assembly now follows a direct Python await on the calling loop. It
cannot run concurrently with that loop's protocol callbacks, and cancellation
propagates directly to `create_connection`. A deterministic event-based test
checks cancellation reaches the inner coroutine; another checks thread, loop
and context identity after completion from a worker thread. A socket stress
test checks immediate peer data/close during the handoff.

On the fixed free-threaded debug extension, 100 stress iterations (12,800
connections) passed. The local full suite had 416 passes, 2 Unix-socket EPERM
failures and 68 skips (optional uvloop/Redis dependencies, Winsock and profile
build). These checks supplement the earlier GIL-enabled validation above;
stress success does not constitute a proof for arbitrary interleavings.
