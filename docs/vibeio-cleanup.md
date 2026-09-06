# Embedded vibeio cleanup

Status: in progress. A clean Clippy run alone does not prove that the embedded
runtime is safe or that the Qualirs review is finished.

## Completion requirements

- Review every remaining vibeio Qualirs finding against its actual code and
  platform configuration. Fix defects; record specific evidence for false
  positives rather than disabling entire safety rules.
- Audit unsafe buffer, pointer, pinning, waker, and FFI contracts, including
  operation cancellation and runtime shutdown. Add regression tests for defects.
- Replace unnecessary unsafe code and narrow remaining unsafe operations with
  accurate local safety explanations. Reassess the module-wide Rust allowances.
- Check dormant feature modules as well as the default rsloop build. Document
  which embedded features are actually supported and remove stale upstream
  installation/feature claims.
- Verify Linux, Windows, and macOS code in proportion to the changed paths;
  distinguish compilation from platform execution and unavailable checks.
- Keep formatting, all-target/all-feature Clippy, and Rust tests passing; check
  relevant Python compatibility and performance when runtime behavior changes.

## Current inventory

### AFD setup ownership review

- Documented NtCreateFile's counted UTF-16 name/object-attribute lifetimes and
  local writable outputs. Its successful non-null handle is immediately wrapped
  in OwnedHandle before association/notification setup can fail.
- Documented that CreateIoCompletionPort associates two live owned handles but
  returns the existing port, not a new owner. Notification setup skips only
  event signaling, not successful completion packets needed for retirement.
- Added a Windows-only test verifying lazy AFD setup, cached handle reuse and
  non-inheritance. It cross-compiles; native execution and setup-failure injection
  remain unverified. No production behavior change is claimed.
- The whole-Windows unsafe-comment probe now reports five sites (four production,
  one test) in completion association/disassociation, packet batching, cancellation
  and the modeled-packet test. The Windows-wide gate remains pending that review.
  Native create/open API reference:
  https://learn.microsoft.com/en-us/windows/win32/api/winternl/nf-winternl-ntcreatefile

### IOCP socket-provider resolution

- Base-socket IOCTL results now require a full SOCKET-sized output and a value
  other than INVALID_SOCKET. Added local comments for synchronous stack outputs
  and thread-local WSAGetLastError. The returned handle remains borrowed from the
  caller's socket; no new owning wrapper is constructed.
- Factored provider-chain traversal into a Windows/test helper. It now detects
  multi-node cycles as well as self-loops, preventing a malformed provider from
  spinning forever during registration. The direct base-handle success path
  performs no Vec allocation; fallback tracks previously visited handles.
- Linux-executed scripted tests cover direct and multi-hop success, cycles of
  lengths 1/2/3/32, invalid returned handles, and preservation of provider errors.
  Windows-only IPv4/IPv6 live lookup and invalid-socket tests cross-compile but
  remain unexecuted. No real third-party provider malfunction is claimed.
- API reference for handle queries and synchronous SIO_BSP_HANDLE_POLL:
  https://learn.microsoft.com/en-us/windows/win32/winsock/winsock-ioctls

### IOCP error encoding boundary

- Replaced IOCP completion error narrowing/unchecked negation with checked
  positive-u32 to negative-i32 encoding. Representable nonzero error codes are
  preserved; zero or values above i32::MAX map to ERROR_ARITHMETIC_OVERFLOW.
  The previous fallback to negating raw NTSTATUS could yield a positive count
  or overflow for synthetic edge inputs. No native occurrence is claimed.
- A Linux-executed pure conversion test covers normal errors, the signed boundary,
  zero and oversized values, including round trips through completion_error.
  Added Windows-only native mapping cases for cancellation, EOF and invalid
  handles; these are cross-compiled, not executed here. Documented both remaining
  integer-only RtlNtStatusToDosError call sites locally.
- Microsoft documents ERROR_MR_MID_NOT_FOUND for unmapped NTSTATUS values; the
  ordinary positive fallback remains preserved:
  https://learn.microsoft.com/en-us/windows/win32/api/winternl/nf-winternl-rtlntstatustodoserror
  This hardening does not resolve native IOCP packet/lifetime validation.

### IOCP finite-timeout sentinel fix

- IOCP converted finite durations at or above u32::MAX milliseconds into the
  Windows INFINITE sentinel. Extracted the conversion into a Windows/test helper
  and capped finite waits at u32::MAX-1; only None produces INFINITE. Existing
  sub-millisecond truncation is unchanged. Very long finite waits can return
  early for the runtime to reconsider its deadline, rather than never timing out.
- A portable regression reproduced the wrong sentinel before the fix and passes
  afterward. It checks None, zero, one millisecond, the finite/sentinel boundary,
  u64::MAX milliseconds and Duration::MAX without waiting for those durations.
  Microsoft API semantics:
  https://learn.microsoft.com/en-us/windows/win32/api/ioapiset/nf-ioapiset-getqueuedcompletionstatusex
- Reviewed/commented completion-port creation, checked OwnedHandle acquisition
  and wake-packet posting while an upgraded Arc holds the port alive. Remaining
  IOCP unsafe sites and native Windows execution are still open; this does not
  enable the Windows-wide unsafe-comment gate yet.

### Package-level Unix unsafe-comment gate

- Completed the local-comment review of Unix-stream scalar/vectored writes and
  enabled that module's lint. Like TCP/pipe adapters, their source slices stay
  borrowed through synchronous poll-only dispatch and local operation teardown.
- Removed manual unsafe copy/slice construction from AsyncWrap test fixtures by
  reusing read_into_buf/iobuf_to_slice; those helpers now compile under cfg(test)
  even when optional I/O features are disabled. Documented the integer-only
  statx_timestamp test initialization. AsyncWrap's module lint now passes too.
- Whole-harness all-feature/all-target undocumented_unsafe_blocks checks pass
  on Linux and macOS ARM64. Enabled the package-level gate on Unix. Windows still
  exposes 16 IOCP library/test sites and is not covered by the package-level gate
  yet; existing module-local Windows gates remain enabled. No blanket suppression
  was added. Probe logs: target/vibeio-cleanup-unsafe-lint-{linux,macos,windows}.log.
- Linux tests: 257 all-feature and 169 no-default-feature tests, plus 16
  documentation checks in each configuration, pass. This establishes comment
  coverage, not complete unsafe-code soundness or native macOS/Windows validation.

### UDP borrowed polling coverage

- Reviewed all six raw temporary-buffer constructors in PollUdpSocket. Each
  operation is local to the poll, uses poll_op_poll rather than completion
  submission, and keeps the caller's initialized read-only or exclusive writable
  borrow for that call. Added direct safety comments and enabled the module's
  undocumented_unsafe_blocks lint; no production behavior changed.
- The existing poll-socket test actually used owned-buffer async methods; renamed
  it to make that scope explicit. Added direct poll_send, poll_send_to, poll_recv,
  poll_recv_from, poll_peek and poll_peek_from coverage. The regression verifies
  Pending does not modify the buffer, later reuse works, peeks preserve the
  datagram, empty datagrams are consumed exactly once, and unused buffer suffix
  bytes remain intact after a fresh receive. Linux execution passed.
- All-feature Linux harness: 257 tests and 16 documentation checks pass. Native
  Windows and macOS execution remains outstanding; cross-Clippy is compilation.

### TCP borrowed-buffer contracts

- PollTcpStream::peek now constructs its temporary wrapper and RecvOp inside
  each poll closure, matching the wrapper's poll-only lifetime contract. The
  previous future retained the operation across polls even though the caller's
  mutable borrow stayed live; no observed use-after-free is claimed.
- Added direct safety comments to peek and scalar/vectored writes, and enabled
  undocumented_unsafe_blocks for the TCP stream module. All borrowed-buffer
  operations explicitly use poll_op_poll, which rejects completion submission.
- New loopback regression polls peek to Pending, drops it, reuses the caller's
  buffer, then verifies incoming data can be peeked and subsequently read without
  consumption by the cancelled operation. It accepts valid short peeks and
  verifies untouched bytes beyond the reported count. Linux execution passes;
  Windows/macOS cross-checks do not constitute native lifecycle validation.

### Borrowed pipe write audit

- Reviewed PollPipe scalar/vectored writes: the borrowed data remains valid
  throughout each synchronous poll, WriteOp/WritevOp only read it, and
  poll_op_poll rejects completion-based handles. Local operations are destroyed
  before returning, including Pending; readiness registration retains the waker,
  not the temporary caller buffer. Added local safety comments and enabled
  undocumented_unsafe_blocks for the pipe module.
- A native Linux backpressure test fills a pipe, obtains Pending from scalar and
  vectored writes, mutates/drops the original buffer, drains and verifies the
  previously written bytes, then succeeds with fresh vectored data. It passed;
  no production behavior change is claimed. This tests data/lifetime behavior,
  not every possible native readiness race or platform driver.
- Linux harness: 255 tests and 16 documentation checks pass. Linux strict Clippy
  passes; macOS-target Clippy checks the Unix-only module without native execution.

### Unsafe-comment gate follow-up

- A temporary package-wide undocumented_unsafe_blocks check exposed 21 Linux
  all-feature library/test sites, including comments separated from their unsafe
  block and borrowed polling-buffer adapters still needing local review. The
  package-wide gate was not retained or replaced with blanket suppressions.
- Removed raw-slice construction from fs::read: its returned buffer is a fully
  initialized fixed array, so ordinary bounded slicing preserves the existing
  clamp and byte-copy behavior. No unsafe operation or IoBuf import is needed.
- Enabled the lint locally in the io_uring driver and StatxOp after reviewing
  eventfd ownership, SQE copying versus referenced allocation retention, and
  successful statx initialization. Moved existing comments directly above the
  corresponding unsafe calls and clarified the local contracts. Driver shutdown
  retention remains an explicit broader proof obligation, not solved by comments.
- Linux all-feature strict Clippy and 254 harness tests plus 16 documentation
  checks pass. Remaining polling-buffer sites and Windows-only contracts still
  require review before a package-wide unsafe-comment gate is justified.

### Dead-code checks in the standalone harness

- Removed vibeio's package-wide dead_code allowance. The private Python embedding
  and two harness-free benchmark module declarations retain documented allowances
  because they intentionally consume only a subset of the API. The standalone
  public-API harness now checks dead code throughout the runtime.
- Added one local exception for Runtime::poll_once, whose production caller is
  the Python loop, outside the standalone harness. Removed the stale
  private_interfaces suppression from AsInnerRawHandle.
- The stricter check exposed over-broad compilation gates. Limited the
  AnyDriver ignore-completion wrapper to non-Windows (Windows uses cancel),
  positional_offset to Linux fs, raw nonblocking setup to its feature/test users,
  and completion-length/address helpers to Linux/Windows or tests.
- All 24 isolated strict Clippy combinations pass: no features plus fs, process,
  signal, pipe, stdio, splice and blocking-default, on Linux GNU, Windows GNU and
  macOS ARM64. Output: target/vibeio-cleanup-lint-matrix-current.log. Root default
  and all-feature all-target strict Clippy pass; the Linux harness passes 254
  tests and 16 documentation checks. Cross-checks are not native execution.

### Integration refresh after cancellation-retention changes

- Rebuilt and installed the default-feature release extension with
  `.venv/bin/maturin develop --release --locked` (CPython 3.14, rsloop 0.1.48;
  optimized compilation 38.23 seconds). Build output is in
  `target/vibeio-cleanup-rebuild-current.log`.
- The rebuilt binary completed `scripts/run_python_tests.py`: 109 tests in
  3.770 seconds, two skips, no failures. Output is in
  `target/vibeio-cleanup-python-current.log`.
- All 13 workload_matrix scenarios completed for rsloop with one warmup and one
  measured run, CPUs 2,3, and five idle cycles plus one idle warmup cycle. Raw
  results and output are in `target/vibeio-cleanup-matrix-smoke.json` and `.log`.
  This is a short functional smoke check, not a matched performance comparison,
  idle-stability result or evidence of improvement over uvloop. Previous matrix
  comparison artifacts remain separate; README performance claims are unchanged.
- This refresh validates the Linux Python integration. Optional embedded-feature
  coverage and native Windows/macOS lifecycle proof remain separate requirements.

### Flat repeated-cancellation retention

- Replaced the initial nested boxed-pair grouping with a shared flat retention
  list used by io_uring and IOCP. Repeated cancellation no longer builds a
  recursively dropped ownership chain. First cancellation still stores the
  original box directly; subsequent calls keep each payload box stable in a Vec.
- A portable 100,000-payload stress test verifies that the first allocation's
  address is unchanged, none are dropped early, and all are released at final
  retirement. Existing unknown-token reentrancy and repeated-retention tests
  remain passing. This is a robustness follow-up, not a measured speedup.
- Linux harness: 254 tests and 16 documentation checks pass. Strict harness
  Clippy passes on Linux and Windows/macOS cross-targets; native Windows/macOS
  driver execution remains outstanding.

### Cancellation retirement edge cases

- io_uring's unknown-token ignore path dropped caller storage under the mutable
  state borrow. It now returns a synthetic retired completion so the existing
  outer cleanup drops the payload after releasing the borrow. A destructor probe
  using a live driver failed before the change and passes afterward.
- Repeated ignore/cancel calls previously replaced and dropped the first retained
  payload before acknowledgement. io_uring and IOCP now group old and new owners
  until completion retirement. The ordinary first-ignore path adds no allocation;
  grouping allocates only for a repeated call. This hardens the edge-case API
  contract; no normal-operation duplicate-cancellation trace is claimed.
- Linux state tests reproduce early release before the fix and verify both
  owners survive until retirement afterward. The equivalent IOCP state test is
  cross-compiled, not native-executed. IOCP already retired unknown-token payloads
  outside its state borrow, so only repeated retention needed changing there.
- Linux harness: 253 tests and 16 documentation checks pass; root strict Clippy
  passes. Windows strict harness cross-Clippy checks the IOCP change. Broader
  kernel cancellation/shutdown acknowledgement proofs remain open.

### Positioned-read completion coverage

- Extended the same sparse-file test to ReadAtOp rather than duplicating scratch
  setup. At each offset, it checks spare-capacity initialization, EOF clearing a
  populated Vec, zero-capacity success and shared cursor preservation. It also
  checks unchanged buffers after invalid offsets and an actual EBADF completion
  from reading a write-only descriptor.
- Reviewed the ReadFile Q0095 site and `set_buf_init` ordering in the findings
  ledger. The test executed on Linux; Windows EOF and IOCP cancellation semantics
  remain native-validation gaps. No production behavior change is claimed.

### Positioned-write audit and live validation

- Reviewed WriteAtOp's Windows WriteFile unsafe scope, initialized-prefix length,
  stable buffer retention, cancellation handoff and offset-word assignment.
  Added a specific finding disposition without suppressing the rule; native
  Windows completion acknowledgement remains part of the open IOCP audit.
- A real Linux io_uring test now checks offsets 0, 4097 and 2^32+3, sparse file
  length/data, unchanged shared cursor and returned buffer. It also checks that
  signed-range violations/append-sentinel offsets are rejected before submission.
  The test permits legitimate short writes and unlinks its exclusively created
  scratch file before testing, ensuring cleanup on assertion failure.
- The targeted test executed successfully on this host. This adds evidence for
  existing behavior; no production change or performance improvement is claimed.

### ConnectEx bind error handling

- Windows connect setup no longer treats WSAEADDRINUSE as successful binding.
  Only WSAEINVAL retains the documented already-bound behavior; other errors
  preserve their native error code. `completion_bound` is set only after this
  check succeeds, so a bind conflict does not advance to ConnectEx submission.
- Microsoft documents WSAEINVAL for an already-bound socket, but WSAEADDRINUSE
  for an address conflict. ConnectEx requires a previously bound socket:
  https://learn.microsoft.com/en-us/windows/win32/api/winsock/nf-winsock-bind
  https://learn.microsoft.com/en-us/windows/win32/api/mswsock/nc-mswsock-lpfn_connectex
- Added Windows-only tests for the error policy and actual IPv4/IPv6 wildcard
  binding, preservation of the assigned port on repeated setup, and invalid
  socket rejection. Strict Windows cross-Clippy compiles these tests; they were
  not executed here. No live ephemeral-port-exhaustion reproduction is claimed.
  The Linux suite still passes (250 tests and 16 documentation checks).

### Refreshed findings and connect coverage

- Current vibeio Qualirs inventory: 215 diagnostics (90 Q0087, 63 Q0090,
  44 Q0095, 18 other), versus the previous 220 snapshot. Rules remain enabled.
  Added per-location dispositions for the five ConnectOp Q0095 blocks; each
  encloses a single FFI call. The ledger distinguishes those bounded calls from
  the still-open Windows completion lifetime and exceptional-bind review.
- Expanded live connect validation to IPv4 and IPv6 for ordinary and poll-mode
  streams under both Mio and io_uring. All eight connections executed successfully
  on Linux; address/move/cancellation tests also pass. Replaced the unsafe IPv4
  test fixture initialization with the existing socket-address constructor.
- This is additional audit and test coverage, not a production connect change or
  a native Windows/macOS lifecycle proof. Full cleanup remains in progress.

### Unix bind without a runtime

- `UnixListener::bind` now rejects a missing runtime before the standard bind
  creates a filesystem socket. Previously it returned NotConnected after creating
  the pathname, so retrying in a valid runtime could fail with address-in-use.
