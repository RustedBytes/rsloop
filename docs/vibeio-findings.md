# Remaining Qualirs finding dispositions

Rescan before the eventfd retry change: **196** diagnostics (73 Q0087, 63 Q0090,
44 Q0095 and 16 other findings). The reduction follows direct safety comments
on the executor's two stateless test callbacks; no analyzer rules were disabled.

Earlier rescan after reaper fallback extraction: **198** diagnostics (75 Q0087,
63 Q0090, 44 Q0095 and 16 other findings). The two fewer Q0082 reports result
from moving the lock into a helper, not eliminating Drop's fallback blocking.
That limitation remains explicitly open below; no rules were disabled.

Earlier rescan after the IOCP detachment review and cross-platform safety-comment
gate: **200** diagnostics (75 Q0087, 63 Q0090, 44 Q0095 and 18 in other rules).
No analyzer rules were disabled. This includes recorded false positives and
still-open reviews; it is not a count of confirmed defects. The detailed older
snapshots below retain their historical line numbers and counts.

Earlier rescan after owned open/accept results and the connect review: **215**
diagnostics (90 Q0087, 63 Q0090, 44 Q0095 and 18 in other rules). No rules were
disabled. This includes findings with recorded dispositions, not 215 proven bugs
or an assertion that the full remaining inventory has been reviewed.

Earlier rescan after the kqueue ownership follow-up: **220** diagnostics
(95 Q0087, 63 Q0090, 44 Q0095 and 18 in other rules). Findings remain enabled;
the per-location dispositions below do not remove them from analyzer output.

Rescan before the RecvOp follow-up: **231** diagnostics (106 Q0087, 63 Q0090,
44 Q0095 and 18 in other rules). Older counts below remain historical snapshots.

Earlier rescan after safe file/pipe/socket ownership conversions: **242** findings
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

### Partial kqueue deregistration also retires cached readiness and waiters

The retryable-deletion change initially updated installed flags alone. Successful
deletions now also clear cached readiness and take their wakers; failed filters
keep all three pieces of state. Waker destruction occurs outside state borrows
on success and error. Failure-combination fixtures verify this distinction.
macOS cross-Clippy passes; native execution is still outstanding.

### Kqueue event filtering after partial deregistration

wait_events now delegates to Registration::record_readiness after validating
token/generation. The helper ignores events for filters marked unregistered,
which matters when a failed deregistration preserves a token after deleting only
one filter. A state-machine test covers both flags independently and verifies
ignored events neither latch readiness nor consume wakers. Cross-Clippy passes;
native stale-event delivery is not reproduced or asserted by this fixture.

### Kqueue deregistration no longer discards retry state on failure

deregister_with now snapshots the installed filters, attempts both deletions,
records successful ones and removes the token only on full success. A failed
rebind therefore keeps bookkeeping for remaining filters instead of leaving a
token that points to a removed slab entry. Tests cover all three combinations
of deletion failure and verify selective successful retry, including kernel
filter absence afterward. They compile under macOS strict Clippy but have not
run natively. This supersedes the earlier intentional-retirement-on-error policy;
permanent Drop-time errors and failed initial-registration rollback remain open.

### Kqueue's two Q0095 reports and interrupted filter deletion

The 196-finding snapshot flags apply_changes (147) and wait_events (220). Each
unsafe block contains one kevent call with a preceding local contract: bounded
live changelist input with no output, or a fixed-capacity event output with a
live optional timespec and no changelist. Splitting either argument list would
not improve safety. Output iteration is restricted to the returned count.

Reviewing rollback identified that EV_DELETE interruption returned immediately.
delete_filter now retries Interrupted iteratively; existing absent/invalid-target
normalization handles the case where an interrupted deletion already succeeded.
A fault-injection test covers 100,000 interruptions followed by success, ENOENT
or EIO, preserving the terminal error. macOS cross-Clippy compiles this test;
it has not run natively. Non-interruption kernel deletion failures can still
leave filters until descriptor/queue close and remain an explicit open concern.

### io_uring teardown evidence: direct Drop as well as explicit quiescence

The live read-shutdown fixture now checks direct UringDriver destruction in
addition to manual quiesce followed by destruction, for both queued and submitted
reads. Retained buffers are released on normal teardown; explicit quiescence
checks the canceled CQE before release. Both live shutdown tests (read retention
and overflowed descriptor results) execute successfully on Linux. This closes a
test-coverage gap, not the separate exceptional-quiescence-error/leak proof or
all shutdown interleavings.

### UringDriver: six reports in the 196-finding snapshot

| Reports/location | Local disposition |
| --- | --- |
| Q0087, 71 | eventfd write uses an upgraded owning Arc and initialized eight-byte value. Its safety comment is retained at the call; the path now shares iterative Interrupted retry with Apple wake notifications. |
| Q0087, 122 | Completion::drop closes only an unclaimed nonnegative fd result. A consumer takes the result before removal; the existing comment documents single ownership. |
| Q0087, 390 | SQ push copies an entry whose referenced allocations remain with the operation/driver. The existing contract covers cancellation retention; this local explanation is not a proof of all teardown paths. |
| Q0087, 520 | Accept CQE's fresh descriptor is immediately owned by OwnedFd and either queued or dropped. Existing ownership comments apply. |
| Q0090, 528/1090 | Option::as_mut reborrows multishot-accept state through an exclusive registration borrow. No raw-pointer reference conversion occurs. |

The wake retry helper already tests 100,000 injected interruptions followed by
success, WouldBlock or a terminal error. Sharing it adds retry to Linux eventfd
without recursive stack growth. A full nonblocking wake source remains treated
as already notified. No native lost-wakeup reproduction or speedup is claimed.

### Socket address conversion: four Q0087 reports in the 198-finding snapshot

| Location | Local disposition |
| --- | --- |
| 19, socket_addr_to_raw | The existing comment describes socket2 storage's platform-native type and copying the initialized address while its owner is live. |
| 54, Unix IPv4 decode | Family and minimum/maximum returned length are checked before borrowing sockaddr_in from sufficiently aligned/sized sockaddr_storage. The preceding comment exists. |
| 68, Unix IPv6 decode | The same checked family/length and storage alignment contract applies to sockaddr_in6. The preceding comment exists. |
| 163, socketaddr_from_buffer copy | Checked integer subtraction/addition select a slice within the owned output buffer. Copying bounded initialized bytes into fresh aligned zeroed storage permits unaligned provider output without dereferencing the provider's integer address. The comment explains bounds and non-overlap. |

Expanded native length tests now reject every undersized length, accept every
length from the native structure size through storage capacity, and reject an
unsupported family. Existing fixtures cover out-of-buffer pointers, signed
length failures, unaligned IPv4/IPv6 and full address-field round trips. Passing
these pure conversion tests does not validate external providers' whole I/O
lifecycles.

### Concurrent lazy reaper initialization: reproduced and fixed

current_zombie_reaper previously released its cache borrow, spawned an async
initializer and awaited it. Two initial requests could each start a reaper and
return distinct senders, with only the last cached. A two-request join regression
failed before the change with "duplicate reapers were created". Startup now
creates the channel and queues the reaper synchronously under the cache borrow;
it executes no user future and cannot suspend before publication. The test and
existing canceled-wait cleanup pass on Linux. This supersedes the earlier open
duplicate-initializer concern while leaving broader teardown verification open.

### Executor: all seven reports in the 198-finding snapshot

