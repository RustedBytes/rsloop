# Transport configuration

These optional variables are read by the native transport implementation.
Set them before importing rsloop. Values are cached on first use; changing the
environment later is not a supported way to retune an active process.

| Variable | Type and default | Effect |
| --- | --- | --- |
| `RSLOOP_TRANSPORT_STATS` | Boolean; disabled | Enables transport counters. Trimmed, case-sensitive values `1`, `true`, `yes`, `on` enable it; all other values disable it. |
| `RSLOOP_MAX_WRITE_BUFFER_BYTES` | Positive integer bytes; `67108864` (64 MiB) | Caps buffered writes. Missing, invalid, zero, or out-of-range values use the default. |
| `RSLOOP_MAX_PENDING_TLS_HANDSHAKES` | Positive integer count; `256` | Caps pending server TLS handshakes. Missing, invalid, zero, or out-of-range values use the default. |
| `RSLOOP_READER_SPIN_US` | Unsigned integer microseconds; `30` | Blocking reader workers retry nonblocking reads before polling. Zero disables spinning; values above `1000` clamp to `1000`; invalid values use `30`. |

For example, enable counters for a local script on Unix:

```bash
RSLOOP_TRANSPORT_STATS=1 python examples/03_streams.py
```

On PowerShell, set `$env:RSLOOP_TRANSPORT_STATS = "1"` before running Python.

The write cap is separate from flow-control watermarks (64 KiB high, 16 KiB
low by default). Use `drain()` with streams and protocol pause/resume callbacks
with transports; increasing the cap does not remove backpressure. Inbound
pending-read thresholds are 1 MiB to pause and 256 KiB to resume. These are
internal queue thresholds, not a bound on total connection or process memory.
The reader's stream `limit` controls line/separator handling separately.

`RSLOOP_USE_FAST_STREAMS` no longer selects a mode. TCP stream helpers always
use native streams on rsloop; see [Fast Streams](fast-streams.md).

For build-time features and profiling variables, see
[Development](development.md). Configuration above is derived from
`src/transport/stream/tuning.rs` and `src/transport/stream/stats.rs`.