- The regression failed before the fix and passes afterward: no pathname is
  created, an existing regular file's contents survive, and binding the same
  scratch path subsequently succeeds inside a Mio runtime. Its cleanup is scoped
  to the test-owned directory and socket/file, with no recursive deletion.
- This is not a general bind rollback guarantee. Later registration or setup
  errors may still leave the pathname; docs now state this and that bind is
  synchronous. No automatic unlink is added, since a replacement pathname or a
  listener passed to from_std must not be removed as an error-cleanup side effect.
- Linux all-feature harness: 250 tests and 16 documentation checks pass. Linux
  and macOS-target strict harness Clippy pass; native macOS remains unverified.

### Owned accept-operation results

- TCP accept now returns an owned platform socket with its peer address; Unix
  accept returns `OwnedFd`. Poll accept, io_uring accept and Windows AcceptEx
  retain ownership through the operation result instead of releasing a raw
  handle. TCP/Unix listener callers use safe standard-library conversions.
  Existing high-level callers already claimed raw ownership immediately; the
  defect was leaking a discarded successful low-level result.
- New TCP and Unix poll-path discard regressions both failed before the change
  and pass afterward. A separate real io_uring regression verifies peer EOF
  after discarding TCP multishot and Unix single-shot accept results. It executed
  successfully on this Linux host, without an unavailable-io_uring skip.
- Linux all-feature harness: 249 tests pass. Windows/macOS all-feature strict
  cross-Clippy passes; native Windows AcceptEx execution remains unverified.
  No speedup is claimed. Broader ownership and teardown review remains open.

### Owned open-operation results

- Linux `OpenOp` now returns `OwnedFd`, not a bare descriptor. The driver-to-op
  ownership transfer is documented at the checked successful completion; the
  filesystem caller converts it safely to `std::fs::File`. Discarding a result
  now closes it automatically. The existing high-level caller already acquired
  ownership immediately; this fixes the lower-level operation's discard path.
- A real io_uring regression opens a pipe writer through `/proc/self/fd`, drops
  the original writer and operation, verifies the result keeps the writer alive,
  then drops the result and requires EOF from a nonblocking reader. It failed
  before the change with WouldBlock and passed afterward on this Linux host.
  Unsupported io_uring environments explicitly report the unavailable check.
- Validation: 363 root tests, 246 all-feature harness tests and 16 documentation
  checks pass, as do root and harness strict Clippy. This is an ownership fix,
  not evidence of a wall-clock improvement. Broader cleanup remains in progress.

### Refreshed isolated-feature execution

- All 24 strict Clippy combinations pass: no features and each of fs, process,
  signal, pipe, stdio, splice and blocking-default on Linux, Windows GNU and
  macOS ARM64 targets. Windows/macOS checks remain compilation, not execution.
- Running the Linux feature tests exposed 12 fs-only failures previously hidden
  by all-features tests: test runtimes assumed blocking-default supplied a pool.
  Filesystem success tests now configure a small explicit test pool; the two
  Windows-only filesystem fixtures use it too. Production feature dependencies
  and no-pool error policy remain unchanged. The fs-only suite now passes 196
  tests, including those 12 formerly failing tests.
- All eight Linux configurations now execute successfully: none 159, fs 196,
  process 173, signal 171, pipe 163, stdio 165, splice 173, blocking-default 161.
  Each also passes 8 documentation checks. The fs-only Clippy checks were rerun
  successfully on all three targets after the fixture fix.
- The native CI isolated-feature loop now runs cargo test as well as Clippy.
  This workflow change is uncommitted and has not been dispatched; native
  Windows/macOS results remain outstanding.

### Integration after self-cancellation fix

- Rebuilt and installed the default-feature release extension with maturin
  develop --release --locked (37.67 seconds compilation). Environment: Linux
  x86_64, CPython 3.14.0, rsloop 0.1.48, uvloop 0.22.1. Python suite passed:
  109 tests in 3.320 seconds, with two skips.
- Re-ran all 13 workload-matrix scenarios for rsloop and uvloop on CPUs 2,3.
  All 26 loop/scenario records contain three measured runs and completed without
  a benchmark failure. This is an integration smoke run, not a matched
  before/after performance experiment.
- Idle v2 completed all six fresh-process runs (three per loop), each with
  100 measured cycles and five warmup cycles at 0.2 seconds idle. The observed
  paired latency difference was **+35.6% for rsloop versus uvloop**. The harness
  labels it inconclusive because fewer than seven process runs provide no
  confidence interval; this result neither establishes a scheduler regression
  nor supports a speedup claim. Sustained paired measurements remain necessary.
- Current artifacts (ignored, overwritten by future refreshes):
  target/vibeio-cleanup-rebuild-current.log,
  target/vibeio-cleanup-python-current.log,
  target/vibeio-cleanup-matrix-current.log and the matching .json file.
  Earlier sections below retain historical results. No README speed claims
  were changed, and native Windows/macOS validation remains outstanding.

### Owned multishot accept queue

- io_uring's accept queue now stores Result<OwnedFd, i32> instead of raw signed
  integers. Successful CQEs acquire descriptor ownership at the completion
  boundary; stale/undelivered results close by normal drop. Queue destruction
  closes only unconsumed successes, without a manual close loop. Returning an
  accepted result explicitly transfers raw ownership to the caller.
- Added a socket-pair regression mixing successful and error queue entries.
  Dropping the queue closes its abandoned endpoint (peer observes EOF), leaves
  a transferred endpoint alive (peer observes WouldBlock), and closing the
  transferred owner then yields EOF. Existing extreme-error dispatch tests pass.
- Validation: 354 root tests, 237 harness tests, 8 documentation checks, strict
  root/harness Clippy, formatting and whitespace checks pass. This Linux-only
  change has no new native Windows behavior. Queue representation changed;
  no throughput or memory-footprint improvement is claimed. Changes remain
  uncommitted and the broader cleanup remains open.

### Post-ownership-change Python integration

- Rebuilt and installed the release CPython 3.14 extension from the current
  worktree using maturin develop --release --locked. Python compatibility suite:
  109 tests in 3.607 seconds, successful with 2 skips. Logs are in
  target/vibeio-cleanup-rebuild.log and target/vibeio-cleanup-python-current.log.
- Completed the default workload matrix for rsloop and uvloop, pinned to CPUs
  2,3, with raw results in target/vibeio-cleanup-matrix-current.json and the
  matching .log file. This is a three-repeat integration smoke run, not a
  sustained before/after experiment or evidence of a cleanup speedup. Idle v2
  completed all three process blocks per loop, 100 cycles per run, and reports
  an inconclusive comparison (fewer than seven process runs).
- A fresh Qualirs scan reports 242 vibeio diagnostics: 117 Q0087, 63 Q0090,
  44 Q0095 and 18 in the other rules. The disposition document retains its
  earlier snapshot locations and now records this newer count separately.
  Remaining unsafe findings still require per-location review. No README
  performance claims were updated; changes remain uncommitted.

### Safe socket ownership

- TCP/Unix listeners, TCP/Unix streams and UDP sockets now own registration
  handles directly, with declaration order ensuring deregistration precedes
  socket closure. Raw-handle/std conversions destructure safely and release
  registration before transferring ownership. Removed their manual destructors,
  ManuallyDrop and pointer reads. Shared TCP conversion still duplicates the
  descriptor when Arc ownership prevents taking the original socket.
- A live Unix regression covers all five wrapper types plus shared TCP. It
  checks raw descriptor identity where ownership is unique, address validity,
  retained shared TCP ownership after closing the duplicate, and exactly-once
  deregistration for all six registrations. This is conversion/ownership
  coverage, not proof of outstanding-operation cancellation or shutdown safety.
- Validation: 350 root tests, 233 harness tests, 8 documentation checks, strict
  root/all-feature/default-feature harness Clippy and Windows/macOS cross-target
  harness Clippy pass. Formatting and whitespace checks pass. Native Windows
  and macOS execution remain outstanding; changes are uncommitted and the
  package audit remains open.

### Safe pipe conversion and destruction

- Pipe now directly owns its registration handle, declared before OwnedFd so
  normal destruction deregisters before closing. IntoRawFd destructures safely,
  releases registration, then transfers the descriptor. Removed the manual
  destructor, ManuallyDrop and raw pointer reads from production pipe code.
- A live pipe regression transfers both endpoints to standard file ownership,
  checks descriptor identity, writes and reads data through EOF, and verifies
  exactly-once deregistration. Existing readiness/mode-conversion tests pass.
- Validation: 349 root tests, 232 all-feature harness tests, strict root/harness
  Clippy, macOS cross-target Clippy, isolated pipe-feature tests and Clippy,
  formatting and whitespace checks pass. Pipe is Unix-only; macOS compilation
  is not native execution. Changes remain uncommitted and broader cleanup is
  still open.

### Safe file conversion and destruction

- FileIo now directly owns its completion handle. File declares that state
  before the standard file, enforcing deregistration-before-close through normal
  field destruction. into_std destructures the object, drops registration state,
  and returns the file without raw pointer reads or ManuallyDrop. The custom
  unsafe destructor is removed.
- A regression registers two real files with the mock driver. Conversion keeps
  the original descriptor/handle and readable contents; conversion and ordinary
  destruction each release their registration exactly once. This tests ownership
  bookkeeping, not native IOCP cancellation or io_uring shutdown.
- Validation: 348 root tests, 231 harness tests, 8 documentation checks,
  strict root/harness Clippy, Windows/macOS cross-target harness Clippy,
  formatting and whitespace checks pass. Native Windows/macOS execution remains
  outstanding. Changes remain uncommitted; the package audit is still open.

### Child-stream safe conversion and destruction

- ChildIo now owns InnerRawHandle directly. Each child-stream destructor
  replaces its I/O state with Blocking before the standard stream is dropped,
  preserving deregistration-before-close without manual destruction. into_std
  takes the existing Option safely and lets normal destruction deregister it.
  Removed raw pointer reads and all ManuallyDrop uses from process/mod.rs.
- Added live Unix conversion coverage for stdin, stdout and stderr. The mock
  registration ledger records each deregistration exactly once; returned
  descriptors retain their original number and remain valid for duplication.
  The test owns and reaps its child even when an assertion fails.
- Validation: 347 root tests, 230 harness tests, 8 documentation checks,
  strict root/harness Clippy, Windows/macOS cross-target harness Clippy,
  formatting and whitespace checks pass. The live conversion regression ran
  on Linux, not Windows or macOS. Broader cleanup remains open and changes
  remain uncommitted.

### Remaining process ownership handoffs

- ChildStdin flush and Command status/output now use the shared owned-operation
  helper. Removed the remaining duplicated RefCell/mutex take-and-recover paths
  from process/mod.rs. Successful operations and worker errors restore the
  returned object before exposing the result to the caller.
- A real child test enumerates the current test executable without executing
  its tests. It checks status, captured output, and preserved executable/argument
  configuration. The same sequence with a rejecting pool returns errors while
  leaving the command reusable. Shared-helper tests cover worker unwinding;
  this test does not inject a panic into std::process or exercise native Windows
  child-stdin flushing.
- Cancellation semantics are unchanged: dropping the pending wrapper operation
  leaves its inner slot empty while the worker retains the object until it
  finishes. This work does not claim cancellation makes commands reusable.
- Validation: 346 root tests, 229 all-feature harness tests, 162 process-only
  tests, 8 documentation checks, strict root/harness/process-only Clippy,
  Windows/macOS cross-target harness Clippy, formatting and whitespace checks
  pass. Broader audit remains open; changes are uncommitted.

### Child-pipe blocking-buffer handoff

- Child-pipe read/write offloads also took the stream and buffer out of shared
  storage before calling synchronous I/O. They now pass the owned pair through
  blocking::with_buffer, preserving both objects when the worker unwinds.
  The helper and cancellation test are enabled for the isolated process feature.
- Fault injection uses safe Read/Write implementations that mutate a stream
  counter then panic on a real worker thread. Both operations return an error
  with the changed counter, original buffer contents, and original allocation.
  Existing empty-vector and EOF coverage continues to pass.
- Validation: 345 root tests, 228 all-feature harness tests, 161 process-only
  harness tests, 8 documentation checks, strict root/harness/process-only
  Clippy and Windows/macOS cross-target harness Clippy pass. Native platform
  execution is not implied. ChildStdin flush and Command status/output still
  have separate ownership handoffs requiring review. Changes are uncommitted;
  broader package cleanup remains open.

### Shared blocking-buffer handoff

- Filesystem positional read/write offloads had the same take-before-I/O
  pattern as stdio. Both now use a shared blocking::with_buffer helper, as does
  stdio. The worker borrows the buffer; only the resumed caller takes it out.
  File-clone errors still return immediately with the original buffer, and
  file/stdio-specific worker-error messages are retained.
- Added actual offloaded file-read and read-only-descriptor write-error tests,
  checking contents and allocation identity. Added deterministic cancellation
  coverage: dropping a pending caller keeps the buffer alive until the queued
  worker either executes on a real thread or is discarded, then drops it once.
  Existing stdio worker-unwind tests now exercise the shared helper.
- Validation: 344 root tests, 227 harness tests, 8 documentation checks, strict
  root/harness Clippy, Windows/macOS cross-target harness Clippy, and isolated
  Linux fs/stdio feature Clippy pass. Cross-target checks are not native tests.
  This does not establish a latency improvement or complete the package audit.
  Changes remain uncommitted.

### Stdio blocking-buffer ownership follow-up

- Consolidated three stdio buffer-offload implementations into one helper. The
  worker now borrows the buffer inside shared storage instead of taking it out
  before I/O. An unwinding worker leaves the buffer recoverable, including any
  partial mutation; the caller returns the worker error without a second panic.
  Removed the redundant RefCell inside the mutex.
- Fault injection on a real worker thread reproduced the old secondary
  `buf is none` panic after the injected worker panic. The regression now
  returns the original allocation. Additional tests cover missing/rejecting
  pools, successful operations and ordinary I/O errors without losing identity.
  The deterministic test pool joins its worker; this is not a latency test or
  a claim to exercise real terminal input/output. Panic-abort is not recoverable.
- Validation: 342 root tests, 225 harness tests, 8 documentation checks (one
  compile-only), strict root/harness Clippy and Windows/macOS cross-target
  harness Clippy pass. Filesystem helpers have a similar handoff pattern and
  still need the corresponding audit; this does not close all blocking I/O.
  These changes remain uncommitted.

The latest focused review is in [remaining finding dispositions](vibeio-findings.md).
It records 255 current diagnostics and per-location dispositions for 18 findings
outside the three bulk unsafe-analysis rules. The baseline below is historical.

`target/qualirs-cleanup-baseline.json` captures the start of the full cleanup
after the initial buffer/timer fixes. It contains 481 vibeio findings:

| Rule | Count | Review area |
| --- | ---: | --- |
| Q0087 | 341 | Unsafe code safety explanations |
| Q0090 | 61 | Potential mutable aliasing |
| Q0095 | 60 | Scope of unsafe blocks |
| Q0078 | 9 | Blocking work in async functions |
| Q0069 | 2 | Library panics |
| Q0094 | 2 | Unsafe Send/Sync contracts |
| Q0084 | 2 | Blocking channels in async functions |
| Q0074 | 1 | Result mapping |
| Q0085 | 1 | Lock across await |
| Q0080 | 1 | Detached task lifecycle |
| Q0068 | 1 | Ignored result |

Counts are heuristic diagnostics, not confirmed defects or distinct source
locations. Some diagnostics include tests despite `skip_tests`, flag safe
`.as_mut()` chains, or miss existing safety comments. These limitations still
need a traceable per-location disposition; they do not justify claiming a clean
audit. Reports under `target/` are generated local artifacts.

## Verified work before this inventory

- Replaced uninitialized-byte references in AsyncWrap, poll stream adapters,
  and Windows receive/vectored-copy paths; tracked initialized poll-buffer size.
- Made the transport coalescing-cache return best-effort during destruction.
- Made completed timeouts release their future and timer immediately, preserved
  pinned drop, and released stale sleep registrations on completion.
- Linux: 169 Rust tests and all-target/all-feature Clippy passed. Pipe module
  compilation was separately checked. Windows changes have not been executed.

## Next audit areas

1. Vectored buffer ownership and cancellation storage.
2. Executor/waker ownership and driver teardown with outstanding operations.
3. Process reaping, signals, and blocking fallback lifecycle.
4. Remaining FFI/net/filesystem contracts and supported-feature build coverage.
5. Per-finding disposition, final cross-platform and performance verification.

## Vectored-buffer audit progress

- Removed unsafe owned-buffer implementations for raw `libc::iovec`
  collections: these collections do not keep their pointed-to storage alive.
  Replaced them with `Vec<Box<[u8]>>`, migrated the low-level completion test,
  and added compile-time rejection, pointer-stability, and socket-I/O tests.
- Made vectored trait methods required instead of defaulting to panics, and
  made cursor/borrowed-vector internals private to preserve their invariants.
- Audited buffer-module unsafe implementations, documented contracts, removed
  unnecessary uninitialized allocations, and enabled local unsafe-operation
  and undocumented-unsafe checks. This does not complete the other modules.
- Corrected Windows vectored read submission to obtain writable vectors and
  retained Windows staging allocations during cancellation. Both vectored
  operation destructors now use the handle's owning driver rather than the
  currently entered runtime. Windows execution is still outstanding.