| Reports/location | Local disposition |
| --- | --- |
| Q0069, 392, spawn | Missing runtime is a documented caller-precondition panic; an existing test verifies the message. Replacing it with a silently dropped task or changing the public return type would change the API contract. |
| Q0087, 1051/1055, test RawWaker callbacks | Added direct safety comments to the stateless clone/ignore functions. They neither read the null data pointer nor own an allocation; clone invokes only the current thread's test hook. Waker construction already had its own contract. |
| Q0090, 718/797 | Pin::as_mut reborrows the pinned root/task future. Neither operation performs a raw-pointer cast or moves the pinned pointee. Task storage is taken out of its RefCell before polling user code. |
| Q0090, 846 | Safe mutable dereference of a RefMut to take the task slab during shutdown. The borrow ends before detached task futures are dropped, permitting reentrant cleanup. |
| Q0090, 1378 | Pin::as_mut on the test's pinned oneshot receiver. This is an ordinary safe pin reborrow. |

The existing self-cancellation test covers the SpawnFuture wrapper's post-poll
cancellation check, which applies to both executor polling loops. The reentrant
clone test covers join completion during waker cloning. A redundant second
channel-sender clone in current_zombie_reaper was removed; the first clone already
detaches the returned sender from its RefCell borrow. Concurrent lazy reaper
initialization and full shutdown interleavings remain separate audit concerns.

### ReadAtOp: all ten reports in the 198-finding snapshot

| Reports/location | Local disposition |
| --- | --- |
| Q0087, 97 | set_buf_init follows acknowledged completion and error/EOF conversion. The existing safety comment covers stable capacity, initialized prefix and zero-length Windows EOF. |
| Q0087, 123 | The two offset-word assignments use the driver-provided OVERLAPPED before submission. The preceding comment describes exclusive live storage and subsequent driver ownership. |
| Q0087 + Q0095, 133 | One ReadFile call with checked capacity, stable CompletionBuffer storage and retained OVERLAPPED. Its multi-line local comment exists; keep the single FFI call together. |
| Paired Q0090, 92, 104, 163 | Six ordinary Option/CompletionBuffer::as_mut reborrows in completion decoding, Windows submission and Linux entry construction. These are safe reborrows, not raw-pointer as_mut operations. |

Linux entry creation checks the signed positional range before building the
SQE. Existing positional-offset boundary tests and the native positioned-I/O
fixture cover invalid offsets, large sparse offsets, unchanged shared cursor,
EOF and read errors with retained buffers. Windows EOF and offset handling have
cross-checked fixtures but still need native execution. ReadAtOp/WriteAtOp's
non-completion-platform dead-code exemption now applies only to offset rather
than hiding unused fields across the entire struct.

### WriteOp, SendOp and SendtoOp: 13 snapshot reports

| Reports/location in the 198-finding snapshot | Local disposition |
| --- | --- |
| WriteOp Q0095, 45 | One synchronous WSASend invocation with initialized payload, checked byte length, local descriptor/output and null OVERLAPPED. Keep its argument list intact. |
| WriteOp Q0087 + Q0095, 200 | One overlapped WSASend. The existing comment documents descriptor capture and CompletionBuffer/driver retention of payload and OVERLAPPED. |
| WriteOp Q0087 + Q0095, 236 | One WriteFile invocation with checked length and stable initialized payload; the driver owns the completion context. The preceding safety comment is present. |
| SendOp Q0087 + Q0095, 45 | One synchronous WSASend with the same live payload/local output contract; an existing comment precedes it. |
| SendOp Q0087 + Q0095, 203 | One overlapped WSASend with captured WSABUF metadata and retained payload/context. Existing comment references the capture contract. |
| SendtoOp Q0095, 47 | One synchronous WSASendTo; destination address, descriptor and outputs remain live through the call. |
| SendtoOp Q0095, 138 | One synchronous Unix sendto with initialized payload and correctly sized encoded destination address. |
| SendtoOp Q0087 + Q0095, 244 | One overlapped WSASendTo using boxed destination metadata and stable payload. Both are transferred to the owning driver on pending cancellation; the existing comment covers this. |

Completion decoding clears the operation token before returning an acknowledged
result; negative results remain errors and positive partial counts are preserved.
Linux Sendto installs address/iovec/header pointers only after boxing its state.
The three poll paths now directly return poll_result_or_wait instead of matching
and reconstructing every variant unchanged. Existing native sendmsg and workload
tests cover ordinary I/O; mock cancellation tests cover owning-driver retention.
Native Windows cancellation and complete shutdown proofs remain separate work.

### WritevOp: all seven reports in the 198-finding snapshot

| Reports/location | Local disposition |
| --- | --- |
| Q0087 + Q0095, 50 | One synchronous WSASend call with checked descriptor lengths/count and a local byte-count output. The preceding comment documents initialized stable payloads and synchronous lifetimes. |
| Q0087 + Q0095, 230 | One overlapped WSASend call. The existing API-referenced comment distinguishes descriptor capture from retained payload and driver-owned OVERLAPPED lifetimes. |
| Q0087, 280 | The staging gather forms a readable slice only for nonempty initialized IoVectoredBuf regions, then copies into an independent allocation. Its preceding SAFETY comment is present. |
| Q0087 + Q0095, 287 | One WriteFile call into retained initialized staging bytes. Checked total length and stable ownership cover the call; staging is released only on immediate failure or acknowledged completion, otherwise transferred on cancellation. |

Linux descriptor-array lifetime is separately checked by a new mock cancellation
test: building the real SQE installs a boxed array, and dropping the pending op
transfers that same array and the same payload pointers to the owning driver.
It includes an empty segment without dereferencing it. No kernel request is made
in this test. Existing io_uring pipe coverage exercises actual vectored writes;
Windows staging retention has a cross-compiled test, not native execution.

### ReadvOp: all 12 reports in the 198-finding snapshot

| Reports/location | Local disposition |
| --- | --- |
| Q0087 + Q0095, 50 | Synchronous WSARecv with checked descriptor count/lengths and local outputs. The existing comment covers disjoint writable regions and no retained pointers. One FFI call accounts for the long block. |
| Q0087, 122 | Synchronous readv receives a live descriptor array converted from owned writable buffers, with checked native count. Its preceding safety comment is present. |
| Q0087, 213 | Windows file completion scatters a separately owned staging allocation into writable destinations. Source slicing is bounds checked; raw copy avoids making initialized slices over destination spare capacity. The existing comment explains non-overlap. |
| Q0087 + Q0095, 255 | One overlapped WSARecv submission. Winsock captures descriptors; buffer ownership and the driver OVERLAPPED outlive the request. The existing local contract cites the API documentation. |
| Q0087 + Q0095, 301 | One overlapped ReadFile submission into initialized staging storage. Checked total length precedes allocation; staging is retained on success/pending and transferred on cancellation. |
| Q0090, 115, 195, 228, 336 | Four ordinary Option::as_mut calls in polling, completion scatter, Windows submission and Linux entry building. These are safe reborrows, not unsafe pointer conversions. |

The Linux boxed descriptor array is stored before the SQE can be submitted and
transferred together with owned buffers on cancellation. The existing datagram
test now executes both synchronous polling and real io_uring completion: empty
segments, a short packet spanning segments, an empty packet, unchanged suffixes,
fixed segment lengths and stable backing addresses. It executed on Linux without
skipping io_uring. Windows compilation does not establish native scatter or
cancellation behavior; broader shutdown review remains open.

### ReadOp and RecvOp: 24 reports in the 198-finding snapshot

