# Remaining Qualirs finding dispositions

Rescan before the RecvOp follow-up: **231** diagnostics (106 Q0087, 63 Q0090,
44 Q0095 and 18 in other rules). Older counts below remain historical snapshots.

Latest rescan after safe file/pipe/socket ownership conversions: **242** findings
(117 Q0087, 63 Q0090, 44 Q0095, and 18 findings in the other rules).
The table below retains its earlier snapshot locations; these are historical
identifiers, not current line numbers. The new count is not evidence that the
remaining bulk unsafe findings have been reviewed.

Snapshot: 2026-09-06, based on `6488a7c` plus the uncommitted AsyncWrap fixes.
Regenerate with `qualirs . --config qualirs.toml --format json --output
target/qualirs-cleanup-current.json`. Filter `smells` by `location.file`
containing `src/vibeio/`. The snapshot contains 255 findings; diagnostics are
not unique defects. Line numbers below identify this snapshot, not permanent
anchors. No rules were disabled for this review.

## Non-bulk findings reviewed

| Rule | Location under src/vibeio | Disposition and evidence |
| --- | --- | --- |
| Q0069 | executor.rs:394, spawn | Intentional documented panic outside an entered runtime. The function returns JoinHandle, not Result. Keep the public contract; not a newly discovered runtime failure. |
| Q0078 | fs/mod.rs:587, Linux rename | Conditional blocking is real: completion driver uses RenameOp; fs offload moves both owned paths into spawn_blocking; only the documented final fallback calls std synchronously. |
| Q0078 | fs/mod.rs:639, non-Linux rename | Same intentional offload/fallback policy, without the Linux completion branch. Not evidence of blocking inside the worker's caller. |
| Q0078 | fs/mod.rs:669, Linux remove_dir | Completion uses UnlinkOp with the directory flag; offload owns the path; final fallback is synchronous. Retained documented behavior. |
| Q0078 | fs/mod.rs:710, non-Linux remove_dir | Offload owns the path; only the no-offload branch calls std directly. Retained documented behavior. |
| Q0078 | fs/mod.rs:737, Linux remove_file | Completion uses UnlinkOp without the directory flag; offload owns the path; final fallback is synchronous. Retained documented behavior. |
| Q0078 | fs/mod.rs:776, non-Linux remove_file | Offload owns the path; final fallback is synchronous. Retained documented behavior. |
| Q0078 | fs/mod.rs:805, Linux create_dir | Completion uses MkDirOp; offload owns the path; final fallback is synchronous. Retained documented behavior. |
| Q0078 | fs/mod.rs:847, non-Linux create_dir | Offload owns the path; final fallback is synchronous. Retained documented behavior. |
| Q0085 | io/util.rs:30, copy | False attribution: copy has no lock or guard. The same file's split-half methods intentionally hold a futures_util async mutex across I/O. That separate full-duplex limitation is documented and is not dismissed by this disposition. |
| Q0089 | io/buf.rs:535, read_into_buf | Required initialization of spare capacity before forming a safe mutable byte slice. IoBuf contracts require initialized length <= capacity and exclusive writable storage; the offset may be one-past only for a zero-byte initialization. Preserve the bounded pointer operation. |
| Q0094 | io/buf.rs:370, IoBufTemporaryPoll Send | Missing-comment diagnostic is false: the immediately preceding SAFETY comment states the unsafe constructor's polling-thread/borrow restrictions. This only closes the missing-documentation claim, not a proof of every constructor call's lifetime discipline. |
| Q0082 | process/reaper.rs:51, ReapChild worker lock | Lock runs in the newly spawned fallback thread, not on the dropping thread. The worker takes ownership then waits. The analyzer attributes the nested closure to Drop. |
| Q0082 | process/reaper.rs:57, ReapChild fallback lock | Open exceptional blocking path: thread creation failed, so Drop recovers the child and synchronously waits to avoid silently abandoning reaping. The important hazard is wait, not ordinary contention on this private mutex. |
| Q0084 (first) | process/reaper.rs:572, zombie_reaper_fn_unix | False positive: rx is async_channel::Receiver and rx.recv() constructs an async receive future passed to futures_util::select. No blocking channel receive here. |
| Q0084 (second) | process/reaper.rs:572, zombie_reaper_fn_unix | Same source expression also calls Signal::recv(), an async signal wait. Neither select input is a blocking channel operation. |
| Q0082 | signal/unix.rs:256, Signal::drop | Open synchronization constraint: removes its slab entry under a std mutex, then unregisters the signal. Retired wakers are dropped after unlocking, but cross-thread mutex contention and registry synchronization still require review. |
| Q0082 | signal/windows.rs:94, CtrlC::drop | Open synchronization constraint: removes its slab entry under a std mutex. Retired waker drops occur after unlocking; cross-thread contention remains possible. Portable state tests are not native console lifecycle verification. |