- Default Linux tests: 172 passed. All-target/all-feature Clippy passed.
  `target/qualirs-cleanup-current.json` now reports 457 vibeio findings (64
  critical, 392 warning, 1 info); this is not a completed safety audit.

### Newly verified build-coverage gap

An explicit Rust build with the dormant `fs`, `process`, `signal`, `pipe`,
`stdio`, `splice`, and `blocking-default` cfgs fails because `async_channel`
and `rusty_pool` are not declared dependencies. See
`target/vibeio-all-modules-build.log`. None of these features is currently
declared in the rsloop Cargo feature table, so `--all-features` cannot find
this problem. Restoring intentional feature/dependency wiring and testing
these modules remains required work; the default build is not evidence for it.

### Feature-coverage repair

The gap above is now repaired: Cargo declares all seven opt-in features and
their optional `async-channel`, `once_cell`, and `rusty_pool` dependencies.
Default features remain empty. The signal registry keeps its retryable
initialization: standard `OnceLock::get_or_try_init` is not stable on the pinned
toolchain, so replacing it would not compile or preserve behavior without a
separate synchronization design.

- The test runner accepts `--all-features` and `--features`; CI has separate
  default/all-feature Rust test entries. Clippy checks all features with the
  lockfile enforced.
- Removed stale upstream installation claims and the nonexistent `time`
  feature gate that had disabled signal-test timeouts.
- Fixed the default blocking pool's core size exceeding a requested small
  maximum; a single-worker regression test now exercises that configuration.
- All-feature Linux validation: 190 Rust tests passed; Clippy passed across all
  targets. This does not replace the remaining safety or cross-platform audit.
- Default Linux validation: 172 Rust tests passed. Each of the seven features
  passed `cargo check --all-targets --no-default-features --features <name>`
  independently. The Python test runner passed Ruff and the CI YAML parsed.
- Refreshed the Rust example's stale lockfile (which still referenced rsloop
  0.1.38 and the external vibeio dependency); its offline build passed with the
  embedded runtime. The current Qualirs count remains 457 vibeio findings.

## Buffered cancellation audit

- All ten scalar/vectored buffered operation types now retain storage on the
  driver owned by their handle, independent of the currently entered runtime.
  A shared handle helper uses explicit cancellation on Windows and completion
  retention on other platforms. Fsync and splice also use the owning driver.
- `take_bufs` rejects reclamation while an operation has an outstanding
  completion token. Unwinding through this rejection still transfers the
  buffers to the driver rather than freeing kernel-visible memory.
- Ten regression tests each exercise four scenarios: drop or attempted reclaim,
  both outside a runtime and inside a different runtime. A test-only mock driver
  retains the payload until simulated completion acknowledgement; reference
  lifetimes verify that it is neither freed early nor retained afterward.
- Linux suites: 180 default and 200 all-feature tests passed; Clippy passed. These deterministic
  ownership tests do not replace real IOCP/io_uring cancellation execution or
  driver teardown verification. Accept/connect and path-based operations still
  require their separate lifetime audit. Qualirs remains at 457 findings: these
  correctness fixes are not measured by its syntax-based count.

## Path-operation and completed-result audit

- Open, statx, mkdir, unlink, rename, hard-link, and symlink operations now own
  their submitting driver explicitly. They reject polling on another driver and
  retain path allocations (plus statx result storage) until completion is
  acknowledged. Constructors and filesystem callers carry the owner explicitly;
  cancellation no longer depends on thread-local runtime state.
- Seven tests verify both path contents and allocation addresses after dropping
  outside a runtime or inside a different runtime. They also verify rejection
  of the wrong polling driver and absence of a retained driver ownership cycle.
- io_uring registrations now distinguish descriptor-producing results from byte
  counts. Unclaimed successful open/accept results are closed on registration
  removal; a consumer takes the result first to transfer descriptor ownership.
- Cancellation of an already-completed registration now removes it immediately:
  otherwise there is no future CQE to release its retained storage. Five tests
  cover cancellation on either side of completion, claimed/unclaimed descriptors,
  and byte-count results that numerically match an open descriptor.
- Linux: 185 default and 212 all-feature tests passed; all-target/all-feature
  Clippy and formatting passed. These tests use real socket descriptors for
  lifetime checks and modeled driver state, not a live io_uring queue.
- Current Qualirs inventory: 458 vibeio findings (64 critical, 393 warning,
  1 info). In particular it reports the new descriptor cleanup block despite
  its preceding multi-line SAFETY explanation. Per-location false-positive
  disposition remains unfinished; the count is not a measure of these fixes.
- Accept/connect operation storage, error-path descriptor ownership, and real
  driver shutdown remain the next lifecycle audit areas.

## Accepted-socket ownership and platform checks

- Unix TCP/local accept paths now immediately guard returned descriptors with
  `OwnedFd`. Flag setup and peer-address failures close the socket; only a
  successful return transfers ownership. Shared TCP finalization removes
  duplicated unsafe peer-address handling. Tests cover address rejection,
  successful ownership transfer, and cancellation routing.
- Windows accept now owns its accepted socket with `OwnedSocket`, including all
  context/address error paths. Its byte-count output has stable boxed storage;
  cancellation retains the socket, address buffer, and byte-count storage until
  IOCP acknowledgement. Both accept destructors use the registered driver.
- Added `tools/vibeio-check`, an unpublished harness that includes the actual
  embedded source without PyO3/TLS dependencies. Root-aligned dependency versions
  are locked. Linux harness tests additionally exercise portable syscall
  fallbacks; the root tests retain normal platform cfg coverage.
- Harness compile checks passed for Linux x86_64, Windows x86_64 GNU, and macOS
  ARM64, all features and all targets. This caught a missing Windows buffer import
  and stale Windows signal-test timeout gates, which are fixed. The portable
  signal-pipe helper cfg now follows syscall availability consistently.
- Linux execution: 98 harness tests and 215 root all-feature tests passed.
  Harness doctests contain 51 ignored upstream examples, not 51 verified examples.
  Root Clippy, formatting, and CI YAML checks passed. Native runtime test jobs
  for Linux/Windows/macOS are configured but have not been dispatched remotely.
- Qualirs now reports 451 vibeio findings (66 critical, 384 warning, 1 info).
  The remaining inventory still requires individual disposition. Connect-address
  ownership, driver shutdown, process/signal lifecycle, and documentation examples
  remain open work; compile checks are not proof of native runtime behavior.

## Connect-address ownership

- TCP, UDP, and Unix-domain connect callers now transfer typed addresses into
  `ConnectOp`, which owns aligned, stable boxed storage. The constructor rejects
  lengths outside that storage instead of accepting an unconstrained raw pointer.
- Cancellation retains the original allocation on the handle's owning driver
  until completion acknowledgement, including outside an entered runtime or
  inside a different runtime. Moving the future cannot move its submitted address.
- Four regressions cover address moves/cancellation, invalid lengths, Unix address
  storage, and live loopback TCP connects through adaptive and poll-only APIs.
- Linux validation: 192 default root tests, 219 all-feature root tests, and 102
  standalone harness tests passed. Strict all-target/all-feature Clippy,
  formatting, and diff whitespace checks passed. Windows GNU and macOS ARM64
  all-target/all-feature cross-checks passed without warnings; these platforms
  were not executed locally. The 51 upstream harness doctests remain ignored.
- Real io_uring/IOCP cancellation and shutdown, Windows UDP temporary registration
  mode restoration, process/signal lifecycles, and remaining finding disposition
  still require audit. This change adds an address allocation per connect; no
  performance improvement or benchmark result is claimed.

## Windows UDP connection state