| Reports/location | Local disposition |
| --- | --- |
| RecvOp Q0087 + Q0095 at 48 | socket_recv uses one synchronous WSARecv call. Its local safety comment covers descriptor/output lifetimes and exclusive writable capacity. Keep the complete FFI argument list together. |
| RecvOp Q0087 + Q0095 at 227 | submit_windows uses one overlapped WSARecv call. The existing comment references descriptor capture and delayed flags behavior; payload/OVERLAPPED are retained until acknowledgement. Native overlapped MSG_PEEK remains separate open verification. |
| RecvOp paired Q0090 at 116, 192, 202, 260 | Eight reports on safe Option/CompletionBuffer::as_mut reborrows, respectively polling, completion decoding, Windows submission and Linux entry building. They are not unsafe raw-pointer mutable-reference conversions. |
| ReadOp Q0087 + Q0095 at 212 | One overlapped WSARecv call with the same documented captured-descriptor/retained-payload contract. The safety comment is present, not missing. |
| ReadOp Q0095 at 47 | One synchronous WSARecv call with local writable outputs and no retained pointers. Splitting its arguments into multiple unsafe blocks would not improve the contract. |
| ReadOp Q0095 at 241 | One ReadFile call with retained payload capacity and driver-owned OVERLAPPED storage. EOF conversion is handled by read_error_result; it exposes zero initialized bytes. |
| ReadOp paired Q0090 at 106, 188, 198, 273 | Eight safe Option/CompletionBuffer reborrow reports in the same four phases. No extra alias is created by these method calls. |

Both operations clear their completion token before exposing successful output;
negative results return before length updates (except ReadOp's explicit EOF
mapping). Drop transfers the stable buffer when the token remains outstanding.
A native Linux io_uring test now exercises both Read and Recv on a stream with
preinitialized reused storage: partial data replaces the old visible length and
EOF clears it. Existing ReadOp polling tests cover short reads, EOF and errors;
both operations have mock owning-driver cancellation/reclamation tests. Native
Windows cancellation, peek compatibility and full shutdown proofs remain open.

### RecvfromOp: all 17 reports in the 198-finding snapshot

| Reports/location | Local disposition |
| --- | --- |
| Q0087 + Q0095, line 59, socket_recvfrom | One synchronous WSARecvFrom call with live stack outputs and exclusive IoBufMut capacity. The existing SAFETY comment precedes it. The long block is one FFI argument list, not multiple unrelated unsafe operations. |
| Q0087 + Q0095, line 167, poll_poll | One synchronous recvfrom call; no MSG_TRUNC input flag. The initialized sockaddr output and writable buffer bounds are documented locally. Keep the single call together. |
| Q0087 + Q0095, line 334, submit_windows | One overlapped WSARecvFrom submission using boxed address/flags/length storage and a stable CompletionBuffer. Drop transfers both allocations to the owning driver. The local contract exists; native Windows cancellation/peek behavior remains separately unverified. |
| Two Q0090 each, lines 158, 258, 287 | Option::as_mut and CompletionBuffer::as_mut reborrow buffer storage in poll_poll and the Linux/Windows completion branches. These are safe methods, not raw-pointer as_mut casts. Completion branches update initialized length only after acknowledged success. |
| Two Q0090 each, lines 298, 370 | The same safe buffer reborrows in submit_windows/build_completion_entry. The separate completion-state box supplies stable descriptor/address fields; pointers into it are installed after boxing. |
| Q0090, line 498, reused_recvmsg_state test | Safe Option::as_mut on the test's completion box. No submission is made; the test changes output metadata then verifies rebuilding resets it without moving storage. |

New native Linux io_uring coverage exercises IPv4/IPv6 source decoding and
oversized datagrams with zero/eight-byte buffers. Both peek and consuming reads
report bounded initialized prefixes, and only the consuming call empties the
queue. Existing mock cancellation tests cover transfer to the owning driver and
rejected early reclamation. These are scoped evidence, not a claim to prove
every driver shutdown interleaving or third-party IoBuf implementation.

### io/buf.rs remaining five reports (198-finding snapshot)

| Rule/location | Disposition and evidence |
| --- | --- |
| Q0087 and Q0094, line 370, IoBufTemporaryPoll Send | The immediately preceding SAFETY comment exists and requires polling-thread confinement and a live backing borrow. These missing-comment reports are false positives; correctness still depends on each unsafe constructor caller honoring the contract. |
| Q0087, line 540, read_into_buf | The preceding SAFETY comment explains exclusive capacity and initialized-prefix bounds. Initializing spare bytes is necessary before exposing a safe mutable byte slice, which a Read implementation may inspect. |
| Q0089, line 541, read_into_buf pointer offset | initialized <= capacity follows from IoBuf's unsafe contract. The write length is capacity - initialized, so it stays in the allocation; the full-capacity case performs a zero-length write at the end. Existing prefix/spare/error tests now also execute without optional features. |
| Q0087, line 724, temporary_poll_buffer_tracks_initialized_prefix | The comment directly before the split let/unsafe expression describes the local storage lifetime and excludes asynchronous submission. The test initializes three bytes before exposing them. Retain the test and comment rather than suppressing the rule. |

Additional cursor coverage verifies suffix capacity/initialization, preservation
of the preceding prefix, rejected advancement (including usize::MAX) without
state mutation, full consumption, and empty-buffer behavior. No production
pointer arithmetic was replaced merely to silence these reports.

### ReapChild fallback mutex scope and worker-start failure

The two historical ReapChild::drop lock sites transferred ownership through
Arc<Mutex<Option<Child>>>. Their if-let temporary guards remained live during
child.wait(). Both branches now use wait_pending_child, whose guard ends before
waiting. Worker-start injection tests with a real child verify that rejecting
the worker leaves ownership for synchronous reaping; WNOWAIT checks do not reap
the child themselves. This passes on Linux with process alone and in the full
harness. The destructor can still block waiting when worker creation fails;
removal of its direct-lock diagnostics does not resolve that limitation.

### Signal drop-lock review and Windows initialization gap

Signal::drop and CtrlC::drop remove their waker slots under a mutex and destroy
the removed waker after unlocking. The Q0082 lock findings are not false
positives: concurrent dispatch scans and Unix handler restoration can contend.
No wait-free claim or blanket suppression is appropriate. Reviewing this path
also found Windows handler installation preceded publication of CTRL_C_STATE.
The state is now published first, with a separate retryable installation cell.
A Linux-executed test of the actual Windows initialization helper verifies
publication, failed-install retry and successful-install reuse. Native console
delivery and Unix fork/global-handler interactions remain open.

### IOCP detachment status and synchronous input lifetime

disassociate_iocp_handle now propagates NtSetInformationFile failures. Completion
registration removal happens only after successful detachment, allowing failed
rebind to preserve the existing registration rather than falsely proceeding.
A Windows-only invalid-target/retry/second-port-association test cross-checks;
native execution remains pending. Microsoft's IoIsOperationSynchronous and
NtSetInformationFile documentation establish the synchronous information-call
contract used by the local safety comment. The package-wide unsafe-comment
gate now covers Windows too. Drop-time detachment failure reporting/bookkeeping,
pending request teardown and native race coverage remain separate open issues.

### IOCP failed-submission reentrancy and cancellation pointer lifetime

The immediate submit_windows error branch formerly destroyed its Completion
(including an arbitrary waker) under a mutable state borrow. It now removes the
entry into a local and destroys it after releasing the borrow. A Windows-only
test checks a custom waker destructor can borrow the state; it cross-checks but
has not been run natively. Cancellation's unsafe-call comment now describes the
retained boxed pointer and the absence of a reentrant retired destructor in that
branch. The repeated-cancellation test verifies pointer stability and completed
retirement behavior. These changes do not establish all native cancellation or
shutdown races; disassociation's NT information-call lifetime remains open.

### Completion-port batch and regular-association contracts

process_batch now documents its synchronous output-buffer bounds and ownership
contract. register_handle_with_mode documents that successful association
returns an alias of the existing port, without transferring source ownership.
The modeled packet test's comment now attaches directly to its unsafe block.
A new native-Windows test covers empty queues and draining 257 interrupt packets
across bounded, possibly partial batches. Windows strict cross-Clippy passes,
but native execution remains pending. The explicit Windows-wide unsafe-comment
probe is down to two production sites: disassociation and cancellation; these
remain open rather than receiving unverified lifetime assertions.

### AFD creation and association comments