## Still requiring per-location review

### RecvOp buffer boundary

The Windows synchronous receive helper now borrows IoBufMut directly rather
than constructing a temporary MaybeUninit slice from raw parts. Capacity is
validated before pointer extraction; successful receive still initializes only
the reported prefix afterward. Added local contracts for synchronous receive,
completion-prefix initialization and error queries, and enabled module-local
undocumented-unsafe Clippy enforcement. Corrected the non-socket error message:
WSARecv is not restricted to listening sockets.

Validation: 238 Linux harness tests and Linux/Windows/macOS all-feature harness
Clippy pass. Native Windows execution remains outstanding. The previously noted
overlapped MSG_PEEK compatibility question is not resolved by this refactoring.
Changes remain uncommitted.

### SendOp descriptor lifetime

Scalar WriteOp follow-up: removed its boxed WSABUF and cancellation metadata
retention, using the same documented capture guarantee. Its socket readiness
helper now consumes an IoBuf borrow directly, eliminating the intermediate
unsafe slice construction. File WriteFile payload retention is unchanged.
Documented the remaining synchronous/overlapped write boundaries and enabled
module-local undocumented-unsafe Clippy checking. Linux harness tests (238),
Windows/macOS/Linux all-feature harness Clippy and formatting pass; native
Windows execution and performance measurements remain outstanding.

Vectored follow-up: WritevOp no longer converts its temporary WSABUF Vec to a
boxed slice or stores that descriptor array through completion/cancellation.
The same WSASend capture contract applies. It still allocates the temporary
descriptor Vec; this is not an allocation-free claim. Owned payload vectors and
Windows file-write staging remain retained. A Windows-only regression inspects
mock cancellation storage and verifies the staging allocation's address/content
and original payload are preserved. The regression compiles under Windows
Clippy but has not executed natively. Linux harness tests (238) and strict
Linux/Windows/macOS-target harness Clippy pass; changes are uncommitted.

Reviewed all five unsafe sites in op/send.rs: synchronous WSASend, its
WSAGetLastError query, Unix send, overlapped WSASend, and its error query.
Added local safety explanations and module-local undocumented-unsafe enforcement.
Removed SendOp's boxed WSABUF and its cancellation-retention field: the descriptor
is now a stack value, while CompletionBuffer and the driver retain payload and
OVERLAPPED storage respectively. The existing pending-buffer ownership regression
continues to pass.

