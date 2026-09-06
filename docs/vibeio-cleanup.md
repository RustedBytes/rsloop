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