The NtCreateFile call now documents live counted-name/object-attribute inputs
and writable local outputs, with optional buffers null. The owned conversion
follows success/non-null checks. ensure_afd_handle keeps both file and port alive
while associating them, treats the returned port as an alias, and preserves IOCP
success packets when setting FILE_SKIP_SET_EVENT_ON_HANDLE. Local setup errors
drop the newly owned AFD handle before publishing it to the cache.
A Windows-only cached-handle/non-inheritance test cross-compiles. These four
comment dispositions do not prove native setup failure behavior or AFD request
cancellation; the latter remains under review.

### IOCP base-socket query and traversal

get_base_socket now documents its synchronous WSAIoctl outputs and validates
both returned byte count and INVALID_SOCKET before use. WSAGetLastError is an
integer-only thread-local query. The resolver previously rejected only a
self-loop; a malformed multi-node fallback cycle could run indefinitely.
The shared testable traversal tracks visited provider handles and rejects such
cycles without allocating on the direct-success path. Scripted tests execute
on Linux; native Winsock lookup tests are Windows-only and cross-compiled.
This is malformed-provider hardening, not a reproduced native provider defect
or completion-packet lifecycle proof.

### IOCP integer status conversion

Both RtlNtStatusToDosError calls now document their integer-only FFI contract.
Completion encoding no longer casts arbitrary ULONG error codes to i32 and
negates them unchecked or falls back to negating raw NTSTATUS. A shared checked
encoder preserves positive representable codes; otherwise IOCP returns the
explicit arithmetic-overflow error instead of a count or panic. Linux boundary
tests pass and common Windows status-mapping tests cross-compile. This is
defensive representation hardening: the documented unmapped-status fallback is
ERROR_MR_MID_NOT_FOUND, and no native abnormal-mapping reproduction is claimed.

### IOCP port ownership and timeout conversion

IocpInterruptor::interrupt now documents that its upgraded Arc keeps the port
live through PostQueuedCompletionStatus and the reserved wake packet has no
OVERLAPPED pointer. IocpDriver::new documents creation with INVALID_HANDLE_VALUE
and no existing port, followed by non-null validation and sole OwnedHandle
acquisition. These three local comments do not resolve the outstanding AFD and
completion-packet ownership review.

The adjacent duration conversion had a separate defect: saturating a finite
duration to u32::MAX selected Windows INFINITE. A shared Windows/test helper now
caps finite values at u32::MAX-1. The Linux-executed boundary regression failed
before and passes after the change. This proves arithmetic/sentinel handling,
not native Windows wait behavior; sub-millisecond rounding remains unchanged.

### Unix-stream and fixture unsafe sites

PollUnixStream's two write constructors now state the initialized borrow and
poll-only operation lifetime, matching the TCP/pipe adapter review. The module
enables undocumented_unsafe_blocks. AsyncWrap's CountingReader and ChunkedWriter
fixtures now use the existing checked buffer helpers instead of duplicating
unsafe pointer copying/slice construction; helper cfgs include tests independent
of optional features. statx_timestamp fixture zeroing documents its integer-only
layout and reserved-field initialization.

Whole-harness Linux/macOS unsafe-comment checks now pass and the package enables
the lint on Unix. The Windows probe still reports 16 IOCP library/test sites;
those are not considered reviewed merely because other modules have passed.
Comment coverage is separate from proof of every pointer lifetime and native
driver teardown path.

### PollUdpSocket temporary buffers

The six constructor sites in poll_recv, poll_recv_from, poll_send, poll_send_to,
poll_peek and poll_peek_from now have individual safety comments. Receive/peek
regions are exclusively borrowed writable slices; send regions are initialized
read-only slices. Each operation is constructed locally and uses poll_op_poll,
which rejects completion submission, so a Pending return retains no temporary
buffer operation. The UDP module now enables undocumented_unsafe_blocks.
Direct borrowed-method tests supplement the separately named owned-buffer async
test: Pending preservation, buffer reuse, source addresses, peek non-consumption,
empty datagrams and fresh connected sends/receives all pass on Linux. This does
not verify Windows overlapped peek semantics; these methods use poll dispatch.

### PollTcpStream temporary buffers

The missing-comment sites in peek, poll_write and poll_write_vectored now state
the backing borrow, read/write permissions and poll-only dispatch restrictions.
Peek creates its temporary wrapper inside each poll rather than retaining the
operation across Pending, aligning implementation with IoBufTemporaryPoll's
documented scope. The caller's mutable slice borrow still spans the async peek;
this is contract tightening, not a reproduced dangling-pointer defect.
The native Linux cancellation/reuse/peek-then-read regression passes; Windows
and macOS execution remains unverified. The TCP stream module now enables
undocumented_unsafe_blocks, including its existing ReadBuf initialization sites.

### PollPipe temporary write buffers

`io/pipe.rs::PollPipe::poll_write` and `poll_write_vectored` now have direct
constructor safety comments: source bytes are initialized and borrowed for the
whole call, operations only read them, and poll_op_poll prevents completion
submission. Pending retains readiness/waker state, not the local temporary
operation. Vectored metadata is owned but the pointed-to bytes remain borrowed;
neither can escape this synchronous poll. The module now enables the safety-
comment lint. The Linux full-pipe regression mutates and drops the original
buffer after Pending, verifies queued bytes and successfully writes fresh
vectors after draining. This closes the two missing-comment dispositions but
does not stand in for the broader borrowed-buffer audit in TCP/UDP/Unix streams.

### Array reads and local driver/statx comment gates

`fs::read` used from_raw_parts on the returned `[u8; 8192]` solely to copy a
bounded initialized prefix into its output Vec. Ordinary slicing now performs
that copy with the same min(buffer length, returned count) bound, removing the
unsafe operation rather than adding a justification for it.

io_uring's eventfd write comment now directly precedes the unsafe call. eventfd
creation documents its integer-only arguments and checked OwnedFd acquisition;
push_entry documents SQE copying and the distinct lifetime of referenced
driver/operation allocations. StatxOp's assume_init comment now immediately
follows extraction of the owned result box and explains the successful-CQE
precondition. Both modules now enable undocumented_unsafe_blocks locally.
These dispositions do not prove the entire shutdown/cancellation graph safe.

### Repeated and unknown cancellation payloads

Inspection of io_uring `DriverState::ignore_completion` found that its early
missing-token return destroyed `data` while the caller held the state RefMut.
It now returns that payload in a retired Completion for out-of-borrow disposal.
The live-driver destructor-borrow regression failed before and passes afterward.
Both this function and IOCP `DriverState::retain_cancelled` also overwrote an
existing ignored_data owner on repeated calls. They now retain both boxed owners
through completion retirement. A shared flat retention list replaces the initial
nested-pair implementation, avoiding a deep recursive destructor chain. A
100,000-payload test verifies stable allocation and complete, deferred disposal.
A Linux state regression proves the previous
early drop and fixed retention; the corresponding IOCP regression is compiled
only. These cases do not establish a duplicate-cancellation trace in normal use,
nor do they close the outstanding native IOCP acknowledgement/teardown audit.

### Positioned-read initialization audit

`op/readat.rs::submit_windows` (Q0095 at line 133 in the 215-finding snapshot)
contains a single ReadFile call. Its writable length comes from checked capacity,
not initialized length; CompletionBuffer retains exclusive stable storage and
Drop transfers pending storage to the owning driver. OVERLAPPED offset-word
assignment is a separate bounded unsafe block. Retain these calls and their
local contracts; native IOCP lifetime validation is still outstanding.

In `poll_completion`, errors return before `set_buf_init`; the Windows EOF code
is explicitly normalized to a successful zero-byte result. For successful reads,
the kernel's initialized byte count determines the returned buffer length. This
relies on the driver's completion belonging to the submitted operation, not on
an independently checked length bound at this layer.