[Microsoft's WSASend remarks](https://learn.microsoft.com/en-us/windows/win32/api/winsock2/nf-winsock2-wsasend)
explicitly allow stack WSABUF arrays because the provider captures descriptors
before return. This does not permit early release of payload or OVERLAPPED.
Validation: 238 Linux harness tests, strict root/harness Clippy and Windows/macOS
cross-target Clippy pass. Native Windows execution remains outstanding; no
wall-clock performance improvement is claimed from removing this allocation.

### Accept-registration teardown reentrancy

Reviewed UringDriver::deregister_handle: its ring borrow ends before registration
removal, and the temporary state borrow ends before the removed registration is
dropped. No production change was needed. A new regression registers a real
listener and installs an owned queued socket plus a waker with a custom Drop.
On handle destruction that callback successfully mutably borrows both driver
state and ring, runs exactly once, and the queued socket's peer observes EOF.
The callback is why a narrowly justified manual_noop_waker allowance is needed;
Waker::noop would not exercise destruction. No broader lint was disabled.

Validation: 355 root tests, 238 harness tests, strict root/harness Clippy and
formatting pass. This establishes the local teardown callback boundary, not
correctness of every cancellation/shutdown interleaving. Changes are uncommitted.

### Negative completion decoding

The decoder now lives beside CompletionIoResult in driver/mod.rs, with an
operation-module re-export. The remaining direct negation in io_uring's
multishot accept queue also uses it. A Linux driver-level regression registers
a real listener, injects i32::MIN and -ECONNABORTED into its accept-result queue,
and verifies InvalidData followed by the preserved OS error without panicking.
It tests dispatch of synthetic error completions, not kernel production of
malformed values. Updated validation: 353 root tests, 236 harness tests, strict
root/harness and Windows/macOS cross-target Clippy pass.

Operation decoders directly negated negative i32 completion results. i32::MIN
has no positive i32 counterpart: direct negation can panic with overflow checks
or retain an invalid negative OS error value otherwise. Added a shared checked
decoder and migrated all 22 direct `from_raw_os_error(-result)` operation sites.
Ordinary positive OS error numbers are unchanged; malformed representations
return InvalidData. Read operations still normalize Windows EOF after decoding.

Linux tests exercise representative OS codes including EOF's numeric code,
i32::MAX, i32::MIN, zero and invalid positive inputs. Validation: 352 root
tests, 235 harness tests, 8 documentation checks, strict root/default/all-feature
harness Clippy and Windows/macOS cross-target harness Clippy pass. This is
defensive result handling, not proof that every driver completion is well-formed.
Changes remain uncommitted.

### IOCP successful byte-count conversion

Submission-order follow-up: io_uring Recv/Send/Write previously evaluated the
buffer-pointer argument before validating the length argument in the opcode
constructor. Validation now occurs first, matching Read and positional file
operations' length handling. The previous code did not submit rejected SQEs;
this change makes the validation boundary explicit before pointer extraction.
Existing live SQE transfer tests pass. Boundary tests validate arithmetic without
allocating giant buffers; they do not prove a real multi-gigabyte transfer.

Follow-up: scalar native length validation now uses the shared signed-result
limit (i32::MAX), including positional file I/O and Windows scalar socket/file
paths. Windows vectored completion submission checks the aggregate length;
file staging checks the same limit before allocating. Oversized requests return
InvalidInput before submission rather than completing unrepresentable I/O.
Existing Windows scalar readiness helpers use the same conservative limit.
Callers must split larger requests; this deliberately tightens the previous
u32-only validation and does not widen CompletionIoResult.

Linux-executed tests cover scalar boundary values, vectored signed-limit sums,
and usize accumulation overflow without allocating giant buffers. Validation:
351 root tests, 234 harness tests, 8 documentation checks and strict root,
Linux/Windows-target/macOS-target harness Clippy pass. Native Windows execution
remains outstanding. The historical decoder-only limitation below describes
the state before this submission-size follow-up.

completion_result_from_entry cast the native u32 successful count to i32,
which is also the shared driver's error representation. A 2 GiB completion
therefore became i32::MIN (unsafe to negate in error decoding); u32::MAX became
-1, an unrelated error. Replaced that cast with checked conversion and a
deterministic negative ERROR_ARITHMETIC_OVERFLOW when the count cannot fit.

Added Windows unit cases for zero, one, i32::MAX, i32::MAX+1 and u32::MAX.
Windows all-target/all-feature Clippy compiles these tests; they have **not**
executed natively here. This is defensive error decoding, not support for
successful transfers above i32::MAX. Such a transfer may already have side
effects before the overflow is reported. Submission limits or a wider shared
completion representation still require review; the large-transfer issue is
not fully closed by this guard.

### AFD poll allocation and cancellation follow-up

Reviewed driver/iocp.rs `arm_poll_operation` and `cancel_poll_operation`:

- Replaced zeroed::<AfdIoStatusCtx>() with explicit IO_STATUS_BLOCK::default()
  and the actual slab token. The context remains boxed and repr(C), with its
  status field first; completion-token recovery is unchanged.
- Before NtDeviceIoControlFile, poll_ops owns all three boxed allocations:
  status/context, input AfdPollInfo, and output AfdPollInfo. Added local safety
  comments identifying those allocations and their exact submitted sizes.
- NtCancelIoFileEx is passed the retained status address and a separate local
  cancellation-result structure. It does not remove the poll entry. Completion
  processing reads the token, checks registration generation, removes the poll
  entry, and only then handles the applicable registration's waiter.

This establishes the local ownership path, not the native API's complete
completion/cancellation behavior. Immediate-failure classification, teardown,
and acknowledgement under native Windows still need full verification. Windows
all-target/all-feature Clippy, formatting and whitespace checks pass; this is
not a claim that Windows tests ran.

### Positional-read submission follow-up

Q0087 at op/readat.rs snapshot lines 89, 111 and 116 now has local safety
contracts and module-local undocumented-unsafe Clippy enforcement:

- Completed-prefix initialization follows successful kernel completion; errors
  return before changing length and Windows EOF is normalized to zero.
- The IOCP driver allocates and stores a boxed OverlappedCtx before submission;
  ReadAtOp writes its two offset words before issuing ReadFile.
- ReadFile borrows the enclosing file handle and CompletionBuffer storage;
  pending operation destruction transfers that storage to the handle's owning
  driver. The cancellation regression verifies buffer retention, not native
  Windows cancellation acknowledgement.

While tracing the IOCP allocation, replaced zeroed::<OverlappedCtx>() with
explicit fields and OVERLAPPED::default(), already used in the driver's tests.
This removes an unnecessary unsafe initialization, without changing layout or
claiming the wider IOCP shutdown audit is complete. Windows/macOS validation is
cross-target linting; Linux harness tests pass.

The 128 Q0087, 63 Q0090, and 46 Q0095 diagnostics are not disposed of by this
table. Existing local safety comments and passing Clippy are supporting evidence,
not substitutes for tracing storage ownership through cancellation and shutdown.
See [the cleanup ledger](vibeio-cleanup.md) for prior fixes, platform limitations,
and the full completion requirements. This inventory is deliberately incomplete.