- Removed temporary registration/nonblocking-mode changes around Windows UDP
  connect. Datagram connect sets a default peer without a stream handshake, so
  the standard socket call leaves registration intact on both success and error.
  This follows the [Winsock connect contract](https://learn.microsoft.com/en-us/windows/win32/api/winsock2/nf-winsock2-connect).
- Removed the now-unused Windows raw-address conversion and asynchronous helper.
  There is no longer an await point holding a temporarily rebound UDP socket.
- Added a Windows-specific regression for poll and completion registrations:
  first-poll completion, unchanged registration token/mode after success and an
  incompatible-address error, followed by reconnect/send verification.
- Windows all-target/all-feature cross-compilation passed without warnings; the
  new Windows regression has not run natively here. Linux's 219 all-feature root
  tests and 102 harness tests passed, as did strict root Clippy and formatting.
- The general `InnerRawHandle::rebind_mode` error transaction and actual driver
  teardown still need separate review; removing this UDP use does not prove
  other callers safe.

## Linux child-wait readiness

- A pipe-controlled live-child regression reproduced an immediate terminal
  `WouldBlock` from `WaitPidOp` while the child was still running. The operation
  incorrectly used `read(pidfd)` as a readiness check; the [Linux pidfd contract](https://man7.org/linux/man-pages/man2/pidfd_open.2.html)
  specifies that such reads fail with EINVAL.
- Both operation entry points now share nonblocking waitpid checks and arm
  pidfd readiness while the child is alive. Interrupted waitpid calls retry;
  spurious wakes remain pending. Removed duplicate state machines and unnecessary
  fcntl calls (pidfd_open already sets close-on-exec).
- Replaced manual descriptor ownership with `OwnedFd`, ordered after its
  registration so deregistration happens before close.
- The regression failed before the fix and passes afterward. It exercises both
  entry points using a real Linux mio driver, a pipe-held child, a spurious poll,
  and exit-status verification under a timeout. This is not io_uring execution.
- Linux validation: 103 harness tests and 220 root all-feature tests passed;
  strict all-target/all-feature Clippy, formatting, and diff checks passed.
- Reaper channel-send failures, cancellation/runtime shutdown ownership, and
  the Windows callback lifecycle remain unresolved audit items.

## Reaper handoff and task shutdown ownership

- Reaper messages now carry a child ownership guard. Rejected sends, dropped
  queues, cancelled initialization, and cancelled per-child tasks preserve
  reaping responsibility through fallback workers. Waiting outside a runtime
  no longer calls blocking child.wait on the polling thread.
- Linux pidfd tasks now actually retain their child guard until wait completion;
  successful raw waitpid disarms it, while pidfd failures fall back to a worker.
- A live-child runtime-shutdown test initially failed: driver-held wakers kept
  pending futures alive in a reference cycle. Runtime drop now detaches the task
  slab and explicitly drops pending futures, without holding slab borrows across
  their destructors, to break that cycle.
- Four Unix regressions cover queued/rejected messages, cancellation during
  reaper initialization, nonblocking wait outside a runtime, and Linux pidfd-task
  shutdown. Child lifetime is controlled by a stdin pipe; waitid with WNOWAIT
  verifies reaping without doing the cleanup on behalf of the code under test.
- Validation: 192 default and 224 all-feature root tests, 107 harness tests,
  strict Clippy, formatting and whitespace checks passed. Process-without-signal
  compilation and Windows/macOS all-target/all-feature cross-checks passed.
- If fallback thread creation fails, the guard retains the child and performs
  a blocking wait as a last resort. This intentionally prioritizes reaping over
  latency under OS thread exhaustion. Native Windows callback lifecycle and
  real outstanding IOCP/io_uring memory teardown remain separate audit work.

## Windows registered process waits

- Replaced the raw boxed callback owner and manual unsafe Send implementation
  with an Arc-owned context containing a mutex-protected message and atomic wait
  handle. The registrar retains a reference while the one-shot callback owns a
  separate transferred reference; neither can prematurely destroy the other's
  process handle or context.
- RegisterWait output now lives in the registrar's stack storage and is published
  only after registration returns. Failed registration recovers the callback
  reference and transfers the message to the existing fallback worker.
- The last context owner explicitly requests nonblocking UnregisterWaitEx before
  releasing the process. One-shot execution does not itself release the wait;
  nonblocking unregistration avoids a callback waiting for itself. See Microsoft's
  [registered-wait](https://learn.microsoft.com/en-us/windows/win32/sync/registerwaitforsingleobjectex)
  and [unregistration](https://learn.microsoft.com/en-us/windows/win32/sync/unregisterwaitex)
  contracts.
- Added Windows tests modeling a callback finishing before registrar release and
  exercising 32 actual fast-exiting process registrations. They compile but have
  not executed on native Windows in this environment.
- Windows/macOS all-target/all-feature cross-checks passed. Linux: 224 root
  all-feature tests and 107 harness tests passed, as did strict root Clippy and
  formatting. Windows runtime/resource-leak verification remains outstanding.
- Refreshed Qualirs: 441 vibeio findings (70 critical, 370 warning, 1 info), with
  no parse errors. Syntax-based counts are not proof of cleanup: individual
  finding disposition and remaining safety/teardown audits are still required.

## Unix signal listener synchronization

- Counter checking and waker registration now share the dispatcher waker lock,
  closing the lost-wakeup window between those operations. Each listener has a
  slab slot, replaces its own old waker, and releases the slot when dropped.
  Wakers are dispatched/dropped outside the lock where practical.
- Last-listener OS handler restoration now occurs under the registration lock,
  preventing restoration from overwriting a concurrent new registration.
  Failed restoration retains the original disposition for a later retry.
- Removed unnecessary unsafe pin projection from CtrlC. A regression verifies
  independent listener slots, replacement/drop reference counts, dispatch, and
  completed-receive cleanup without sending a process-wide signal.
- Linux: 225 root all-feature and 108 harness tests passed, with strict Clippy,
  formatting, and diff checks. macOS all-target/all-feature cross-check passed.
- Handler errno preservation, full-pipe signal loss, pipe initialization/failure
  ownership, dispatcher polling latency, and coexistence with external handler
  replacement remain open. Cancelling recv while retaining its Signal can retain
  one waker until replacement, dispatch, or listener drop; it cannot grow a list
  of stale wakers for that listener.

## Signal pipe ownership and dispatch

- Both pipe ends now use OwnedFd and are retained by the process-wide registry.
  Configuration/startup errors release their owners; the handler write descriptor
  is published only after dispatch-thread startup succeeds. Retaining both ends
  also prevents descriptor reuse or SIGPIPE if the dispatch thread exits.
- The dedicated thread uses a blocking read end while the signal handler retains
  a nonblocking write end. Linux pipe2 no longer makes both ends nonblocking,
  eliminating the former 10 ms empty-pipe polling sleep. EOF/fatal read errors
  terminate dispatch instead of spinning forever. No benchmark speedup is claimed.
- Two regressions cover close-on-exec/nonblocking flags, pipe data/EOF, and
  injected thread-start failure releasing registry ownership. Root tests exercise
  pipe2; the standalone Linux harness exercises portable pipe setup.
- Validation: 227 all-feature root tests and 110 harness tests passed. Strict
  Clippy, formatting, whitespace checks, and macOS cross-compilation passed.
- Handler errno preservation, full-pipe signal delivery, external sigaction
  coexistence, fork behavior, and surfacing an unexpected dispatcher exit remain
  open audit items.

## Signal handler errno preservation

- Signal notification writes now save/restore the interrupted thread's errno,
  including nonblocking pipe-full failures. The handler path performs no logging,
  allocation, formatting, or locking. This follows the
  [signal-safety errno requirement](https://man7.org/linux/man-pages/man7/signal-safety.7.html).
- Added optional errno 0.3 to the signal feature in root and harness manifests;
  its Unix get/set implementations were inspected to verify direct platform
  thread-local access. Lockfiles retain errno 0.3.14 and were refreshed offline.
- A regression exercises the exact write helper with a successful pipe write,
  a saturated private pipe, and an invalid descriptor, checking the original errno
  survives each case without modifying the global signal pipe.
- Validation: 228 root all-feature tests and 111 harness tests passed, as did
  strict Clippy, formatting, Windows/macOS cross-checks, and the Rust example
  build. This does not solve notification loss when the signal pipe is full.

## Signal-pipe saturation

- Signal handlers now set fixed atomic pending flags before attempting the pipe
  write. The dispatcher treats pipe contents as wakeups and scans those flags;
  a full pipe already contains a wake, so distinct pending signal notifications
  survive saturation. Interrupted writes retry with errno still preserved.
- Registration rejects signal numbers outside the fixed supported range (1–127),
  covering Linux/Apple signal kinds without indexing unbounded handler storage.
  The handler does not allocate or lock. Repeated occurrences can coalesce;
  exact counts/order, including real-time signal payloads, are not promised.
- A private-pipe saturation regression verifies distinct pending kinds, repeat
  coalescing, errno preservation, consumption, and a subsequent occurrence.
  Another regression covers out-of-range registration. Existing live-signal tests
  exercise dispatcher integration; the saturation test does not mutate globals.
- Validation: 230 all-feature root tests and 113 harness tests passed, with
  strict Clippy, formatting, whitespace checks, and macOS cross-compilation.
- External-handler coexistence, fork behavior, dispatcher failure reporting,
  and the remaining I/O driver/executor safety inventory remain open.

## Optional blocking-pool and reaper fallbacks

- Missing runtime blocking pools now return SpawnBlockingError rather than
  panicking through an API that already returns Result.
- The no-signal Unix reaper now uses the existing independent fallback worker,
  removing its hidden dependency on an optional blocking pool. SIGCHLD setup or
  receive failure switches to that same worker path; normal channel shutdown
  transfers outstanding children together with their status senders.
- Tests explicitly construct runtimes with no pool. They verify the error API
  and successful fallback reaping/status delivery after runtime drop.
- Validation: 232 all-feature root tests, 115 all-feature harness tests, and 83
  process-only harness tests passed. Strict Clippy, formatting, whitespace checks,
  and macOS all-target/all-feature cross-compilation passed. Signal setup failure
  itself was not fault-injected; broader safety and teardown work remains open.

## io_uring interrupt descriptor ownership

- Replaced Arc<RawFd>/Weak<RawFd> with shared OwnedFd ownership. Driver drop no
  longer manually closes the eventfd while another thread may hold an upgraded
  interrupt reference. The last owner closes it; later weak upgrades fail safely.
- Removed the always-present descriptor's Option and corresponding expect calls.
  The descriptor now also remains owned while the ring field is dropped.
- A real-eventfd regression models an interrupt reference acquired before driver
  ownership release, verifies a successful wake/read afterward, and checks that
  wake after final release is a no-op. It does not require an io_uring queue.
- Validation: 233 root all-feature tests and 116 harness tests passed, plus strict
  Clippy, formatting, and whitespace checks.
- This addresses only interrupt fd lifetime. Outstanding kernel reads into the
  interrupt buffer, retained operation buffers, and deferred ring teardown still
  require their own completion/cancellation audit; closing the ring alone is not
  being treated as proof that kernel-visible memory can be freed.

## io_uring shutdown cancellation

- Driver drop now flushes queued SQEs and requests synchronous cancellation of
  all submitted work before releasing retained buffers. Shutdown drains CQEs
  without waking tasks or rearming the interrupt read; unclaimed descriptor
  results remain owned for cleanup. The [synchronous cancellation contract](https://man7.org/linux/man-pages/man3/io_uring_register_sync_cancel.3.html)
  and the local io-uring submitter implementation were checked.
- Cancellation has a one-second timeout. Submission/cancellation failure retains
  registration/completion allocations and the original interrupt buffer instead
  of freeing possibly kernel-visible pointers. This is an explicit exceptional
  leak, not a complete resource-reclamation solution; failure-path refinement and
  fault injection remain open.
- A live Linux io_uring regression ran without skipping on this host. Both an
  unsubmitted queued read and an already-submitted read returned ECANCELED;
  tracked buffer storage survived acknowledgement and was released by driver
  drop. The test reports unavailable kernels explicitly on EPERM/ENOSYS/EOPNOTSUPP.
- Validation: 234 root all-feature tests and 117 harness tests passed, along with
  strict Clippy, formatting, and whitespace checks. Outstanding work includes
  SQPOLL/custom-ring modes, completion overflow/descriptor cleanup stress,
  shutdown latency under load, and the separate IOCP teardown implementation.

## Shutdown completion overflow

- Shutdown now drains visible CQEs before flushing pending submissions and then
  repeatedly flushes/drains the kernel CQ overflow list after cancellation. With
  NODROP, a single pass over the mapped CQ is insufficient: descriptor-producing
  results may still be buffered in the kernel.
- Extracted shutdown-only CQ handling, preserving the no-wakeup/no-rearm behavior
  and descriptor ownership established in the cancellation cleanup.
- A live two-entry-CQ test forces overflow with three successful file opens,
  confirms the overflow flag is cleared by shutdown, and verifies every result
  reaches its descriptor-owning registration. It ran without skipping here.
- Validation: 235 root all-feature and 118 harness tests passed, with strict
  Clippy, formatting, and whitespace checks. Failure-retention cleanup, custom
  ring modes, load/latency validation, and IOCP teardown remain open.

## IOCP completion retirement

- Cancelling/ignoring a completion whose packet was already dequeued now removes
  the entry immediately. Waiting for another notification previously retained
  its payload forever. Unknown-token payloads also retire outside state borrows.
- Completion processing collects payload retirements and wakers, releases the
  driver-state borrow, then runs destructors and wakes tasks. User callbacks can
  re-enter the driver without colliding with that borrow.
- Two Windows regressions use payload destructors that borrow driver state:
  completed/unknown-token cancellation and simulated dequeuing of a cancelled
  operation. They compile, but have not executed natively here.
- Windows all-target/all-feature cross-compilation, root strict Clippy,
  formatting, whitespace checks, and 118 Linux harness tests passed. Linux tests
  do not execute IOCP. Outstanding Windows kernel requests at driver drop remain
  the next IOCP lifetime issue; completion retirement is not teardown verification.

## IOCP shutdown acknowledgement

- Added driver shutdown draining with a one-second deadline. It removes task
  waiters, requests cancellation of driver-owned AFD polls, and consumes packets
  until no pending OVERLAPPED or AFD storage remains. Completion-operation
  destructors already request cancellation before releasing their driver owner.
- Timeout/error retains unacknowledged completion and poll allocations rather
  than freeing live kernel pointers. Already acknowledged entries release
  normally. This exceptional retention remains a leak requiring later reclamation
  work, not a claim of fully clean shutdown.
- The design follows Microsoft's [CancelIoEx contract](https://learn.microsoft.com/en-us/windows/win32/api/ioapiset/nf-ioapiset-cancelioex):
  requesting cancellation does not establish completion or permit storage reuse.
- A Windows regression models pending storage, confirms zero-timeout shutdown
  does not release it, posts its packet through a real IOCP port, then verifies
  acknowledged shutdown releases it. It cross-compiles but has not run here;
  actual Windows socket/AFD cancellation and shutdown latency remain unverified.
- Validation: Windows all-target/all-feature cross-check, strict root Clippy,
  formatting, whitespace checks, and 118 Linux harness tests passed.

## Thread-safe task wake ownership

- Raw wakers now own a separate Send+Sync wake proxy, not Arc<Task> containing a
  LocalBoxFuture and non-atomic Rc weak references. Final waker release on a worker
  thread can no longer destroy local task state. Borrowed polling wakers retain
  their allocation-free borrow/clone-only reference-count behavior.
- Same-thread wake resolves the local task through the entered runtime and keeps
  its next-task fast slot. Cross-thread queues carry proxy identity as well as the
  slab token; stale wakes cannot accidentally schedule a replacement task after
  token reuse. No unsafe Send/Sync implementation was added.
- Tests enforce proxy Send+Sync, borrowed reference counts, owner-thread future
  destruction despite a remote waker, safe remote final release, unwind behavior,
  and rejection of a stale wake against an actually reused runtime task slot.
- Validation: 198 default and 237 all-feature root tests, 120 harness tests,
  strict Clippy, formatting/whitespace checks, and Windows/macOS cross-checks
  passed. This adds a proxy allocation per task and local lookup overhead;
  performance impact remains to be measured, not presumed neutral.

## Local task ownership and scheduler measurement

- Converted local task ownership, ready queues, and join-state weak references
  from Arc to Rc. Thread-safe wake proxies remain Arc-backed; no local task is
  carried through a Waker. Removed obsolete Arc-with-non-Send lint exceptions.
- Existing ownership, stale-wake, cancellation, and local scheduling regressions
  pass: 237 root all-feature and 120 harness tests, strict Clippy, formatting,
  whitespace checks, and Windows/macOS cross-compilation.
- Ran the existing optimized scheduler benchmark before/after only this Rc
  conversion. Seven-sample medians: spawn/join 26.603→24.680 ms, single-task
  yield 48.714→43.681 ms, batch yield 41.286→33.319 ms. Method and limitations
  are recorded in `benches/vibeio-performance.md`; this does not measure the
  entire wake-proxy redesign or establish Python/uvloop performance.

## Refreshed finding disposition

- Fixed the remaining spawn_blocking panic outside a runtime: it now returns
  SpawnBlockingError and releases the unused closure. Added a no-runtime poll
  regression. Removed unsafe set_buf_init from io::copy's concrete Vec reset,
  replacing it with Vec::clear.
- Q0069 on ordinary spawn is an explicit API precondition: it returns a
  JoinHandle rather than Result and documents requiring an entered runtime.
  Q0069 on spawn_blocking was meaningful and is now addressed.
- Q0078 filesystem reports include calls inside offloaded closures and the
  intentionally synchronous branch when enable_fs_offload is false. They are
  not all erroneous, but the default/offload API behavior still needs review.
- Q0085 is positioned on copy even though its lock calls are in later split-half
  implementations. Those use an async mutex, not a blocking mutex. However, code
  inspection identified a separate real concern: the generic split holds mutual
  exclusion for the full read/write future, so copy_bidirectional can deadlock
  when an outstanding read prevents writing the response needed by a peer.
  This is open work, not a dismissed finding.
- Q0082 on reaper worker locking includes code inside a spawned thread and an
  explicit last-resort spawn-failure path. Signal drop's registry/waker mutex
  acquisition is real synchronization; contention behavior remains to be audited.
- Validation of this increment: 238 root all-feature and 121 harness tests,
  strict Clippy, formatting, and whitespace checks passed. Remaining findings
  have not been globally suppressed or declared clean.

## Bidirectional-copy contract

- Replaced the mutex-split bidirectional relay with Tokio's poll-based
  copy_bidirectional implementation. Its inputs now require Tokio AsyncRead +
  AsyncWrite + Unpin (implemented by PollTcpStream/PollUnixStream), rather than
  the buffer-owning traits whose futures exclusively borrow the whole object.
  No repository call sites required migration. This internal API change is
  intentional: generic buffer-owning traits cannot guarantee duplex access.
- Both directions progress without a whole-object async mutex. EOF shuts down
  the opposite write half, and an error terminates without joining a perpetually
  pending other direction. Existing generic split remains serialized and now
  explicitly warns against full-duplex request/response use.
- Tests cover a one-byte-backpressure request/response relay with half-close and
  an immediate error while the other direction remains pending.
- Validation: 240 root all-feature tests, 123 harness tests, strict Clippy,
  formatting, whitespace checks, and Windows/macOS cross-compilation passed.
  These relay tests use in-memory poll I/O; network throughput was not measured.

## Inline buffer address-stability verification

- Rechecked inline array support against current operation storage. The existing
  CompletionBuffer already boxes completion-mode buffers before submission and
  retains that exact box on cancellation, while poll-only buffers stay inline.
  Array trait implementations were therefore preserved, not removed.
- Clarified the IoBuf contract: implementations keep pointers valid while the
  value is stationary; operation callers must retain that address while a kernel
  pointer is outstanding. This matches the existing completion storage policy.
- Added regressions checking an inline array's exact pointer through operation
  storage moves, conversion to the stable cancellation box, type-erased driver
  payload ownership, and recovery. A poll-mode test verifies inline storage.
- Validation: 242 root all-feature tests, 125 harness tests, strict Clippy,
  formatting, and whitespace checks passed. These are storage-contract tests,
  not additional live-kernel cancellation coverage.

## Executable timer documentation

- Migrated three ignored timer snippets into `tools/vibeio-check/EXAMPLES.md`,
  included as harness crate documentation so Rust compiles and executes them
  against the embedded source. Source API docs point to the executable examples.
- Examples explicitly enable timers and use the mock I/O driver. They cover
  sleep, successful and expired timeout, and a finite interval loop that permits
  catch-up ticks under load instead of assuming an exact elapsed-period count.
- Validation: default-feature doctests passed (3 executed, 19 still ignored);
  all-feature harness passed 125 unit tests and 3 doctests (48 still ignored).
  Formatting and whitespace checks passed. Remaining ignored examples have not
  been validated; this is a documentation-coverage increment, not full cleanup
  or a performance measurement.

## Failed registration ownership

- Fixed a confirmed cross-handle cleanup bug: a partially constructed
  InnerRawHandle previously owned placeholder token 0, so a registration error
  could deregister a different live handle when the partial wrapper was dropped.
  It now starts with an explicit unregistered sentinel and only drops a token
  actually acquired from the driver.
- A mode switch now relinquishes its old token after successful deregistration,
  before attempting registration. Failure no longer leaves a stale owned token;
  dropping is safe and retrying the original mode registers again. This does not
  promise transactional rollback: callers must drop or retry before I/O, as the
  current consuming conversion call sites do.
- Added a real Mio invalid-descriptor regression with an existing live token 0,
  plus mock fault injection covering failed mode switches followed by drop or
  retry. Both tests failed with the previous token handling and pass with the
  fix (see target/vibeio-registration-before.log).
- Validation: 244 root all-feature tests, 127 harness unit tests and 3 doctests
  passed, as did strict Clippy, formatting, whitespace checks, and Windows/macOS
  cross-compilation. Cross-platform execution is still not verified here.

## Socket setup cleanup and Unix poll mode

- TCP stream/listener, UDP, and Unix stream/listener constructors now retain
  automatic registration cleanup through the fallible set_nonblocking call.
  ManuallyDrop is installed only after that succeeds. On setup failure the
  registration is therefore dropped before the owned socket, instead of leaked.
  This includes the Windows poll-listener constructor.
- UnixStream::from_std_poll previously selected the driver's default mode,
  including completion mode on io_uring. It now delegates to the shared
  constructor with an explicit Poll mode. A completion-capable mock regression
  checks the registration mode and a real socket's O_NONBLOCK flag. Restoring
  the old completion-mode selection fails this test.
- Validation: 245 root all-feature tests, 128 harness unit tests and 3 doctests,
  strict Clippy, formatting, whitespace checks, and Windows/macOS compile checks
  passed. OS-level set_nonblocking failure injection and native Windows/macOS
  execution were not performed.
- Follow-up identified during this audit: pipe and process descriptor setup
  currently ignores fcntl errors. Those paths need error propagation and cleanup
  validation; they are not covered by this socket-constructor increment.

## Descriptor mode error propagation

- Pipe construction/conversion and child stdio registration now use one checked
  nonblocking-mode helper. It preserves unrelated flags, avoids redundant
  F_SETFL calls, retries interrupted syscalls, and propagates both query and
  update errors. Registration ownership stays automatically droppable until
  initial configuration succeeds.
- Child stdio configuration errors now reach their constructors. Child::from_std
  installs its reaping owner before these fallible conversions so a partially
  wrapped child is not abandoned. Registration-unavailable blocking fallback is
  preserved. Corrected stale process documentation about outside-runtime use.
- Added tests for mode toggling/idempotence and unchanged unrelated flags,
  invalid-descriptor error propagation with registration cleanup, and a real
  pipe roundtrip through poll/completion-request conversion on the Mio driver.
- Validation: 248 root all-feature tests, 131 harness unit tests and 3 doctests,
  strict Clippy, formatting, whitespace checks, and Windows/macOS compile checks
  passed. F_SETFL-specific fault injection, full child-construction failure
  reaping, and native Windows/macOS execution remain unverified.
- Additional audit item: io::pipe creation still uses plain pipe without
  close-on-exec setup; descriptor inheritance needs review.

## Owned, close-on-exec pipe creation

- Replaced raw pipe creation in async pipes, signal notifications, and splice
  staging with std::io::pipe and safe owned-descriptor conversions. Inspected
  the installed Rust 1.98.1 standard-library implementation: it uses atomic
  pipe2(O_CLOEXEC) on Linux and checked owned pipe/fcntl setup on macOS. The
  latter still has a non-atomic creation-to-cloexec window; this change does not
  claim to remove that platform limitation.
- Signal setup now uses the shared checked nonblocking helper on its write end;
  its read end remains blocking. Removed duplicated raw ownership and flag
  configuration code. Splice retains its blocking pipe semantics.
- Two new async-pipe regressions check FD_CLOEXEC and actual Linux exec
  inheritance, comparing endpoint identity to tolerate numeric FD reuse in the
  child. Both fail with the prior raw pipe creation and pass after the fix.
- Validation: 250 root all-feature tests, 133 harness unit tests and 3 doctests,
  strict Clippy, formatting, whitespace checks, and Windows/macOS compile checks
  passed. Existing signal pipe/mode tests also pass. Native macOS execution and
  macOS concurrent-spawn inheritance remain unverified.
- Reference: https://doc.rust-lang.org/std/io/fn.pipe.html (stable since 1.87;
  ownership and close-on-exec details additionally checked in local Rust source).

## Splice transfer progress

- sendfile_exact now drains each staging-pipe batch completely before refilling.
  Previously a partial socket drain could be followed by a fill waiting on pipe
  space, with the only drain code suspended behind that fill. The loop reports
  WriteZero on a zero-progress drain and preserves short counts at source EOF.
- Clamped each SpliceOp request to the completion ABI's u32 length limit rather
  than silently wrapping a larger request to zero (and falsely signaling EOF).
- Splice writer registration now retains automatic cleanup through checked
  nonblocking configuration, replacing another ignored-fcntl-error path.
- Tests cover partial-drain ordering, EOF, exact limits including zero,
  zero-progress/error termination, oversized request lengths, and a live Linux
  memfd-to-Unix-socket splice with early EOF. Partial-drain ordering is tested
  deterministically; the live test does not claim forced socket backpressure.
- Validation: 255 root all-feature tests, 138 harness unit tests and 3 doctests,
  strict Clippy, formatting and whitespace checks passed. Windows/macOS compile
  checks passed (splice itself is Linux-only). Throughput was not measured;
  source-side readiness and completion-cancellation behavior remain audit work.

## Vectored test descriptor ownership and obsolete configuration

- The live io_uring vectored pipe test now owns both endpoints through std::io::pipe.
  Registrations are declared afterward, ensuring deregistration precedes close
  on success and assertion unwind. Removed manual closes that ran while handles
  were still registered, and raw descriptors that leaked on assertion failures.
- Removed the final syscall_pipe2 conditional and its now-unused build-script
  target list/check-cfg entries from both manifests. Pipe platform selection is
  owned by the standard library; syscall_accept4 configuration remains intact.
- Validation: 255 root all-feature tests, 138 harness unit tests and 3 doctests,
  strict Clippy, formatting, whitespace checks, and Windows/macOS cross-checks
  passed. The live vectored io_uring test ran successfully on Linux.
- Further source inspection found inconsistent read-buffer treatment: native
  ReadOp reads up to capacity and updates initialized length, while several
  file/process/stdio wrappers short-circuit at zero initialized length and their
  blocking helpers expose only that initialized prefix. This needs behavioral
  regressions and a consistent safe fallback, not an uninitialized mutable slice.

## Capacity-aware blocking reads

- File positioned reads, child stdout/stderr, and stdin now accept empty buffers
  with spare capacity rather than treating zero initialized length as EOF.
  File read_exact_at fills writable capacity and retains the actual prefix on
  early EOF, matching the capacity-based native operations.
- Replaced the initialized-prefix slice helper with a checked read adapter.
  It initializes spare bytes before constructing a safe mutable slice, updates
  initialized length only after a valid successful read (including EOF), and
  rejects an oversized byte count from a reader. Errors retain the prior length.
  Zeroing is limited to the blocking fallback's uninitialized spare capacity;
  native I/O paths do not incur that work. No throughput claim is made.
- Tests cover spare-byte initialization, short reads, EOF, error/oversized-count
  handling, zero capacity, direct and offloaded file reads, and the blocking
  child-reader path. Existing process roundtrip tests now use empty vectors with
  spare capacity. Restoring the old file early-return condition fails the new
  file regression (target/vibeio-read-capacity-before.log).
- Validation: 259 root all-feature tests, 142 harness unit tests and 3 doctests,
  strict Clippy, formatting, whitespace checks, and Windows/macOS compilation
  passed. Native non-Linux execution and stdin-specific subprocess integration
  remain unverified; shared adapter behavior is covered without reading user stdin.

## Copy progress and initialized tails

- copy now limits writes to the reader's returned byte count rather than copying
  every initialized byte in the returned vector. Extra initialized tail bytes
  are valid storage but are not data returned by that read.
- Invalid read counts and oversized write counts produce InvalidData rather
  than silently losing data or panicking during cursor advance. Zero-progress
  writes still produce WriteZero. Successful EOF still flushes exactly once.
- Removed the redundant allocate-zero-then-clear initialization of the reusable
  copy buffer. Clarified capacity and count contracts on AsyncRead/AsyncWrite.
- Added regressions using a five-byte initialized buffer with only three bytes
  reported, one-byte partial writes, invalid counts, and zero-progress writes.
- Validation: 261 root all-feature tests, 144 all-feature harness tests, 92 default
  harness tests, 3 executed doctests in each configuration, strict Clippy,
  formatting, whitespace checks, and Windows/macOS compile checks passed.
  Throughput and native non-Linux execution were not measured here.

## Tokio adapter partial-write progress

- AsyncWrap::poll_write now performs one underlying write and returns its count,
  including partial or zero counts. The previous internal write_all loop could
  successfully write a prefix and then return only a later error, hiding that
  progress from callers. Removed repeated split_off allocation/copying as well.
- Oversized counts still produce InvalidData. Tokio's write_all helper now owns
  the retry loop and WriteZero behavior. Updated the existing full-write and
  zero-progress tests to exercise that helper; new tests cover a successful
  prefix followed by BrokenPipe and a plain poll_write returning zero.
- Updated adapter documentation for partial writes and its existing lack of
  concurrent full-duplex support.
- Validation: 263 root all-feature tests, 146 harness tests and 3 doctests,
  strict Clippy, formatting, whitespace checks, and Windows/macOS compile checks
  passed. Cancellation/re-poll behavior with a different buffer while an owned
  write is pending remains an adapter audit item; this increment does not
  establish full Tokio cancellation compatibility or a throughput improvement.

## Buffered write acknowledgement and cancellation

- Replaced the adapter's completion-based write acknowledgement with bounded
  buffering. poll_write accepts at most 4 KiB and returns that count immediately;
  later writes first drain previously accepted bytes. Pending therefore never
  consumes bytes from its current caller, and a replacement caller buffer cannot
  be confused with a stored completion's byte count.
- Deferred draining uses checked cursor advancement, not repeated split_off.
  Errors draining accepted bytes are reported by the next drain operation.
  Reads drain accepted writes first; flush drains writes before the underlying
  flush, and shutdown now flushes instead of silently returning success.
- This supersedes the previous increment's single-operation acknowledgement.
  Documentation explicitly requires flush/shutdown before drop and explains
  delayed errors, bounded buffering, no concurrent full duplex, and no underlying
  half-close support (the buffer-owning trait lacks shutdown).
- Regression replaces a pending write's discarded buffer with a smaller one,
  verifies correct acknowledgement and exact output, and checks shutdown drains
  and flushes. Other tests cover bounded acceptance, deferred errors/WriteZero,
  and multi-batch write_all with two-byte underlying writes.
- Validation: 264 root all-feature tests, 147 harness tests and 3 doctests,
  strict Clippy, formatting, whitespace checks, and Windows/macOS compile checks
  passed. Tests model pending writes deterministically; live completion-mode
  cancellation stress, native non-Linux execution, and throughput remain unverified.

## Zero-period interval fairness

- Zero-period CatchUp intervals now yield once per tick, matching Skip mode.
  Previously their ready-only loop could monopolize the executor. The public
  tick documentation now states the shared zero-period behavior explicitly.
- Deterministic tests verify Pending then Ready(1) on repeated ticks in both
  modes and that cancelling a pending tick leaves the schedule unchanged before
  a successful retry. No wall-clock sleeps or timing thresholds are required.
- Validation: 266 root all-feature tests, 149 harness tests and 3 doctests,
  strict Clippy, formatting, whitespace checks, and Windows/macOS compile checks
  passed. Unrepresentable Instant arithmetic and positive-period boundary
  scheduling remain separate timer audit items; this does not finish that audit.

## Absolute interval deadlines and deterministic catch-up tests

- Intervals now submit their absolute target to Sleep instead of computing a
  remaining duration and adding it to a later Instant::now(). Scheduler pauses
  between those clock reads can no longer extend that individual wait.
- Consolidated Sleep constructors around one absolute-deadline initializer,
  retaining configurable yielding for already-expired targets and correcting
  the misleading sleep_until documentation about relative conversion.
- Factored the scheduling clock into a private tick_at helper. Replaced the
  catch-up test's real initial wait and latency-sensitive exact count with
  deterministic tests for due-now, just-before, exact-period, and multi-period
  boundaries. Added a constructor test preserving an already-expired deadline.
- Validation: 267 root all-feature tests, 150 harness tests and 3 doctests,
  strict Clippy, formatting, whitespace checks, and Windows/macOS compile checks
  passed. Unrepresentable Instant arithmetic remains open; no timer-latency or
  performance improvement is claimed from these tests.

## Representable timer deadline limits

- Relative sleeps/timeouts and interval deadline advancement now use checked
  addition. On overflow, deadlines saturate at the platform's last representable
  Instant, with that policy documented on Sleep and Interval.
- Ordinary additions take one checked-add path. Overflow alone invokes a bounded
  binary search because Instant exposes no portable MAX constant. This avoids
  inventing a fixed horizon or letting a large duration wrap into immediate expiry.
- Tests cover exact ordinary additions, saturation/maximality, repeated addition
  at the limit, pending Duration::MAX sleeps/timeouts/intervals, and advancing an
  overdue large-period catch-up interval.
- Validation: 270 root all-feature tests, 153 harness tests and 3 doctests,
  strict Clippy, formatting, whitespace checks, and Windows/macOS compile checks
  passed. Huge timers were polled and cancelled, not left in a real OS wait;
  extreme platform wait conversions and native non-Linux behavior still require
  separate verification. Absolute timeout_at's relative conversion also remains
  to be corrected, as identified while following deadline creation paths.

## Absolute timeout construction

- Added Timeout::new_at and routed timeout_at directly through an absolute Sleep
  deadline. Removed the deadline-to-duration-to-later-deadline conversion that
  could extend timeouts when execution paused between clock reads. Relative
  Timeout construction shares this initializer and retains overflow saturation.
- Documented existing poll priority: an immediately ready inner future wins
  even with an expired deadline. Pending futures expire when polled.
- Added tests for expired absolute deadlines with pinned-future destruction,
  ready-future priority, and cancellation releasing both the future and timer
  registration.
- Validation: 272 root all-feature tests, 155 harness tests and 3 doctests,
  strict Clippy, formatting, whitespace checks, and Windows/macOS compile checks
  passed. Native non-Linux execution remains unverified.
- Inspection of the timer heap identified another audit target: cancellation
  drops a removed waker in an expression containing a RefMut, and expiry wakes
  callbacks while the reusable expired-vector RefMut is held. Reentrant waker
  behavior needs targeted verification before declaring timer cleanup complete.

## Timer callback reentrancy

- Timer cancellation now drops removed wakers after releasing the heap borrow.
  A regression with a reentrant destructor fails with the previous expression
  (RefCell already borrowed) and passes with explicit ownership separation.
- Expiry takes the reusable waker vector out of its RefCell before invoking
  callbacks, permitting nested timer spins. It retains reusable capacity after
  callbacks without overwriting a nested spin's larger allocation.
- The next deadline and current time are read after callbacks, so inserted or
  cancelled timers and callback elapsed time are reflected in the returned wait.
  A deterministic test reenters spin and inserts a new deadline during wake.
- The test-only ReenterOnDrop waker has a narrowly documented manual_noop_waker
  exception: its destructor is the behavior under test, which Waker::noop cannot
  supply. No production lint policy was relaxed.
- Validation: 274 root all-feature tests, 157 harness tests and 3 doctests,
  strict Clippy, formatting, whitespace checks, and Windows/macOS compile checks
  passed. Timer throughput and native non-Linux execution were not measured.

## Mio waiter callback ownership

- Mio now collects ready wakers and releases poll, event, and registration
  borrows before invoking them. A reusable vector retains capacity, including
  across nested polling, instead of requiring a fresh allocation every wait.
- Waiter replacement returns the old or redundant incoming waker for destruction
  after the registration borrow ends. Deregistration similarly drops removed
  registrations outside the state borrow.
- A real Unix-socket regression exercises replacement, deregistration, and
  readiness delivery with callbacks/destructors that reenter both registration
  inspection and zero-timeout polling. All three paths complete without a
  RefCell panic, and the cached wake vector is empty afterward.
- Validation: 275 root all-feature tests, 158 harness tests and 3 doctests,
  strict Clippy, formatting, whitespace checks, and Windows/macOS compile checks
  passed. Other drivers' waiter replacement/removal paths still need auditing;
  Mio throughput and native non-Linux execution were not measured here.

## io_uring completion dispatch ownership

- Completion collection now returns an owned batch; payload destruction and
  waiter wakeups occur only after the caller releases ring and state borrows.
  Previously drain_cq woke callbacks while its caller still held both RefMuts
  and dropped cancelled-operation payloads during slab removal under that borrow.
- The interrupt read is rearmed before dispatching reentrant callbacks. Retained
  the existing inline eight-waker fast path; retired payload storage is allocated
  only when that batch contains cancelled completions to retire.
- get_completion_result and deregistration also move removed records outside
  their state borrows before destruction.
- A live NOP completion carries a payload whose destructor verifies both borrows
  are free and reenters completion collection. Its waiter verifies retirement
  happened first. The test ran successfully on this Linux host.
- Validation: 276 root all-feature tests, 159 harness tests and 3 doctests,
  strict Clippy, formatting, whitespace checks, and Windows/macOS compile checks
  passed (io_uring itself is Linux-only). Waiter replacement and submission-error
  cleanup remain separate io_uring audit work; throughput was not measured.

## io_uring waiter replacement and failed arming

- update_waiter now returns replaced/redundant wakers instead of dropping them
  inside the state borrow. Poll, multishot accept, and completion-waker callers
  retain those objects until their state updates and submission attempts end.
- Failed poll/accept submissions take the stored waker out and release the state
  borrow before destruction. Multishot accept clones the incoming waker before
  borrowing state, avoiding a reentrant custom clone callback under the borrow.
- Added a completion-slot replacement/unknown-token regression using a destructor
  that checks both driver borrows are available. Its destructor-only test waker
  has a narrowly documented manual_noop_waker exception, like the timer test.
- Validation: 277 root all-feature tests, 160 harness tests and 3 doctests,
  strict Clippy, formatting, whitespace checks, and Windows/macOS compile checks
  passed. Poll/accept submission exhaustion was not fault-injected in this
  increment; native non-Linux execution and throughput remain unmeasured.

## Kqueue callback ownership and harness lint alignment

- Kqueue now takes its reusable wake vector out before callbacks, permitting
  nested waits. Waiter replacement moves ownership without cloning under the
  state borrow, and interest removal retains discarded wakers until after state
  changes and filter syscalls finish (including error unwinding).
- Added native-kqueue regressions for replacement, interest removal, and actual
  readiness with callbacks/destructors that inspect state and reenter wait.
  These tests compile for macOS here but have NOT been executed natively.
- Cross-target Clippy exposed that the standalone harness lacked the root's
  approved lint policy. Copied that same group policy and explicit safety checks
  into the harness; no new production suppressions or policy changes were made.
- Validation: macOS all-target/all-feature compile and strict Clippy checks pass;
  Linux harness passes 160 tests and 3 doctests, plus strict Clippy, formatting,
  and whitespace checks. Native kqueue runtime verification and transactional
  interest-update behavior on kernel errors remain open work.

## IOCP waiter and deregistration ownership

- IOCP waiter replacement, completion consumption, and failed AFD arming now
  release wakers/records outside state borrows. Failed arming no longer retains
  a waiter for an operation that was never submitted successfully.
- Deregistration now distinguishes registration kind explicitly. An idle Poll
  registration with no poll token no longer takes the completion-disassociation
  branch; removed poll waiters also drop outside the state borrow.
- Added a Windows completion-consumption regression with a reentrant payload
  destructor. It is compile-checked here, not natively executed.
- Windows-target Clippy uncovered 14 redundant casts in Windows socket code;
  removed those casts without suppressions. Strict all-target/all-feature
  Windows Clippy now passes under the approved policy.
- Linux harness passes 160 tests and 3 doctests; formatting and whitespace
  checks pass. Native Windows socket/AFD failure tests and disassociation
  semantics still need runtime verification; no cross-platform execution claim
  is inferred from compilation.

## Native CI regression checks

- Extended the existing native harness jobs for Linux, Windows, and macOS with
  rustfmt, strict all-target/all-feature Clippy under the approved risk-focused
  policy, and separate default-feature and all-feature tests, including doctests.
- Documented the same commands in the harness README. The workflow remains
  manually dispatched; it was not dispatched during this change.
- Local Linux validation passes: 108 default-feature unit tests and 160
  all-feature unit tests, plus 3 doctests in each configuration. Respectively
  19 and 48 legacy doctests remain ignored. Strict Clippy, formatting, and
  whitespace checks pass. Native Windows/macOS execution remains unverified.

## Kqueue interest-update failure recovery

- Interest updates now install new filters before removing old filters, and
  update each installed-filter flag only after its syscall succeeds. Removed
  waiters/readiness are cleared only after successful removal. This prevents
  failed additions from discarding existing waiters or advertising filters that
  were never installed. The unused duplicate interest field was removed.
- Multi-filter changes are not atomic. If a later removal fails, the successful
  addition remains recorded, so retries finish the remaining change and handle
  deregistration still knows which filters to remove. No rollback guarantee is
  claimed. Retired wakers continue to drop outside the state borrow.
- Added failure-injection coverage for failed addition and for successful
  addition followed by failed removal, checking preserved waiters/readiness,
  installed-filter state, and exact retry operations. These kqueue tests compile
  under macOS all-target/all-feature strict Clippy; native execution remains
  pending. Linux passes 160 harness tests and 3 doctests, strict Clippy,
  formatting, and whitespace checks, but does not execute kqueue code.
- Initial-registration partial failures and deregistration syscall failures
  remain separate audit items; this change addresses interest updates only.

## Splice source-readiness handling

- Reproduced the output-only readiness bug with a real empty source pipe and
  writable destination: the new test failed before the fix because destination
  readiness woke the operation even though no input had arrived.
- On WouldBlock, the poll path now checks source readiness without waiting. An
  empty source gets a lazy, independently registered duplicate on the destination
  handle's owning driver. A ready source continues to wait on output writability.
  The duplicate is deregistered before closing when the operation is dropped;
  sources already registered elsewhere do not conflict with this registration.
- Live Mio/Linux regressions cover empty input, full output, an already
  registered source, cancellation followed by a replacement operation, and EOF
  wakeups. The success path adds no source poll/duplicate registration. No
  throughput improvement is claimed without measurements.
- Documented readiness-mode socket nonblocking requirements, exclusive source
  consumption, and possible regular-file storage blocking. Source descriptor
  status flags are not changed by this operation.
- Validation passes: 280 root all-feature tests; 163 harness all-feature tests
  plus 3 doctests; 116 splice-only harness tests plus 3 doctests; root/harness
  strict Clippy, formatting, and whitespace checks. The new tests execute Mio,
  not io_uring's poll mode. Completion-mode cancellation/descriptor lifetimes
  and the public AsRawFd-to-BorrowedFd conversion remain separate audit work.

## Splice completion descriptor ownership

- Splice SQEs now use owned, close-on-exec duplicates of both descriptors.
  Cancellation transfers those exact descriptors to the owning driver's ignored
  completion storage until CQE retirement. Failure to duplicate either endpoint
  returns an OS error before an SQE can be submitted. Repeated entry building
  reuses the retained descriptors rather than replacing queued descriptor numbers.
- Removed the unchecked AsRawFd-to-BorrowedFd conversion. The public zero-copy
  functions still accept AsRawFd; poll syscalls validate raw numbers directly,
  and duplication uses checked F_DUPFD_CLOEXEC with Interrupted retries. No
  descriptor duplication was added to the successful readiness-mode fast path.
- Deterministic tests verify ownership of both endpoints after originals close,
  cancellation without an ambient runtime, descriptor reuse across entry builds,
  and invalid source/destination errors. A live io_uring regression queues a
  splice, cancels before flushing, closes originals, then observes transferred
  data and EOF after CQE cleanup releases the destination. It ran on this host,
  not merely compile-checked; explicitly unavailable io_uring may skip elsewhere.
- Documented that cancellation does not undo transfers or guarantee a queued
  transfer will not run. Completion-mode duplication has syscall overhead;
  throughput has not been measured for this change.
- Validation: 284 root all-feature tests; 167 harness all-feature tests and
  3 doctests; 120 splice-only harness tests and 3 doctests; harness strict Clippy,
  formatting, and whitespace checks pass. This resolves the prior splice
  descriptor-lifetime and unchecked-borrow audit items, not the broader runtime
  shutdown fallback leaks or all remaining package findings.

## Executable I/O documentation and refreshed findings

- Replaced four ignored I/O snippets with references to executable harness
  examples. The old pipe snippet checked an original array copy instead of the
  returned read buffer; the echo loop lost buffer ownership, used `?` without a
  Result return type, and omitted partial-write handling. The copy snippet also
  had an invalid return type, while the buffer snippet hid errors as EOF.
- Added examples for initialized length versus capacity, returned pipe buffers,
  and copying through EOF. The copy example explains producer closure and the
  need for a concurrent consumer when a payload exceeds pipe capacity.
- All-feature doctests pass 6 examples, including live Linux pipe I/O, with 44
  ignored legacy snippets remaining. Default-feature doctests also pass 6, but
  the two Unix/pipe bodies are explicitly cfg-gated out (4 active examples);
  16 default-feature legacy snippets remain ignored. No new API was exposed to
  make the examples compile. Formatting and whitespace checks pass.
- Refreshed `target/qualirs-cleanup-current.json`: 421 repository findings, 410
  in vibeio (73 labeled critical, 336 warning, 1 info). Of vibeio findings, 264
  concern missing unsafe comments, 68 mutable-reference casts, and 60 large
  unsafe blocks. These are analyzer labels, not confirmed defects; they still
  require source-level review, not broad lint suppression. The report's nonzero
  exit status is expected while findings remain.

## Executor join callback and cancellation ownership

- Reviewing executor findings uncovered concrete callback hazards beside the
  flagged unsafe paths: SpawnFuture woke its join waiter under a mutable
  JoinState borrow; JoinHandle cloned/replaced custom wakers under that borrow;
  cancellation destroyed the removed future before its slot borrow ended.
- Completion now releases join state before wake/destruction. Join polling
  preserves the unchanged-waker fast path, clones outside state borrows, and
  rechecks completion after the clone callback before installing a waiter.
  Replaced or redundant wakers drop after releasing state. Cancellation takes
  the future into a local and releases its slot borrow before destruction.
- New regressions exercise waiter replacement and completion callbacks,
  synchronous cancellation destructors inspecting their task slot, and a custom
  clone callback that makes the join result ready during polling. All execute
  locally. The clone regression also checks no waiter remains installed after
  that ready result.
- Validation passes: 287 root all-feature tests, 170 harness all-feature tests,
  6 doctests (44 still ignored), harness strict Clippy, formatting, and whitespace
  checks. No throughput claim is made. The executor's UnsafeCell queue audit
  and broader unsafe-code findings are not declared resolved by these fixes.

## Safe root-task waker

- Replaced BlockOnNotify's manual RawWaker vtable and Arc raw-pointer ownership
  conversions with the standard library's safe Wake interface. The shared notify
  method preserves ready-state updates and cross-thread interrupt coalescing;
  wake_by_ref is overridden so borrowed wakes do not clone the Arc.
- Production executor code no longer imports RawWaker/RawWakerVTable; the custom
  clone-callback regression still uses those APIs inside the test module with
  an explicit stateless-vtable safety explanation. Task's separate borrowed
  WakerRef optimization is unchanged and remains a distinct audit surface.
- Added tests for local borrowed/consuming wake ownership, readiness consumption,
  cross-thread borrowed wakeups, and final-reference release on another thread.
  Using Wake also makes Send + Sync requirements compiler-checked at construction.
- Validation passes: 289 root all-feature tests, 172 harness all-feature tests,
  and 6 doctests (44 ignored); Linux, Windows-target, and macOS-target harness
  strict Clippy; formatting and whitespace checks. Cross-target checks do not
  establish native Windows/macOS execution. Throughput was not measured.

## Safe borrowed task wakers

- Reviewed the locked futures-task 0.3.33 implementation behind futures-util's
  existing task helpers. Its safe waker_ref preserves the borrowed no-refcount
  construction path, while cloning takes an owned reference to the proxy.
- Replaced TaskWake's manual RawWaker vtable and raw Arc conversions with
  ArcWake plus the existing waker/waker_ref helpers. No dependency was added.
  The task module now forbids unsafe code; scheduling, local-future ownership,
  wake deduplication, and the remote proxy queue are unchanged.
- Extended ownership coverage to borrowed wake_by_ref and added repeated-wake
  queue deduplication/requeue assertions. Existing owner-thread future destruction,
  foreign-thread waker release, and unwind tests also pass with the safe helpers.
- Validation: 290 root all-feature tests; 173 harness all-feature tests and
  6 doctests (44 ignored); Linux/Windows-target/macOS-target strict harness
  Clippy; formatting and whitespace checks pass. Non-Linux checks are not native
  execution, and no throughput comparison was performed. This supersedes the
  previous section's open task-vtable audit item, not the rest of the package audit.

## Checked local ready queue and measured tradeoff

- Replaced the executor's uniquely owned Rc<UnsafeCell<VecDeque<_>>> queue with
  a directly owned RefCell<VecDeque<_>>. Enqueue, drain, and emptiness checks no
  longer manufacture references from raw pointers. Draining releases its borrow
  before task polling/callbacks. Corrected the misleading work-stealing claim.
- Enabled deny(unsafe_op_in_unsafe_fn) in the executor module. Its remaining
  production unsafe blocks are the explicitly justified pinned future projections;
  this increment does not establish that every executor invariant is resolved.
- Added FIFO, zero/partial drain-budget, queued-flag, and skip-wait regression
  assertions. Validation passes 291 root all-feature tests, 174 harness tests,
  6 doctests (44 ignored), Linux/Windows-target/macOS-target strict harness
  Clippy, formatting, and whitespace checks. Non-Linux execution remains pending.
- Benchmarked original/candidate binaries before other validation, then ran two
  alternating pinned pairs after tests completed. Results and limitations are in
  benches/vibeio-performance.md. Single-task yield median increases were 2.1%
  and 0.7%; spawn/join varied substantially and batch yield was slightly lower.
  This is a checked-ownership tradeoff, not a demonstrated speed improvement or
  evidence of Python/uvloop performance. Longer controlled measurements remain
  useful for characterizing its cost.

## Bounded platform-version response parsing

- macOS platform initialization now parses only the sysctl response's reported
  slice using safe CStr validation, replacing the unbounded CStr::from_ptr scan.
  Oversized lengths, missing/interior terminators, invalid UTF-8, empty/non-numeric
  majors, and overflowing major numbers return InvalidData. Valid releases below
  the existing macOS 13 minimum still return Unsupported; trailing storage beyond
  the reported length is ignored.
- Windows version-query storage now uses an explicit initialized structure
  instead of unsafe zeroed. Added the FFI lifetime/layout explanations and enabled
  unsafe_op_in_unsafe_fn denial plus undocumented-unsafe warnings for the builder.
  Platform support thresholds and driver selection were not changed.
- Validation: response-parser tests execute on Linux, including malformed cases;
  293 root all-feature tests, 176 harness tests and 6 doctests pass (44 ignored).
  Linux/Windows-target/macOS-target strict harness Clippy, formatting, and
  whitespace checks pass. Native sysctl/RtlGetVersion execution remains unverified.

## In-place timer waiter updates and wall-clock measurement

- Sleep no longer cancels/reinserts a live timer on every spurious poll. Timer's
  update_waker keeps its heap location and generation, avoids unchanged-waker
  clones, rejects stale handles, and releases heap borrows before custom clone
  or drop callbacks. It revalidates the generation after cloning.
- A stable-registration regression failed on the old repoll path and passes now.
  Additional tests cover heap/deadline preservation, stale-slot rejection,
  replaced-waker destruction reentry, and Sleep waiter ownership/cancellation.
- Added benches/timer.rs and its Cargo benchmark target. At the user's request,
  measured optimized original/candidate binaries in three alternating CPU-2
  pairs with 3 warmups and 7 measured samples per workload per run. Pooled
  elapsed reductions were 66.4%/47.0% for one unchanged/changing-waker timer and
  88.2%/81.0% for 1,024 timers. Method, raw artifact paths, and limitations are
  recorded in benches/vibeio-performance.md; no end-to-end speedup is inferred.
- The temporary baseline branch removal was restored before final validation.
  296 root all-feature tests, 179 harness tests, and 6 doctests pass (44 ignored).
  Root all-target/all-feature strict Clippy, formatting, and whitespace checks
  pass, including the new benchmark. Broader package cleanup remains open.

## Datagram source-address length validation

- After the committed cleanup batch, refreshed Qualirs and reviewed recvfrom's
  address decoding. Poll and completion paths previously ignored the returned
  source-address length while interpreting IPv4/IPv6 fields.
- The shared per-platform decoder now rejects lengths smaller than the selected
  address structure or larger than storage capacity. Unix recvfrom, Linux
  recvmsg completion, Windows synchronous WSARecvFrom, and Windows completion
  all pass their returned lengths. Negative Windows lengths cast to oversized
  usize values and are rejected. Added size/alignment/union safety explanations
  beside the address casts; no blanket suppression was introduced.
- Regression tests cover IPv4/IPv6 exact lengths, truncated/empty/oversized
  responses, and usize::MAX. Linux executes these tests; Windows/macOS variants
  are cross-compiled. This hardens parsing of malformed metadata; no normal
  kernel misbehavior or end-to-end vulnerability is claimed from the finding.
- Validation passes: 297 root all-feature tests, 180 harness tests and 6 doctests
  (44 ignored), Linux/Windows-target/macOS-target strict harness Clippy, formatting,
  and whitespace checks. Native non-Linux receive execution remains unverified.

## Recvfrom unsafe-boundary documentation and metadata reuse

- Reviewed the remaining recvfrom unsafe blocks and documented synchronous
  output-storage lifetimes, initialized-prefix publication after successful I/O,
  Winsock error retrieval, retained overlapped storage, and validity of zeroed
  C metadata. Enabled unsafe_op_in_unsafe_fn denial and undocumented-unsafe
  warnings locally; no broad suppression was added.
- Removed duplicate zero-initialization of existing Linux address/header state.
  Initial allocation still initializes all storage; every msghdr input/output
  field is explicitly assigned on each build, and address decoding checks the
  returned length. No performance claim was inferred from removing those writes.
- Added a regression that mutates receive output metadata, rebuilds the SQE,
  and verifies reset lengths/flags/control fields and unchanged boxed metadata
  and buffer addresses. Existing cancellation and address-length tests still pass.
- Validation: 298 root all-feature tests, 181 harness tests, 6 doctests (44
  ignored), Linux/Windows-target/macOS-target strict harness Clippy, formatting,
  and whitespace checks pass. Native Windows/macOS FFI behavior and broader
  findings remain open; these documentation checks are not a proof of all I/O safety.

## Shared accepted-socket address validation

- Moved recvfrom's checked IPv4/IPv6 decoder and length regressions into the
  internal op/socket_addr module. Accept now uses the same decoder and forwards
  getpeername's returned length on Unix and Windows instead of ignoring it.
- Windows AcceptEx no longer reads an entire SOCKADDR_STORAGE from a returned
  pointer. It checks the pointer's offset and signed length against the retained
  output buffer, copies only that checked slice into aligned initialized storage,
  and validates the selected address structure's length. Reads use the original
  buffer's provenance, not the provider-returned pointer. No extra syscall was
  added. This is defensive validation, not a claim of observed provider corruption
  or a measured performance improvement.
- New regressions exercise null/outside/end/overflow-sized addresses, negative,
  zero, truncated and oversized lengths, and unaligned IPv4/IPv6 input ending
  exactly at the buffer boundary. They check nonzero port, IPv6 address, flowinfo,
  and scope preservation. IPv6 field byte order remains consistent with Rust std.
- Validation: 300 root all-feature tests, 183 harness tests and 6 doctests pass
  (44 ignored). Root and Linux/Windows-target/macOS-target strict harness Clippy,
  formatting and whitespace checks pass. Native Windows/macOS execution and the
  remaining package-wide cleanup findings are still open.

## Accept ownership and unsafe-boundary audit

- Windows polling accept now immediately wraps a successful socket in OwnedSocket
  and passes it to a finishing helper. getpeername and address-decoding errors
  release that owner automatically; removed the two separate manual closes.
  Successful completion still transfers ownership exactly once. No previously
  observed socket leak is claimed; the change makes error cleanup structural.
- Removed nested Option wrappers around Winsock's already-optional extension
  function pointers and their duplicate absence checks. Centralized last-error
  retrieval and documented the remaining accept-module unsafe calls, including
  synchronous output lifetimes and retained overlapped buffers. Enabled local
  unsafe_op_in_unsafe_fn denial and undocumented-unsafe warnings.
- Extended the successful ownership-transfer test to Windows and added a bounded
  peer-EOF check after the transferred socket is dropped. The existing owning-
  driver cancellation regression now also compiles on Windows; a Windows-only
  test checks rejection of an unconnected socket. Windows tests are compile/lint
  checked here, not natively executed.
- Validation passes: 300 root tests, 183 harness tests, 6 doctests (44 ignored),
  root strict Clippy, Linux/Windows-target/macOS-target strict harness Clippy,
  formatting and whitespace checks. Broader cleanup remains unfinished.

## Atomic close-on-exec for Unix-domain completion accept

- Found that AcceptUnixOp's Linux SQE omitted SOCK_CLOEXEC and only set FD_CLOEXEC
  after observing the CQE. The returned descriptor was therefore inheritable
  between kernel creation and userspace finishing. Added SOCK_CLOEXEC to the
  submission and removed the now-redundant Linux finishing query/set. Preserved
  blocking completion-mode descriptors; readiness accept4 still requests both
  close-on-exec and nonblocking flags.
- Added a live io_uring regression using a preconnected abstract Unix socket.
  It inspects the accepted descriptor directly from the CQE, without executing
  finishing code. The test failed on the original submission (FD_CLOEXEC was
  zero) and passes with the flag added; it also verifies blocking status remains
  unchanged. This proves the descriptor flag, not an observed child-process leak.
- Shared the existing close-on-exec fallback helper between TCP and Unix-domain
  accept. Platforms without accept4 still set this flag after accept and therefore
  retain their non-atomic fallback limitation. Documented Unix accept unsafe
  boundaries, enabled local safety lints, and added an owning-driver cancellation
  test both outside a runtime and inside a different runtime.
- Validation: 302 root tests, 185 harness tests, 6 doctests (44 ignored), root
  strict Clippy, Linux/Windows-target/macOS-target strict harness Clippy,
  formatting and whitespace checks pass. No wall-clock speedup claim is made
  from removing the finishing syscall. Native non-Linux and wider cleanup work
  remain open.

## UDP completion-send ancillary-header correction

- Auditing the remaining descriptor-creation operations confirmed that file-open
  options and TCP accept submissions already request close-on-exec. Follow-up
  inspection found SendtoOp building a Linux msghdr with a null msg_control but
  msg_controllen = 1, causing send failure rather than a valid no-ancillary send.
- A live loopback io_uring regression returned CQE result -14 (EFAULT) before
  the correction, instead of the eight-byte payload length. Setting ancillary
  length to zero fixes the send; msg_flags is also reset to zero. The test now
  verifies payload and source address for both nonempty and empty datagrams.
- Removed redundant Linux header re-zeroing while explicitly resetting every
  msghdr field. Initial address state uses the already-built address rather than
  a temporary zeroed value. A metadata-reuse regression checks all reset fields
  and stable boxed metadata/payload addresses.
- Documented the sendto module's unsafe address conversions, synchronous payload
  lifetimes and overlapped retention, and enabled local safety lints. A search of
  the remaining production msghdr field assignments found no other nonzero
  ancillary lengths paired with null control pointers.
- Validation: 304 root tests, 187 harness tests, 6 doctests (44 ignored), root
  strict Clippy, Linux/Windows-target/macOS-target strict harness Clippy,
  formatting and whitespace checks pass. This fixes a reproduced Linux send
  error; native non-Linux execution and wider cleanup remain unverified/open.

## Owned, non-inheritable TCP socket construction

- TCP stream and listener constructors used raw socket calls without CLOEXEC
  on Unix. Added direct constructor regressions; both failed with FD_CLOEXEC
  absent before the change and pass afterward. This extends the inheritance
  audit beyond completion submissions to the underlying networking constructors.
- Replaced TCP socket creation with socket2::Socket::new, using the dependency
  already present in rsloop. Listener setup now uses safe owned option/bind/listen
  calls and transfers ownership into std only after success. Removed duplicated
  listener address encoders and manual close/error branches on both platforms.
- Preserved Unix SO_REUSEADDR, Windows default reuse policy, IPv6 dual-stack
  configuration and platform SOMAXCONN. socket2 requests atomic CLOEXEC on Linux,
  non-inheritable overlapped sockets on Windows, and fallback CLOEXEC plus
  NOSIGPIPE on Apple. The Apple CLOEXEC fallback is not atomic. Removed per-socket
  WSAStartup calls; socket2 delegates one-time Winsock initialization to std.
- Added the harness dependency, locking socket2 to 0.6.4 like the root lockfile.
  Tests cover inheritance flags, successful listener transfer, duplicate bind
  rejection, Unix address reuse, and IPv6 wildcard dual-stack settings. Corrected
  the initial IPv6 test to use a wildcard: a local socket experiment confirmed
  Linux forces IPV6_V6ONLY after binding specifically to ::1, even when disabled
  beforehand. Windows inheritance checks compile but have not run natively.
- Validation: 308 root tests, 191 harness tests, 6 doctests (44 ignored), root
  strict Clippy, Linux/Windows-target/macOS-target strict harness Clippy,
  formatting and whitespace checks pass. Wider cleanup and native non-Linux
  runtime verification remain open; no throughput claim is inferred.

## Unix-domain stream construction inheritance and pathname tests

- The remaining direct libc::socket constructor was in Unix-domain streams and
  also omitted CLOEXEC. Its new constructor regression failed with zero
  FD_CLOEXEC before the change. Replaced raw creation/from_raw_fd with socket2
  creation and safe Socket -> OwnedFd -> std UnixStream ownership transfers.
  The regression now passes; Linux gets atomic CLOEXEC, while Apple retains the
  dependency's post-creation flag fallback and NOSIGPIPE setup.
- Added pathname regressions for empty, interior-NUL and overlong rejection,
  the maximum accepted pathname, non-UTF-8 bytes, terminator initialization,
  exact address length and platform sun_len. These do not create filesystem
  entries. Documented why zero-initialized sockaddr_un storage is valid.
- Validation: 311 root tests, 194 harness tests, 6 doctests (44 ignored), root
  and Linux/macOS-target strict Clippy, formatting and whitespace checks pass.
  This turn changes Unix-only code; native macOS execution remains unverified.
  The wider cleanup, including remaining Windows-specific socket paths, is open.

## Owned non-inheritable AcceptEx socket creation

- The separate Windows AcceptEx constructor still used WSASocketW with only
  WSA_FLAG_OVERLAPPED. Switched it to socket2::Socket::new with TCP protocol,
  preserving overlapped I/O while requesting non-inheritance. It now returns
  OwnedSocket directly, removing a raw ownership handoff at submission.
- Listener-family lookup now uses full initialized SOCKADDR_STORAGE and the
  shared length-checked decoder instead of reading a family from an unchecked
  returned address length. Both IPv4 and IPv6 select their matching socket domain.
- Added a Windows-only regression checking IPv4/IPv6 listener-family lookup,
  non-inheritable handles, stream socket type, and invalid-listener rejection.
  This regression is cross-compiled and linted, not natively executed: unlike
  the Linux constructor regressions, no before/after runtime result is claimed.
- Validation: Windows-target strict all-target/all-feature harness Clippy passes;
  Linux/macOS-target harness Clippy, root strict Clippy, 194 harness tests and
  6 doctests (44 ignored), formatting and whitespace checks also pass. Native
  Windows AcceptEx execution and the wider package cleanup remain open.

## Consolidated outbound IP socket-address encoding

- Removed the repeated IPv4/IPv6 encoders in TCP stream construction, UDP
  connect and SendtoOp. All three now use one internal adapter from std SocketAddr
  through socket2::SockAddr into the platform's native storage. The adapter has
  one documented unsafe storage view instead of repeated native writes/copies.
  Public networking APIs and address-storage ownership remain unchanged.
- Added round-trip tests against the independent checked decoder for unspecified,
  nonzero, maximum-port, broadcast, IPv6 global and scoped link-local addresses.
  Tests check exact native lengths, IPv6 flow/scope preservation, and BSD/Apple
  storage length fields. socket2 fills platform length fields instead of the old
  encoders' zero values; native non-Linux behavior remains unverified.
- Validation passes: 312 root tests, 195 harness tests, 6 doctests (44 ignored),
  root strict Clippy, Linux/Windows-target/macOS-target strict harness Clippy,
  formatting and whitespace checks. Existing live UDP completion tests pass.
- Refreshed Qualirs: 296 findings remain under src/vibeio; the largest file counts
  are IOCP (27), connect (24), read (23), recv (18), and recvfrom (17). These are
  audit candidates, not confirmed bugs. The report remains local under target;
  full package cleanup is not complete.

## Connect address validation and borrowed unsafe boundaries

- ConnectOp's internet constructor now uses the checked decoder to reject
  truncated IPv4/IPv6 structures and unsupported families, not just lengths
  outside storage capacity. The Unix-domain constructor rejects non-Unix
  families. Errors remain InvalidInput; valid owned addresses retain their
  original allocation and cancellation lifetime.
- Synchronous connect helpers now borrow ConnectAddress instead of accepting
  unrelated raw pointer/length arguments. Windows ConnectEx binding reads the
  family from owned typed storage, removing its raw pointer dereference.
- Simplified the nested optional ConnectEx pointer cache, documented synchronous
  output storage and overlapped address retention, and enabled module-local
  unsafe_op_in_unsafe_fn denial and undocumented-unsafe warnings. Existing
  Windows bind-error classification was preserved, not newly certified.
- Tests cover exact/truncated internet structures, unsupported families, and
  mismatched Unix families. Live TCP connect and address movement/cancellation
  regressions still pass. This is defensive input validation; no kernel
  memory-safety failure was reproduced or inferred from the original input checks.
- Validation: 313 root tests, 196 harness tests, 6 doctests (44 ignored), root
  strict Clippy, Linux/Windows-target/macOS-target strict harness Clippy,
  formatting and whitespace checks pass. Native Windows/macOS runtime behavior
  and broader cleanup remain open.

## Read completion lengths and retained Windows metadata

The Windows metadata-retention change below is superseded by the later
"Winsock receive descriptor lifetime correction" audit; length checks remain.

- ReadOp now checks capacity before encoding Linux's u32 completion length,
  matching its Windows paths instead of silently truncating a usize. Shared
  conversion tests cover zero, ordinary lengths, u32::MAX, and oversized 64-bit
  values. No multi-gigabyte buffer allocation was needed for these boundary tests.
- Windows receive flags now share boxed completion state with WSABUF, keeping
  both addresses live through cancellation acknowledgement. This is conservative
  lifetime hardening; no native Windows dangling-pointer failure was reproduced.
  A Windows-only test verifies that cancellation retains the original flags
  address and value alongside the payload allocation.
- Documented read's remaining unsafe call/storage boundaries and enabled local
  safety lints. A live Unix pipe test checks a short read into spare capacity,
  EOF clearing the initialized prefix, and an error preserving existing bytes.
- Validation: 315 root tests, 198 harness tests, 6 doctests (44 ignored), root
  strict Clippy, Linux/Windows-target/macOS-target strict harness Clippy,
  formatting and whitespace checks pass. Native Windows/macOS execution and
  remaining package-wide findings are still open.

## Shared scalar completion length checks

- Found the same unchecked usize-to-u32 casts in Linux RecvOp, SendOp and
  WriteOp. All now use the checked completion_len helper shared with ReadOp;
  oversized lengths return InvalidInput rather than wrapping to a smaller or
  zero-length request. Existing Windows and positional I/O checks are preserved.
- Moved the conversion-boundary tests to the shared utility. Added a live
  io_uring socket-pair regression that builds and submits Read, Recv, Send and
  Write SQEs and verifies byte counts and transferred payloads. Each temporary
  ring closes before its caller's operation/buffer, including error unwinding.
  Boundary arithmetic is tested without allocating multi-gigabyte buffers.
- Validation: 316 root tests, 199 harness tests, 6 doctests (44 ignored), root
  strict Clippy, Linux/Windows-target/macOS-target strict harness Clippy,
  formatting and whitespace checks pass. No remaining direct buffer length or
  capacity casts to u32/inferred integer were found in op sources by the targeted
  search; this is not a claim that all size handling or package cleanup is done.

## Vectored descriptor count checks and safe array construction

- Replaced unchecked descriptor-count casts in readv/writev for Unix readiness,
  Linux completion and Winsock paths with a checked conversion to the receiving
  API's integer type. Boundary tests cover signed/unsigned limits without giant
  allocations. Platform IOV_MAX enforcement remains with the OS; this change
  prevents integer wrapping rather than promising arbitrarily many vectors.
- Replaced the duplicated MaybeUninit native-iovec builders with one safe
  iterator-based builder. It copies pointer/length descriptors only, without
  dereferencing payload pointers or changing existing ownership contracts.
- Extended the live io_uring regression with vectored reads ending partway
  through a second buffer (untouched suffix verified) and vectored writes
  containing an empty segment. All buffers remain owned through completion.
- Validation: 317 root tests, 200 harness tests, 6 doctests (44 ignored), root
  strict Clippy, Linux/Windows-target/macOS-target strict harness Clippy,
  formatting and whitespace checks pass. Windows receive metadata lifetime
  review, native non-Linux testing, and wider cleanup remain open.

## Winsock receive descriptor lifetime correction

- Reviewed Microsoft's WSARecv contract: providers capture WSABUF descriptors
  during submission and delayed completion does not update lpFlags. Removed
  unnecessary descriptor retention from ReadOp, RecvOp and ReadvOp, including
  the recently added conservative ReadOp flags box. Payloads, file staging and
  driver-owned OVERLAPPED storage still survive pending I/O and cancellation.
  Source: https://learn.microsoft.com/en-us/windows/win32/api/winsock2/nf-winsock2-wsarecv
- Added a Windows-only IOCP test for all three operations, releasing sender data
  only after the receive returns Pending. It replaces the test that enforced
  unnecessary flags retention. This new test is cross-compiled, not executed
  here. Removed scalar descriptor allocations; no measured speedup is claimed.
- Validation: 317 root tests, 200 harness tests, 6 doctests (44 ignored), root
  strict Clippy, Linux/Windows-target/macOS-target strict harness Clippy,
  formatting and whitespace checks pass. Native Windows execution and overlapped
  peek behavior remain open, as does wider package cleanup.

## Windows file-read EOF normalization

- Audited ReadOp, ReadAtOp and ReadvOp against Microsoft's asynchronous EOF
  contract. ReadAtOp and ReadvOp propagated EOF as an error; ReadOp normalized
  completion EOF but still propagated EOF reported during submission.
- All three now normalize ERROR_HANDLE_EOF from either path to zero bytes.
  Scalar buffers expose an empty initialized prefix; vectored destinations stay
  unchanged and completed file staging is released. Other errors are preserved,
  including the same numeric error code on non-Windows platforms.
  Source: https://learn.microsoft.com/en-us/windows/win32/fileio/testing-for-the-end-of-a-file
- Added error-preservation coverage and a Windows-only native IOCP regression
  for scalar, positional (at and beyond EOF), and vectored empty-file reads.
  The native test has bounded awaits and uses a create-new, delete-on-close
  temporary file. It compiles here but has not been executed on Windows; it
  does not force both immediate and delayed EOF delivery independently.
- Validation: 318 root tests, 201 harness tests, 6 doctests (44 ignored), root
  strict Clippy and Linux/Windows-target/macOS-target strict harness Clippy pass.
  Formatting and whitespace checks pass. Native Windows verification, overlapped
  peek semantics and the broader cleanup remain open. Changes are uncommitted.

## Linux positional-I/O sentinel validation

- ReadAtOp and WriteAtOp passed unsigned offsets directly to io_uring. Linux
  interprets u64::MAX as its current-position sentinel, so a positional request
  could read or overwrite unrelated data and advance the shared file cursor.
  Source: https://www.man7.org/linux/man-pages/man3/io_uring_prep_read.3.html
  Source: https://www.man7.org/linux/man-pages/man3/io_uring_prep_write.3.html
- Added shared signed-64-bit offset validation before either SQE is returned.
  Negative encodings, including the sentinel, now return InvalidInput without
  submitting I/O or changing the caller's buffer. Valid offsets are unchanged.
  This guard is Linux-specific; Windows offset semantics remain a separate audit.
- Added boundary coverage and a live file regression covering both the blocking
  fallback and io_uring. Independently bypassing each new guard reproduced an
  unexpected successful three-byte read/write at u64::MAX. Both guards were
  restored; the test now verifies rejection, intact file contents and unchanged
  shared cursor, alongside successful ordinary positional reads and writes.
- Validation: 320 root tests, 203 harness tests, 6 doctests (44 ignored), root
  strict Clippy, Linux/Windows-target/macOS-target strict harness Clippy,
  formatting and whitespace checks pass. Broader cleanup remains open.

## Buffered adapter interrupted writes

- AsyncWrap acknowledged a write batch before draining it, but discarded the
  remaining owned bytes when the inner writer returned Interrupted. Retrying
  flush could not recover those already accepted bytes. The drain loop now
  retains the returned buffer and retries without advancing its cursor.
- A regression alternates interruptions with two-byte successful writes. It
  failed before the fix with Interrupted; afterward all six accepted bytes are
  delivered exactly once across six attempts and the inner writer is flushed.
- Validation: 338 root tests, 221 harness tests, 7 doctests (44 ignored), root
  strict Clippy and Linux/Windows-target/macOS-target strict harness Clippy pass.
  Windows and macOS checks are cross-target linting, not native execution.
  This change is uncommitted; the broader package cleanup remains open.

## Buffered adapter terminal drain errors

- A non-interrupted write-drain failure discarded the remainder of an accepted
  batch, then allowed shutdown/flush to succeed and new writes to be accepted.
  The adapter now remembers the failure kind and rejects subsequent drains.
  The original failing call retains the complete original error. This is a
  documented terminal-error policy, not recovery of the discarded bytes.
- The regression failed on shutdown before the fix. It now checks repeated
  flush, shutdown, nonempty write and read calls after an inner BrokenPipe,
  zero-byte write, or oversized completion. Reads return the drain error without
  invoking the inner reader or modifying the caller's buffer. Interrupted writes
  retain the retry behavior tested separately.
- Empty read/write calls remain successful no-ops. Flush errors from the inner
  writer are not made terminal: those do not discard this adapter's write batch.
- Validation: Linux root and harness tests and strict Clippy pass; Windows and
  macOS validation remains cross-target linting, not native execution. Broader
  cleanup remains open; these adapter changes are not committed.

## Append/truncate option validation

- OpenOptions allowed append+truncate through its common validation. Standard
  blocking open rejected that combination unless create_new was enabled, but
  the io_uring flags allowed opening and truncating an existing file. Added the
  common validation check before either backend can touch the path.
- A live regression failed on the previous io_uring path with unexpected
  success. It now verifies InvalidInput and unchanged file contents on both
  blocking and io_uring paths. The create_new exception is also checked: an
  existing file returns AlreadyExists and remains intact.
- Validation: 336 root tests, 219 harness tests, 7 doctests (44 ignored), root
  strict Clippy and Linux/Windows-target/macOS-target strict harness Clippy pass.
  Formatting and whitespace checks pass. Changes remain uncommitted and the
  full package audit remains incomplete.

## Exact positional-I/O retry loop

- Centralized read_exact_at and write_exact_at's partial-transfer loop. Both
  now retry Interrupted without advancing the offset or buffer cursor. Zero
  writes return WriteZero instead of UnexpectedEof; short reads ending at EOF
  continue to return UnexpectedEof. Updated the public method documentation.
- Replaced saturating offset advancement with checked arithmetic when another
  operation is needed, and reject impossible byte counts before advancing the
  buffer cursor. The caller's owned buffer is returned on every error path.
- Four deterministic scripted tests exercise both modes: interruptions before
  and after partial progress, offset/remaining-capacity tracking, zero progress,
  overflow, ordinary error preservation, oversized counts and empty buffers.
  Existing live-file and spare-capacity tests also pass. Scripted tests do not
  simulate the kernel writing into uninitialized read buffers.
- Validation: 324 root tests, 207 harness tests, 6 doctests (44 ignored), root
  strict Clippy, Linux/Windows-target/macOS-target strict harness Clippy,
  formatting and whitespace checks pass. Changes remain uncommitted; native
  platform verification and broader package cleanup remain open.

## Integration verification and per-location dispositions

- Rebuilt the release CPython 3.14 extension with `maturin develop --release
  --locked`. The repository Python runner completed 109 tests in 3.593 seconds,
  with 2 skipped. Log: `target/vibeio-cleanup-python-tests.log`. This is the
  unittest discovery suite, not every optional framework's standalone smoke
  script or the complete supported-Python-version matrix.
- Refreshed Qualirs: 265 remaining vibeio findings (Q0069: 1, Q0078: 8,
  Q0082: 3, Q0084: 2, Q0085: 1, Q0087: 137, Q0089: 1, Q0090: 63,
  Q0094: 1, Q0095: 48). Counts are not an audit completion metric.
- Q0074, `net/tcp/listener.rs::TcpListener::accept`: replaced the manual
  success/error mapping with Result::map. Ownership transfer and error cleanup
  are unchanged. The refreshed report no longer includes this finding.
- Q0069, `executor.rs::spawn`: retained the documented panic outside a runtime.
  Returning Result would change this API's contract; silently abandoning the
  task would be incorrect. Added a regression verifying the specific panic and
  that the unpolled future's capture is dropped. This is an intentional API
  precondition, not a suppressed diagnostic.
- Q0089, `io/buf.rs::read_into_buf`, pointer addition at the initialized-prefix
  boundary: retained. IoBuf's unsafe implementation contract guarantees
  initialized length <= capacity and a valid stable allocation; the helper
  zeroes only the spare suffix before constructing a safe mutable byte slice.
  A safe slice cannot first be formed over uninitialized bytes. Existing
  `blocking_read_initializes_spare_capacity_and_tracks_result_length` and
  `blocking_read_errors_do_not_expose_unreported_bytes` tests cover this path.
- Q0082 in `process/reaper.rs::ReapChild::drop`: the normal worker takes the
  mutex on its own thread; the synchronous wait is restricted to worker-spawn
  failure. The resource-exhaustion fallback remains a real blocking tradeoff,
  not a fully resolved finding. Q0082 in `signal/unix.rs::Signal::drop` also
  remains open for contention/lifecycle review.
- The release integration build above preceded the equivalent listener mapping
  simplification and the test-only spawn regression. Final source validation:
  325 root tests, 208 harness tests, 6 doctests (44 ignored), root strict Clippy,
  and Linux/Windows-target/macOS-target strict harness Clippy pass. Native
  Windows/macOS execution and other ledger requirements remain outstanding.
- Networking smoke command: `.venv/bin/python -u benches/workload_matrix.py
  --loops rsloop,uvloop --cpu-affinity 2,3 --json-output
  target/vibeio-cleanup-matrix-smoke.json`. All 13 scenarios completed for both
  loops (26 combinations, three measured runs each), including all idle blocks.
  Log: `target/vibeio-cleanup-matrix-smoke.log`. This was not sustained mode;
  build activity overlapped part of the run, and three process blocks cannot
  establish the idle benchmark's confidence interval. These artifacts establish
  successful workload execution, not a before/after speedup or a README update.

## Unix signal waker clone reentrancy

- Signal::poll_recv cloned a caller-provided waker while holding the listener
  slab mutex. RawWaker clone callbacks can execute arbitrary user code, so
  reentering this state could deadlock even though wake/drop already ran outside
  the lock. Cloning now runs unlocked; polling rechecks the notification counter
  under the lock before installing the new waker. Matching wakers still avoid
  cloning, and replacement/retirement continues to release callbacks unlocked.
- Added an isolated custom-RawWaker regression that checks lock availability
  during clone and drop, publishes a notification during clone, verifies that
  the same poll observes it, and checks unchanged-waker reuse and capture
  release. It uses private signal state without installing a process handler.
- Validation: 326 root tests, 209 harness tests, 6 doctests (44 ignored), root
  strict Clippy, Linux and macOS-target strict harness Clippy, formatting and
  whitespace checks pass. macOS is compilation-only. The signal Drop mutex
  contention finding, process-wide handler races and wider cleanup remain open.

## Windows Ctrl-C listener ownership and notification race

- Windows CtrlC checked its counter before locking for waker registration.
  Dispatch in that interval could leave a pending listener unwoken. Its global
  Vec also retained cancelled listeners and accumulated obsolete wakers when
  a listener moved between tasks; matching wakers were replaced under the lock.
- Each listener now owns one slab slot, removes it on Drop, and retires replaced
  wakers outside the mutex. Counter checking and registration are serialized
  with dispatch. Cloning happens unlocked with a counter recheck afterward.
  Dispatch takes live wakers without invalidating slots and wakes them unlocked.
  Removed unnecessary unchecked pin projection from the Unpin CtrlC future.
- Windows listener-state tests now also compile and execute on Unix by including
  the production Windows module under a test-only module name. Console FFI and
  registration stay Windows-only; no fake Windows API or copied state machine is
  substituted. Tests cover replacement, cancellation, independent listeners
  sharing a waker, broadcast, slot reuse, ownership release, and notification
  during a custom clone callback.
- Validation: 328 root tests, 211 harness tests, 6 doctests (44 ignored), root
  strict Clippy, Linux/Windows-target/macOS-target strict harness Clippy,
  formatting and whitespace checks pass. Native console-handler execution,
  process-wide initialization behavior and wider cleanup remain open.

## Signal FFI boundary checks

- Enabled local unsafe-operation and undocumented-unsafe diagnostics in both
  signal platform modules. Unix sigaction setup now uses individually justified
  unsafe blocks, validates sigemptyset failure, and exposes a safe private
  installation wrapper. Restoration retains its unsafe contract: the saved
  handler must remain valid. Documented that contract explicitly.
- Removed the test-only raw getpid call and documented remaining signal-test
  FFI. Both signal future implementations already use safe pin projection.
- Added repeated SIGKILL/SIGSTOP registration-failure coverage: the OS rejects
  each request with EINVAL and no registry entry remains. The test never sends
  either signal. Existing signal-delivery and pipe tests continue to pass.
- Validation: 329 root tests, 212 harness tests, 6 doctests (44 ignored), root
  strict Clippy, Linux/Windows-target/macOS-target strict harness Clippy,
  formatting and whitespace checks pass. Platform compilation is not native
  execution; external handler replacement and lifecycle races remain open.

## Isolated-feature regression and CI coverage

- A fresh default-feature Clippy check found a regression in the recent
  positional-I/O tests: Linux and Windows test functions imported ReadAtOp
  without requiring the fs feature. Added fs gates to both tests. Production
  feature wiring was unchanged; all-features validation had hidden the error.
- Verified strict all-target Clippy for the default configuration and each of
  fs, process, signal, pipe, stdio, splice and blocking-default independently,
  on x86_64-unknown-linux-gnu, x86_64-pc-windows-gnu and aarch64-apple-darwin:
  24 configurations passed. This covers isolated builds, not all 128 feature
  combinations or native execution on the cross targets.
- Added the same eight isolated checks to each existing native-runtime CI job
  alongside all-feature Clippy and default/all-feature tests. Parsed the workflow
  YAML and checked the new Bash step's syntax. CI has not been dispatched.
- Default root tests pass (260); default/all-feature harness tests, formatting
  and whitespace checks pass. Broader audit requirements remain outstanding.

## Structural pin projection

- Replaced the remaining handwritten get_unchecked_mut/Pin::new_unchecked
  projections in SpawnFuture and Timeout with pin-project-lite projections.
  Both generic futures remain structurally pinned; Timeout still clears its
  Option with Pin::set so cancellation drops the inner future in place.
  This adds no box or allocation to either wrapper.
- Declared the already-locked pin-project-lite 0.2.17 dependency explicitly in
  the root and embedded harness manifests; synchronized the example lockfile.
  No dependency versions were upgraded. Added a spawned !Unpin-future regression
  checking address stability between polling and cancellation drop. Existing
  timeout tests retain their !Unpin and prompt resource-release coverage.
- Validation: 330 root tests, 213 harness tests, 6 doctests (44 ignored), root
  strict Clippy and Linux/Windows-target/macOS-target strict harness Clippy pass.
  Formatting and whitespace checks pass. A source search finds no remaining
  handwritten unchecked pin projections in vibeio; this is not a claim that
  all unsafe code or the package-wide cleanup has been resolved.

## Package-wide lint allowance reduction

- Replaced the embedded module's unsafe_op_in_unsafe_fn allowance with deny.
  Linux, Windows-target and macOS-target all-target/all-feature checks pass;
  the runtime no longer permits implicit unsafe operations inside unsafe bodies.
- Removed its broad unused_imports allowance. Deleted obsolete raw-handle
  conversion imports, restricted completion-length helpers to their platform
  users and moved the read-buffer trait import behind cfg(test).
- Kept explicit allowances on individual public re-exports that private rsloop
  embedding does not consume. Harness-free runtime/timer benchmark inclusions
  have a documented import-only exception: Cargo builds their cfg(test) modules
  but omits #[test] functions. Normal library and test compilation still checks
  those imports. The dead_code allowance remains for the embedded API surface.
- Validation: strict Clippy passes for root default/all-feature all-target builds
  and 24 isolated-feature/platform harness configurations. All-feature harness
  tests (213 plus 6 doctests, 44 ignored), formatting and whitespace checks pass.
  Cross-target checks do not establish native platform behavior. Changes remain
  uncommitted and broader cleanup requirements remain open.

## Documentation verification

- Corrected the signal module example: ctrl_c returns a Result containing the
  future, so registration must be unwrapped before awaiting it. Added a harness
  example with a zero-duration timeout, exercising listener cancellation without
  waiting for external signals. Its body runs with signal enabled; otherwise it
  is gated out. Corrected four stale positional readv/writev syscall claims to
  describe the actual io_uring Read/Write operations.
- Strict all-feature documentation builds pass for Linux, Windows and macOS
  targets after fixing seven Windows broken links: Unix network types are now
  documented conditionally and Unix symlink references use cross-platform
  external documentation links. Added the warning-as-error check to the existing native
  CI jobs; validated the YAML locally, without dispatching remote CI.
- Default and all-feature doctest suites pass with 7 executable examples. The
  remaining ignored example counts are 16 and 44 respectively; those examples
  are not verified by a successful documentation build. Native Windows/macOS
  example execution and broader documentation/API cleanup remain outstanding.

## Vectored operation unsafe boundaries

- Enabled local undocumented-unsafe checks in ReadvOp and WritevOp after
  reviewing synchronous descriptor lifetimes, writable-region ownership,
  Windows staging and completion/cancellation retention. Safety comments now
  identify those contracts at each native call and pointer-to-slice conversion.
- Corrected ReadvOp's copy-pasted writev error labels and clarified the vectored
  buffer traits: readable memory feeds output operations, writable memory
  receives input, and methods return owned descriptors rather than raw arrays.
- Windows file-write staging skips empty segments before constructing slices.
  Extended the Windows IOCP empty-file regression to write nonempty segments
  interspersed with empty ones and read back the exact concatenation. It is
  cross-compiled here, not executed; existing Linux live vectored I/O tests pass.
- Validation: 213 harness tests, 7 doctests (44 ignored), root strict Clippy,
  Linux/Windows-target/macOS-target strict harness Clippy, formatting and
  whitespace checks pass. No measured performance claim; broader cleanup and
  native Windows verification remain open.

## Windows positional-write append sentinel

- Microsoft's WriteFile contract assigns Offset=OffsetHigh=0xFFFFFFFF the
  special meaning "append at EOF". WriteAtOp previously encoded u64::MAX
  unchanged, so a positional request could append rather than fail.
  Source: https://learn.microsoft.com/en-us/windows/win32/api/fileapi/nf-fileapi-writefile
- Added a shared Windows sentinel guard in both low-level submission and
  File::write_at before choosing completion/offloaded/blocking paths. Invalid
  requests retain their input buffer; empty public writes still perform no I/O.
  This guard rejects only the special sentinel, leaving other offsets to the OS.
- The boundary test executes on Linux as well. Extended the Windows native-file
  regression to check the error, returned bytes and unchanged file length after
  attempting a sentinel write. That native regression compiles but has not run
  on Windows. Existing owning-driver cancellation coverage is unchanged.
- Enabled local undocumented-unsafe checks in WriteAtOp and documented offset
  splitting and payload/OVERLAPPED lifetime boundaries. Validation: 331 root
  tests, 214 harness tests, 7 doctests (44 ignored), root strict Clippy and
  Linux/Windows-target/macOS-target strict harness Clippy pass. Formatting and
  whitespace checks pass. Broader cleanup remains open and changes uncommitted.

## Vectored emptiness contract

- Reviewed the high-level vectored read early-return checks. IoVectoredBuf's
  is_empty intentionally describes the absence of descriptors, not a zero sum
  of initialized lengths: IoVectoredBufMut may provide spare writable capacity
  through different descriptor lengths. Changing the check to initialized-byte
  emptiness would silently skip valid reads. Kept behavior and clarified docs.
- Added a custom owned buffer regression with zero readable bytes and eight
  spare writable bytes. Its default is_empty returns false, and a live Mio
  Unix-stream read receives and verifies three bytes. Also checked absent
  descriptors versus a collection of empty segments. This is contract coverage,
  not a claim that a previously observed runtime defect was fixed.
- Validation: 215 harness tests and 7 doctests (44 ignored) pass; the extended
  live regression passes separately. Linux/Windows-target/macOS-target strict
  harness Clippy and whitespace checks pass. Windows compiles the portable
  descriptor checks; the socket portion is Unix-only. Cleanup remains open.

## Copy interruption handling

- copy previously propagated Interrupted immediately from reads, partial writes
  and its final flush. It now retries those operations, retaining the returned
  read buffer or current write cursor without claiming progress. Other errors
  still terminate the copy; previously written data is not rolled back.
- Added a scripted regression that interrupts every read/write attempt before
  allowing the next attempt, including between single-byte writes, and interrupts
  the first flush. The old implementation failed with Interrupted; the new one
  verifies exactly abc, three copied bytes and exact read/write/flush call counts.
- Validation: 333 root tests, 216 harness tests, 7 doctests (44 ignored), root
  strict Clippy and Linux/Windows-target/macOS-target strict harness Clippy pass.
  Formatting and whitespace checks pass. This does not address every remaining
  runtime or platform audit item; changes remain uncommitted.

## Vectored forwarding through adapters

- Box, mutable-reference and owned split-half adapters forwarded scalar I/O but
  inherited the Unsupported defaults for vectored methods. Added direct
  read_vectored/write_vectored forwarding to all six implementations. Underlying
  results and owned buffers are returned without copying or scalar fallback.
- Added a vectored-only test object whose scalar methods panic. Through each
  adapter, the test verifies the underlying read error, successful byte count,
  buffer contents and original allocation identity. It failed on the old boxed
  read with Unsupported instead of PermissionDenied; all paths now pass.
- Split halves still serialize access through their documented whole-object
  async mutex; forwarding does not claim to fix that full-duplex limitation.
- Validation: 334 root tests, 217 harness tests, 7 doctests (44 ignored), root
  strict Clippy and Linux/Windows-target/macOS-target strict harness Clippy pass.
  Formatting and whitespace checks pass. Changes remain uncommitted; broader
  package cleanup remains open.

## File write helper reuse

- fs::write allocated a fresh copy of the remaining slice after every partial
  write and maintained a separate loop that returned Interrupted immediately.
  It now copies the input once and delegates to File::write_exact_at at offset
  zero. The newly created/truncated file's internal cursor is not observable
  after this helper returns. Creation, truncation and final flush are preserved.
- Added live blocking-fallback and io_uring coverage for a 128 KiB binary
  payload, overwrite with shorter NUL-containing bytes, and empty truncation.
  Test-created files have scoped cleanup. Interruption and partial-write behavior
  is covered by the exact-write loop's existing scripted tests.
- Validation: 335 root tests, 218 harness tests, 7 doctests (44 ignored), root
  strict Clippy and Linux/Windows-target/macOS-target strict harness Clippy pass.
  Formatting and whitespace checks pass. No wall-clock speedup is claimed;
  broader cleanup remains open and changes are uncommitted.

## Windows symlink paths and argument order

- The cross-platform Windows symlink functions passed (source, destination)
  into helpers whose native argument order was (link, target), and converted
  OsStr paths through lossy UTF-8 strings. Replaced those paths with standard
  Windows symlink_dir/symlink_file calls using their (original, link) order.
  Offloaded operations own PathBuf values without lossy conversion.
  Source: https://doc.rust-lang.org/std/os/windows/fs/fn.symlink_file.html
- Retained the legacy string helpers' link-first signature, delegating safely
  with reversed arguments to std. Removed manual UTF-16 termination and raw
  CreateSymbolicLinkW calls; standard path conversion rejects embedded NULs.
- Windows tests cover NUL rejection plus actual file/directory link placement,
  unchanged source contents and an unpaired-surrogate source path. The live test
  first probes standard-library capability and explicitly reports an unavailable
  check for ERROR_PRIVILEGE_NOT_HELD; other probe failures remain failures.
  Native tests are cross-compiled, not executed here; offload execution remains
  unverified on Windows.
- Validation: 218 Linux harness tests, 7 doctests (44 ignored), root strict
  Clippy, Windows/macOS-target strict harness Clippy, strict Windows documentation,
  formatting and whitespace checks pass. Broader cleanup remains open.