The existing sparse positioned-write fixture now also runs real ReadAtOp calls
at all three offsets (including above 4 GiB): spare-capacity Vec initialization,
EOF resetting a reused Vec to empty, zero-capacity reads and unchanged shared
cursor are verified. Invalid offsets preserve the original buffer. Reopening the
unlinked scratch file write-only then reading it produces a real EBADF completion
and preserves buffer contents. These checks executed on Linux; they do not test
Windows EOF normalization or native cancellation.

### Positioned-write buffer and offset audit

`op/writeat.rs::submit_windows` (Q0095 at line 131 in the 215-finding snapshot)
contains one WriteFile call, not a broad unsafe algorithm. It submits only the
initialized IoBuf prefix, with `completion_len` rejecting lengths above i32::MAX
before submission. The two-word OVERLAPPED offset is assigned in a separate,
locally documented block; u64::MAX is rejected as the Windows append sentinel.
The operation retains stable boxed buffer storage until completion, or transfers
that allocation to the owning driver's cancellation retention on Drop. Retain
the bounded unsafe calls; native IOCP acknowledgement/teardown remains open.

Linux `build_completion_entry` uses the same checked length and `positional_offset`
to reject unsigned values outside the signed file-offset range. A new native
io_uring test verifies writes at 0, 4097 and 2^32+3, unchanged shared cursor,
returned original buffer, file length and data read back through standard
positional I/O. It accepts valid short writes, and rejects i64::MAX+1/u64::MAX
before submission. The scratch file is sparse and unlinked immediately after
exclusive creation, so unwinding closes/reclaims it. The test executed, without
an unavailable-io_uring skip, on Linux. Existing mock cancellation tests verify
buffer retention and rejection of premature reclamation, not native Windows
cancellation. No production positioned-write defect or speedup is claimed.

### ConnectOp large-unsafe-block findings

The five Q0095 locations below refer to the 215-finding snapshot. Each flagged
unsafe block contains one FFI invocation; its argument formatting is not a reason
to extract an additional unsafe wrapper or suppress the rule. Retain the bounded
calls and existing local safety comments. This resolves the large-block claim,
not every platform-lifecycle question associated with the operation.

| Location in op/connect.rs | Scope and ownership evidence |
| --- | --- |
| 137, load_connect_ex | One synchronous WSAIoctl call; the GUID, optional function-pointer output and returned-byte count are local storage with exact supplied sizes. OVERLAPPED and completion callback are null. Failure and absent extension pointers are checked before use. Native Winsock provider behavior remains unverified here. |
| 170, set_connect_context | One setsockopt call; SO_UPDATE_CONNECT_CONTEXT supplies no payload (null pointer, zero length). The caller retains its socket handle through the call; no Rust buffer escapes. |
| 346, Unix poll_poll | One getsockopt(SO_ERROR) call with a live c_int output and its exact capacity. The following getpeername probe uses separate sockaddr_storage and checks only status, not uninitialized address fields. Kernel-reported connection errors are handled before declaring success. |
| 432, Windows poll_poll | Equivalent SO_ERROR query using an initialized i32 output and i32 capacity. Socket-only handle validation precedes the call. The subsequent peer probe uses initialized SOCKADDR_STORAGE and checks status only. Native execution remains outstanding. |
| 583, submit_windows | One ConnectEx invocation. The boxed, length-validated address stays at the same allocation through moves; Drop transfers it into cancellation retention. No initial send buffer is supplied. Driver-owned OVERLAPPED retention remains part of the broader IOCP audit, not proven by the local comment or mock test. |

Validation: all five connect tests pass on Linux. The live connect test now runs
IPv4 and IPv6 with TcpStream and PollTcpStream under both Mio and io_uring (eight
connections), with no io_uring-unavailable skip on this host. Existing tests cover
address bounds/families, stable allocations across moves and cancellation routed
to the original driver. The IPv4 test fixture now uses the existing address
constructor instead of an unsafe zeroed native struct. Windows/macOS validation
is cross-compilation only. Follow-up review fixed exceptional ConnectEx binding:
WSAEADDRINUSE is now propagated, while WSAEINVAL preserves the documented
already-bound case. The policy and native binding tests are Windows-only and
cross-compiled, not executed; the broader IOCP lifecycle audit remains open.

### Accept result ownership

`op/accept.rs` finishing helpers and the Windows AcceptEx success branch now
return their existing `OwnedFd`/`OwnedSocket` directly. `op/accept_unix.rs` does
the same in its poll and completion branches. This removes the owner-to-raw-to-
owner round trip, including unsafe reconstruction in both listener callers and
the TCP ownership test. Error-path RAII and cancellation storage are unchanged.
Discard tests reproduced both poll-path leaks before the fix; an additional
native Linux io_uring test verifies closure of discarded TCP and Unix completion
results. Windows/macOS paths compile under strict Clippy but are not native-tested
here. This does not resolve the outstanding IOCP teardown audit.

### Open completion ownership

`op/open.rs::OpenOp::poll_completion` now turns a nonnegative OpenAt result into
`OwnedFd` at the operation boundary. The driver removes a retrieved completion
and the operation clears its cancellation token before handing ownership out;
pending cancellation continues to retain the path through `ignore_completion`.
The remaining `from_raw_fd` has a local safety contract and the module enables
`undocumented_unsafe_blocks`. `fs/open_options.rs::open` uses the safe owned-fd
conversion instead of independently acquiring raw ownership. The native Linux
pipe-EOF regression failed for discarded raw results and passes for owned ones.
This disposition does not prove every driver teardown/cancellation path safe.

### Network backend and registration documentation

Removed stale module/type notes claiming networking falls back to blocking-pool
or synchronous standard-library I/O and that every method panics without a
runtime. Poll mode instead uses nonblocking calls plus readiness; registration
without a runtime returns an error, while direct address/option queries use the
owned socket. Async I/O should still be driven inside a runtime. The module notes
now explicitly distinguish synchronous bind/address resolution from data I/O.
Poll stream descriptions refer to the owning readiness driver, not exclusively Mio.

Added an executed no-runtime registration regression for standard UDP sockets,
TCP listeners and Unix stream pairs, checking NotConnected rather than panic.
All 245 Linux harness tests and 16 documentation checks pass; Windows/macOS
all-feature Clippy compile the applicable tests. Strict rustdoc and formatting
pass. The test does not prove all methods work outside a runtime or validate
native Windows/macOS execution.

### Process examples and final ignored-example removal

Replaced the final five ignored process examples with one executable Unix/sh
example covering child stdin, stdout, stderr, wait, command status and captured
output. The old stdin snippet called write_all on an owned-buffer stream without
an adapter; the replacement uses AsyncWrap, flushes and closes stdin, then drains
both output pipes concurrently with child.wait. It checks bytes and exit status,
configures its pool independently of blocking-default, and bounds async work
with a timeout without claiming timeout kills the process.

Process-only and all-feature doctests pass: 16 checks, zero ignored examples.
One of those checks remains the intentionally compile-only stdin echo example;
feature/platform-gated blocks execute only when applicable. The process example
executes on Linux and is explicitly Unix-gated, not evidence of Windows execution.
Strict rustdoc, formatting and whitespace checks pass. Removing ignored examples
does not close the remaining unsafe/lifecycle review or native platform gates.

### Executable splice example

Replaced the ignored splice example's dependency on data.txt and the separately
gated vibeio pipe helper with a self-contained standard-pipe-to-Unix-socket
transfer. It executes with splice alone, closes the producer to establish EOF,
checks the short transferred count against a larger requested limit, and verifies
all bytes at the peer. I/O is timeout-bounded; the tiny payload avoids requiring
a concurrent consumer. The example uses PollUnixStream's direct Tokio traits,
not AsyncWrap, which adapts the separate owned-buffer traits.

