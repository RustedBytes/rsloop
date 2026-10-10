# Stream reader cancellation (issue #106)

## Reproduction and cause

Issue: <https://github.com/RustedBytes/rsloop/issues/106>

The baseline is `f16e637923ec4c17f8acb60509ecf8cb0447c44c` (rsloop 0.1.60).
Both the published wheel and a local release build reproduced the stale-waiter
`ValueError` after `asyncio.timeout(0)`. The source baseline also failed the TCP
reproduction and redis-py/hiredis integration test. Standard asyncio and uvloop
passed the same TCP/Redis scenarios.

`ReadWaiter` retained a cancelled Future. A subsequent pending read was rejected
as concurrent. Worse, a later feed could consume bytes for that cancelled Future
and then ignore the failed `set_result`. `readexactly()` also held already-consumed
bytes in its private, partially initialized accumulator.

## Change

- Register a done callback only for pending native reads. It weakly references the
  reader, and checks Future identity before cleanup, so a delayed callback cannot
  clear a replacement waiter or keep an abandoned reader alive.
- Also reconcile cancellation before read buffer inspection and before processing
  data, EOF or errors. Done callbacks are deferred; feeds and retries can occur
  before they execute.
- Restore only the initialized `readexactly()` accumulator prefix, before newer
  unread bytes. No uninitialized portion is exposed. Reapply flow control after
  restoring the buffer; a subsequent large pending read resumes the transport.
- Keep active concurrent-read rejection and the immediate buffered-read awaitable.
  The existing test that expected cancelled exact reads to discard bytes now
  requires preservation.

Cleanup uses the reader's exclusive PyO3 borrow and runs on the owning loop,
under asyncio's existing thread-affinity contract. This does not make asyncio
Futures or readers safe for arbitrary cross-thread operations.

## Regression coverage

`tests/test_stream_cancellation.py` compares stdlib and native readers on asyncio,
rsloop and uvloop. It covers zero-timeout cancellation, repeated task cancellation,
retry without incoming data, active concurrent readers, data/EOF/error before the
queued cleanup callback, partial exact/until reads, stale callbacks, buffer flow
control, weak-reference lifetime and an event-controlled TCP reproduction.
`readline()` and unbounded `read()` also exercise the shared waiter lifecycle.
The Rust buffer property test now includes prepend operations against a Vec model.

`tests/test_stream_redis.py` starts a private temporary Redis server and explicitly
selects `_AsyncHiredisParser`. It checks repeated SET/GET, transactional and
nontransactional pipelines, single-connection reuse, a direct destructive-read
probe and PING. It skips when optional dependencies or redis-server are missing.
A dedicated Linux CI job installs these dependencies and runs both test modules.

## Local validation (2026-10-10)

Linux x86_64; CPython 3.14.7; Rust nightly-2026-09-25
(`rustc 1.100.0-nightly f7575a9da`); release builds with the committed lockfile.
Redis 7.2.5, redis-py 5.3.1, hiredis 3.4.2, uvloop 0.23.0.

| Check | Result |
| --- | --- |
| Cancellation + Redis + existing stream reader tests | 226 passed |
| Full default pytest selection | 453 passed, 3 skipped, 2 failed, 98 deselected |
| Rust default library tests | 319 passed, 15 failed |
| Rust all-features library tests | 415 passed, 19 failed |
| Clippy, all targets/all features, warnings denied | Passed |
| rustfmt, Ruff on added Python files, focused Pyright | Passed |

The two Python failures are Unix-socket operations rejected with `EPERM`; both
were separately reproduced with the source baseline's native extension. Every
Rust failure was an explicit io_uring driver construction rejected with `EPERM`,
including the four additional filesystem tests in the all-features run. These
full-suite runs **did not pass**. Successful network tests used the Linux
mio/epoll fallback; the build-time `build_info()['reactor']` label is not evidence
of active io_uring use.

Actual io_uring operation, macOS, Windows, other CPython versions and free-threaded
Python were not validated locally. CI results must be checked separately.

Commands (optional Redis executable can be selected with `RSLOOP_REDIS_SERVER`):

```sh
maturin develop --release --locked
python scripts/generate_test_tls_certs.py
python -m pytest tests/test_stream_cancellation.py tests/test_stream_redis.py tests/test_stream_reader.py -q
python -m pytest -q
python scripts/run_rust_tests.py
python scripts/run_rust_tests.py --all-features
cargo clippy --all-targets --all-features --locked -- -D warnings
cargo fmt --check
ruff check tests/test_stream_cancellation.py tests/test_stream_redis.py benches/stream_read_lifecycle.py
pyright --pythonpath .venv/bin/python tests/test_stream_cancellation.py tests/test_stream_redis.py benches/stream_read_lifecycle.py
```

## Performance

Both extensions were compiled locally in release mode with the same interpreter,
toolchain, lockfile and default features. The lifecycle benchmark uses 64-byte
records, validates every result and includes done-callback dispatch for waiting
reads. A waiting `read()` round contains two reads; exact/until rounds contain
one read fed in two fragments. These are synthetic per-round costs, not
application request latency.

An initial unpinned run was noisy. The reported lifecycle run pins subprocesses
to CPU 4 and alternates baseline/fix/fix/baseline, with 250,000 rounds and nine
samples per invocation (18 samples per variant), after a 1,000-round warmup.
Medians in nanoseconds per round:

| Workload | Baseline | Fix | Change |
| --- | ---: | ---: | ---: |
| read-buffered | 298.3 | 287.6 | -3.6% |
| readexactly-buffered | 316.7 | 289.0 | -8.7% |
| readuntil-buffered | 355.1 | 337.3 | -5.0% |
| read-waiting | 1701.5 | 2543.2 | +49.5% |
| readexactly-waiting | 1215.5 | 1642.3 | +35.1% |
| readuntil-waiting | 1180.5 | 1677.6 | +42.1% |

Pending reads pay for cancellation checks and a scheduled completion callback.
That overhead is measurable and this is **not a performance improvement**.
Buffered results allocate no callback; differences at that scale are sensitive
to this shared environment and should not be treated as a speedup.

The existing `buffer_paths.py` workload also ran in baseline/fix/fix/baseline
order, with 4,096 rounds, 64 KiB payloads, two warmups and seven samples per
invocation. It was not CPU-pinned. Per-invocation medians (milliseconds):

| Variant | Buffered reads | TCP segmented writes + readexactly |
| --- | ---: | ---: |
| baseline (1) | 35.318 | 86.877 |
| fixed (2) | 33.824 | 80.264 |
| fixed (3) | 34.142 | 75.489 |
| baseline (4) | 34.616 | 72.566 |

The TCP results changed direction across the sequence; this run does not establish
a reliable end-to-end regression or improvement. Recheck on the target hardware
and actual io_uring before drawing a deployment performance conclusion.
Raw samples: [stream-reader-cancellation-bench.json](stream-reader-cancellation-bench.json).

```sh
python benches/stream_read_lifecycle.py --rounds 250000 --repeat 9 --output lifecycle.json
python benches/buffer_paths.py --rounds 4096 --size 65536 --repeat 7 --output buffers.json
```