Clarified the module summary: splice_exact stops at EOF and can return fewer
than len bytes. Splice-only and all-feature doctests pass (15 checks each), with
zero and five ignored examples respectively. Strict rustdoc and formatting pass.
This exercises the Linux readiness path, not io_uring completion or throughput.

### Unix socket examples

Replaced three ignored Unix-socket examples using a fixed /tmp/mysocket path
and an incorrectly awaited bind with a finite executable example. It creates a
unique short directory, checks the bound pathname, polls connect and accept
together, and verifies a flushed message through the Tokio adapter. A five-second
timeout bounds the exchange. An ownership guard removes only the created socket
path and directory; the docs explain that dropping the listener alone does not
unlink a filesystem socket.

No-feature and all-feature doctests pass (14 checks each). There are no ignored
examples in the default-feature build and six under all features. Strict rustdoc,
formatting and whitespace checks pass. The Unix example executes with Mio on
Linux; macOS execution remains outstanding and Windows gates this block out.

### TCP and Tokio-adapter examples

Replaced four ignored TCP/AsyncWrap examples with a finite loopback exchange.
The listener snippets incorrectly awaited synchronous bind; the adapter snippet
called an undefined placeholder reader. The executable replacement binds an
ephemeral port, polls connect and accept together, checks the peer address, and
uses Tokio read_exact/write_all over AsyncWrap without requiring a Tokio runtime.
Explicit flushes deliver both messages; dropping the client allows read_to_end
to observe EOF. A five-second timeout bounds stalled I/O. The example documents
the adapter's lack of half-close and limits itself to sequential request/response,
not a claimed full-duplex fix.

No-feature and all-feature doctests pass (13 checks each), with 3 and 9 ignored
examples respectively. Strict rustdoc, formatting and whitespace checks pass.
This is Linux execution; native Windows/macOS validation remains outstanding.

### UDP examples and runtime requirements

Replaced three ignored UDP examples with one executable, timeout-bounded loopback
exchange on ephemeral ports. The old adaptive-socket snippets incorrectly awaited
synchronous bind, lacked a mutable binding for connect, and applied ? to send's
(result, buffer) tuple. The replacement checks the send count and receive sender,
then converts to poll mode and verifies a reply plus preservation of local address.
No external server or fixed service port is needed.

Corrected UdpSocket's implementation notes: poll mode is nonblocking readiness
I/O, not a blocking std::net fallback; registration outside a runtime returns an
error rather than a blanket panic on every method. No-feature and all-feature
doctests pass (12 checks each), leaving 7 and 13 ignored examples respectively.
Strict rustdoc and formatting pass. Executed on Linux; native Windows/macOS
network behavior remains unverified by this example.

### Signal example cancellation semantics

Replaced two ignored signal examples with references to the executable signal
wait example. The old module example gated only the Unix listener declaration,
leaving its recv call outside that platform gate. The replacement gates the
whole Unix block and tests SIGTERM registration plus immediate receive timeout
without sending process signals. It explicitly drops the retained listener:
canceling its borrowed recv future does not itself unregister the listener.
The existing Ctrl-C cancellation example remains covered in the same test.

Isolated signal and all-feature doctests pass (11 checks each), with 10 and 16
ignored examples respectively. Strict rustdoc, formatting and whitespace checks
pass. These Linux executions verify registration/cancellation, not native
Windows console delivery or external-handler/fork races.

### Runtime and blocking-task examples

Replaced six ignored builder/executor examples with references to two executable
harness examples. The first constructs a runtime, enters it with block_on,
cancels an unpolled task and joins another task's output. The canceled task would
panic if polled, so successful execution checks cancellation instead of only
compilation. The second explicitly configures the optional default blocking pool
and checks spawn_blocking's fallible result. Documentation distinguishes dropping
a join handle from explicit cancellation and notes that running blocking work
cannot be stopped by dropping its future.

No-feature and all-feature doctests pass: 11 checks each, with 10 and 18 ignored
examples respectively. The pool example executes under all features and is gated
out without blocking-default. Strict rustdoc, formatting and whitespace checks
pass. These are Linux executions, not new native Windows/macOS evidence.

### File method examples now exercised

The nine remaining ignored File method examples now reference the executable
filesystem example. Added explicit coverage for File::create, read_exact_at,
write_at, sync_data, sync_all and handle metadata; File::open, read_at and
write_exact_at were already exercised there. The write_at example handles a
short successful count by writing only the remaining suffix at the advanced
offset. Final readback verifies contents, not just the resulting file size.
Created files live only in the owned scratch directory and close before cleanup.

Isolated fs and all-feature doctests pass (9 checks each), leaving 16 and 24
ignored examples respectively. Strict rustdoc, formatting and whitespace checks
pass. These executed Linux examples exercise offload, not io_uring or native
Windows/macOS behavior; they do not simulate crashes to prove disk durability.

### Metadata example semantics

The FileType::is_symlink example incorrectly called metadata, which follows the
link and reports its target. Replaced it with explicit symlink_metadata guidance
and executable Unix coverage comparing both queries on the same link. Consolidated
the other four ignored metadata/file-type examples into the same scratch-directory
harness example, adding size, regular-file and directory assertions. The link
is relative to the scratch directory and has explicit cleanup. Its Unix gating
avoids requiring Windows symlink privileges, rather than silently skipping errors.

Metadata's backend documentation now distinguishes completion-backed queries
from synchronous/offloaded standard-library fallbacks. Isolated fs and all-feature
doctests pass (9 checks each); ignored counts are 25 and 33 respectively. Strict
rustdoc and formatting pass. Symlink execution is verified on Linux only.

### File and OpenOptions example consolidation

Removed a duplicated public File example incorrectly attached to the private
FileIo enum, replacing it with an accurate description of that enum's role.
File, OpenOptions and OpenOptions::open now reference the executable filesystem
example instead of ignored snippets operating on fixed working-directory paths.
Expanded the example to read five bytes at offset seven into owned Vec capacity,
check the returned count/prefix, then truncate only its scratch file and perform
an exact positional write with returned-buffer verification. Closing the handles
before cleanup also makes the example suitable for Windows file-sharing rules.

Isolated-fs and all-feature documentation checks pass (9 each), with ignored
examples reduced to 30 and 38 respectively. Strict rustdoc, formatting and
whitespace checks pass. Native Windows/macOS execution remains unverified.

### Executable filesystem module example

Replaced the ignored filesystem module example, which wrote fixed paths in the
current directory, with a pointer to a runnable harness example. The replacement
configures an explicit demonstration pool, exercises write/read/create_dir and
metadata, and cleans only known paths beneath its successfully created unique
scratch directory. It runs with fs alone as well as all features, without relying
on blocking-default. The example identifies its thread-per-operation pool as a
demonstration, not a production pool recommendation.

Corrected the module's blanket claim that calls outside a runtime panic; current
open/file/path implementations have synchronous fallbacks. Isolated fs and
all-feature doctests pass: 9 documentation checks, with 34 and 42 ignored examples
respectively. Strict all-feature rustdoc passes. The filesystem example executes
on Linux; native Windows/macOS example execution is still outstanding.

### Process Option reborrows and blocking-operation documentation

Reviewed seven process/mod.rs Q0090 sites from the 220-finding snapshot:

| Snapshot location | Disposition |
| --- | --- |
| 326, ChildStdin::write | Option<ChildStdin>::as_mut is borrowed through &mut self only in the synchronous fallback. Offloaded writes instead take owned storage and restore it after awaiting. |
| 351, ChildStdin::flush | Safe Option reborrow for synchronous flush; no raw cast. The worker path separately owns the stream. |
| 380, ChildStdout::read | Safe Option reborrow for synchronous read_into_buf; exclusive buffer initialization is delegated to that checked helper. |
| 414, ChildStderr::read | Same safe synchronous stream reborrow; no alias is manufactured by as_mut. |
| 606, Child::inner_mut | Converts an exclusive Option<Child> borrow to Result<&mut Child>, reporting consumed state. |
| 682, Command::inner_mut | Converts an exclusive Option<Command> borrow to &mut Command, with an intentional consumed-state panic. |
| 773, Command::spawn | Safe exclusive Option reborrow for std's synchronous spawn; consumed state returns an error. |

Corrected module documentation that claimed nonblocking process interaction and
a runtime requirement even for the synchronous outside-runtime fallback. Added
explicit status/output and module-level cancellation notes: once an offload owns
the object, dropping its pending future does not stop the worker or restore the
wrapper. Infallible consumed-command accessors can panic. This documents an open
reusability limitation rather than claiming cancellation recovery is fixed.

### Reaper raw ownership review

No ManuallyDrop or ptr::read ownership conversions remain under src/vibeio.
The Linux pidfd capability probe now wraps its successful descriptor in OwnedFd
instead of manually closing it, and uses std::process::id rather than an unsafe
getpid call. Its existing ENOSYS-only capability decision is unchanged.
Enabled module-local undocumented-unsafe enforcement in process/reaper.rs.

Retained Windows wait_callback's Arc::from_raw: the registrar transfers exactly
one strong reference to the one-shot callback and holds its own reference until
the wait handle is published. Failed registration recovers the transferred
reference; it cannot invoke the callback. The Q0095 registration block is one
FFI call with independent output storage, not a sequence of unchecked mutations.
Existing early-callback tests compile but still need native Windows execution.
This review does not prove all native wait teardown behavior or eliminate the
thread-creation-failure fallback's blocking wait.

Validation: 173 isolated process-feature Linux tests and 8 documentation checks
pass, along with root and Windows-target all-feature Clippy and formatting.

### Splice staging-pipe ownership

Removed WriteOwnedFd's ManuallyDrop wrapper and custom unsafe destructor.
Its InnerRawHandle field now precedes OwnedFd so ordinary field destruction
deregisters before closing the pipe writer. Constructor-local ownership also
continues to close the writer if registration or flag setup fails.

Added an executed Linux regression for successful construction/drop and injected
registration failure. The nonblocking reader observes EOF on both paths (a
descriptor leak would instead fail with WouldBlock); the mock ledger records
exactly one deregistration on success and none for rejected registration.
The test does not inject a subsequent fcntl failure. All 244 all-feature harness
tests, 173 isolated splice-feature tests, 8 documentation checks per configuration,
strict root Clippy and formatting pass.

### Exact splice interruption recovery

splice_exact and sendfile_exact previously propagated Interrupted immediately.
For sendfile_exact, an interrupted drain could discard a nonempty staging pipe
even though its source position had already advanced. Both helpers now use a
shared retry wrapper; transfer totals and pending staged bytes are unchanged
until a successful syscall count arrives. EOF, WriteZero and non-interrupted
errors retain their existing behavior. Public docs describe the retry policy.

Added a deterministic transfer_batches regression: interrupted fill, five-byte
fill, two-byte drain, interrupted drain, remaining three-byte drain, then EOF.
It checks every requested count, exact attempt counts and five transferred bytes.
The original code failed with Interrupted; the fixed code passes. This does not
claim rollback of staged bytes on cancellation or other terminal errors.

Validation: 243 all-feature Linux harness tests, 172 isolated splice-feature
tests, 8 documentation checks per configuration, strict root Clippy and
formatting pass. Production default-feature Python behavior is unchanged.

### Splice completion count and syscall boundary

SpliceOp's per-operation cap still used u32::MAX despite the driver's signed
i32 completion-result format. Aligned it with i32::MAX, preserving the existing
short-transfer policy rather than rejecting larger requests. The boundary test
now includes i32::MAX and i32::MAX+1; it failed on the former implementation's
cap and passes after the change. No multi-gigabyte allocation or actual kernel
count overflow was reproduced. Public splice docs now state the cap and point
to splice_exact for repeated transfers.

Reviewed op/splice.rs:108 (Q0095 in the 220-finding snapshot): the long unsafe
block is one syscall with null offset pointers, no userspace data buffers, and
SPLICE_F_NONBLOCK. Added its local contract and enabled module-local unsafe
comment enforcement. Existing duplicate-fd and poll contracts remain in place;
NONBLOCK does not promise that regular-file storage access cannot block.

All 242 all-feature Linux harness tests, 171 isolated splice-feature tests,
8 documentation checks in each configuration, strict root Clippy, formatting
and whitespace checks pass. Splice is Linux-only and optional; this follow-up
does not alter the default-feature Python path last measured.

### Builder platform-query boundaries

Reviewed builder.rs:23 (Q0095 in the 220-finding snapshot). The nine-line unsafe
block contains a single sysctlbyname call, not nine lines of unsafe logic. Its
NUL-terminated constant name, 64-byte initialized output buffer and length
out-parameter remain live through the call; null newp requests no modification.
The safe parser bounds-checks the returned length before reading it. Existing
tests cover out-of-range sizes, missing/interior NULs, invalid UTF-8 and version
thresholds. Retained the readable call formatting; no lint suppression added.

The Windows query duplicated OSVERSIONINFOW and RtlGetVersion locally. Replaced
both with windows-sys 0.61.2's generated structure and function, inspecting the
installed binding's signature and field layout first. Enabled its SystemServices
and SystemInformation feature gates in root and isolated harness manifests.
Structure size initialization, NTSTATUS error handling and minimum-version policy
are unchanged. Added a Windows-only test that invokes the native query on the
supported test host, in addition to the existing portable threshold tests.

Windows all-target/all-feature Clippy passes, and all 242 Linux harness tests
and 8 documentation checks pass. Native Windows execution and linking remain
outstanding (no MinGW linker found locally); no successful native query is
claimed from cross-compilation alone.

### Apple datagram wake retries

Both native kqueue and the Apple Mio fallback recursively called wake after
Interrupted. Replaced that duplicated recursion with one iterative send helper,
so repeated interruptions do not depend on compiler tail-call optimization to
bound stack use. Successful sends still finish immediately, WouldBlock still
means a wake is already queued, and other errors are returned unchanged.

The shared helper is compiled for Apple production builds and all test targets.
A Linux-executed deterministic test injects 100,000 interruptions before each
of success, WouldBlock and BrokenPipe, checking attempt counts and preservation
of the terminal error. No signal delivery or native macOS wake reliability is
inferred from that simulation. All 242 Linux harness tests and 8 documentation
checks pass; root and macOS-target all-feature Clippy pass. This does not change
the production Linux wake path tested by the latest Python/matrix run.

### Executor reborrow audit and self-cancellation fix

Reviewed the four executor Q0090 locations in the 220-finding snapshot:

| Snapshot location | Disposition |
| --- | --- |
| executor.rs:724 | Root future is pinned with pin!; Pin::as_mut safely reborrows it for each poll. |
| executor.rs:803 | Spawned future is Pin<Box<dyn Future>> taken from its RefCell slot. Moving the box does not move the pinned allocation; the slot borrow is dropped before polling. |
| executor.rs:852 | &mut *inner.token_to_task.borrow_mut() is a safe RefMut dereference used by mem::take. The slab is detached before user futures are dropped. |
| executor.rs:1342 | The remote-wake test uses pin! and Pin::as_mut for sequential receiver polls. No unchecked projection or pointer cast occurs. |

The Q0090 claims themselves are false positives, but inspection found a separate
lifecycle defect: a task canceling itself during poll could return Pending and
be restored to its task slot until a later scheduler tick. SpawnFuture now
rechecks cancellation after a pending inner poll and reports completion so the
executor drops its storage in the same batch. Documented the distinction between
immediate cancellation of a suspended task and cancellation during its own poll.

Added a regression covering self-cancellation followed by Pending and Ready.
It checks one poll, one destructor call and immediate slab reclamation after
poll_once. The Pending case failed before the fix (zero destructor calls), then
passed. All 241 Linux harness tests, 8 documentation checks and strict root
Clippy pass. This changes the task-poll path; Python integration and performance
measurements have not yet been refreshed for this follow-up.

### Q0090 safe reborrows reviewed in the 220-finding snapshot

These nine diagnostics misidentify safe reference reborrows as unsafe mutable
casts. Inspection included the containing fields and methods, not just the
flagged expression. Locations identify this snapshot and may move with edits.

| Location under src/vibeio | Expression/type and disposition |
| --- | --- |
| time/timeout.rs:75 | future_pin.as_mut() reborrows Pin<&mut Option<F>> obtained through pin_project_lite. as_pin_mut projects the optional future; no raw-pointer cast or unchecked projection is present here. |
| time/timeout.rs:90 | this.sleep.as_mut() borrows Option<Sleep> through the distinct unpinned projected field; Pin::new checks its Unpin requirement. This does not alias the projected future. |
| util/async_wrap.rs:92 | write_fut.as_mut() borrows Option<Pin<Box<dyn Future>>> through &mut self. Polling retains the boxed future's pin; the slot is cleared only after the ready result is extracted. |
| util/async_wrap.rs:105 | flush_fut.as_mut() has the same safe Option/Pin<Box> reborrow and ready-before-clear structure for the flush slot. |
| util/async_wrap.rs:172 | read_fut.as_mut() borrows the owned pinned future slot; after completion the returned buffer and inner stream are recovered before use. No mutable pointer cast occurs at this expression. |
| op/io_util.rs:496 | Test borrows CompletionBuffer<[u8; 32]> mutably to obtain an address for equality checks. The pointer is not dereferenced after moving the wrapper; the test checks boxed storage stability. |
| op/io_util.rs:542 | CompletionBuffer::as_mut matches &mut self; its Box branch uses Box::as_mut to return the exclusive borrow. No unsafe block or cast exists in the method. |
| process/reaper.rs:31 | ReapChild::deref_mut uses Option<Child>::as_mut through &mut self and returns that exclusive reference. No raw pointer or unsafe operation occurs. |
| process/reaper.rs:249 | Test uses Pin<Box<future>>::as_mut to poll a pinned wait future. The result is consumed before the next poll; no unchecked pin operation is present. |

These close only the stated Q0090 claims at these nine sites. They do not close
AsyncWrap's full-duplex limitation, the reaper's thread-creation-failure fallback,
or the separate cancellation/FFI findings elsewhere.

Refreshed integrated validation: run_rust_tests.py --all-features passes all
357 root tests, including the embedded vibeio and rsloop transport suites.
Log: target/vibeio-cleanup-root-current.log. This Linux run does not execute
Windows/macOS-only regressions.

### Kqueue descriptor ownership and FFI boundaries

KqueueDriver now owns its queue through OwnedFd rather than a raw descriptor
and custom Drop. Ownership is acquired immediately after successful creation,
so failed wake-socket setup and failed wake-filter installation both release
the queue automatically. The queue remains the first field, preserving closure
before registration/user-waker destruction. Kernel calls borrow its raw handle.

Documented all five remaining unsafe sites in this module: queue creation,
ownership acquisition, changelist submission, event retrieval and initialized
event-prefix access. Enabled module-local undocumented-unsafe Clippy enforcement.
macOS all-target/all-feature Clippy passes; 240 Linux harness tests and 8 docs
pass but do not execute the macOS implementation. Native shutdown, registration
rollback and deletion-failure regressions still require macOS execution.

### Kqueue deregistration error handling

Deregistration removed the slab entry, then used early-return propagation for
each filter deletion. Failure deleting the read filter therefore skipped the
write filter entirely. It now attempts every installed filter, returning the
first error and retiring wakers outside the driver-state borrow as before.

Added fault-injection coverage for read-only failure, write-only failure and
both failures. The test verifies both deletions are attempted, the first error
is retained, the slab token is retired, successful deletion removes real kernel
state, and failed filters remain available for explicit test cleanup. It also
checks the state is not borrowed during deletion. macOS all-target/all-feature
Clippy compiles the test; native execution remains outstanding. Linux harness
tests (240), documentation checks (8), formatting and whitespace checks pass.
This prevents skipping independent cleanup; it does not guarantee removal of a
filter whose kernel deletion actually fails.

### Kqueue initial-registration rollback

Initial registration previously submitted both filters in one changelist and
removed only the slab token on failure. A partially applied changelist could
leave a kernel filter behind. Registration now attempts deletion of every
requested filter before discarding the token, continuing cleanup after a
deletion failure. Successful cleanup preserves the original registration error;
failed cleanup reports both errors. The normal successful path still uses one
batch submission.

[Apple's kevent manual](https://developer.apple.com/library/archive/documentation/System/Conceptual/ManPages_iPhoneOS/man2/kevent.2.html)
describes per-element changelist errors rather than transaction rollback.
The new regression applies zero, one or both real kernel filters before injecting
an error, checks subsequent raw deletions return ENOENT, and verifies the open
descriptor can be registered again. macOS-target all-target/all-feature Clippy
compiles this regression; native execution remains outstanding. Exceptional
rollback failure can still leave filters until descriptor/queue closure and is
now surfaced, not claimed solved. Existing interest-change retry behavior is
unchanged; this fix addresses initial registration only.

### ReadvOp polling initialization contract

ReadvOp already uses owned descriptors without constructing a temporary byte
slice. IoVectoredBufMut intentionally has no initialization-length setter;
documented that callers with custom spare-capacity storage must use the returned
byte count, and that individual buffer lengths are not automatically changed.

Added a connected-UDP regression for deterministic short reads across leading,
interior and trailing empty segments. It verifies the returned count, prefix
contents, unchanged suffix, descriptor lengths and allocation addresses, then
checks that an empty datagram leaves all buffers unchanged. It passes on Linux
and compiles under Windows/macOS all-target/all-feature Clippy. The full Linux
harness now passes 240 tests and 8 documentation checks. This is polling-path
coverage, not native Windows/macOS execution or validation of Windows file
staging and asynchronous completion.

### RecvfromOp polling buffer boundary

The Windows synchronous WSARecvFrom helper now borrows IoBufMut directly,
removing its temporary raw-parts slice. Capacity is checked before extracting
the writable pointer; address validation and successful-prefix initialization
are unchanged. MaybeUninit is now imported only for the Unix address storage.
SendtoOp already borrows IoBuf directly and needed no equivalent change.

Added a cross-platform UDP loopback regression exercising a nonempty packet,
an empty datagram, and another nonempty packet using the same owned buffer.
Each packet is peeked and then consumed, checking both payload and sender address;
the final nonblocking receive confirms that no packet was left queued. A read
timeout bounds failures. This covers polling, not overlapped MSG_PEEK support.

Validation: 239 Linux harness tests and 8 documentation checks pass. Windows
and macOS all-target/all-feature Clippy compile the test; native execution on
those platforms remains outstanding.

### ReadOp buffer boundary

Follow-up after commit 39dde2b: the Windows synchronous socket helper borrows
IoBufMut directly, validates capacity before pointer extraction, and no longer
constructs a temporary MaybeUninit slice with from_raw_parts_mut. The existing
successful-read initialization and completion/cancellation retention are unchanged.
Added a Windows loopback regression using an empty Vec with spare capacity:
one received byte becomes initialized, then EOF clears its initialized length.
The socket has a five-second read timeout to bound a broken regression.

Validation: 238 Linux harness tests and 8 documentation checks pass; strict
Windows all-target/all-feature Clippy compiles the new regression. Native Windows
execution remains outstanding; this is not evidence of an overlapped-read test
or a performance improvement.

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
