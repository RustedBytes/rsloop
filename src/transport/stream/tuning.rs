//! Sizing constants and their environment-variable overrides.
//!
//! The compile-time defaults are what every transport uses in practice; the
//! `RSLOOP_*` overrides exist so a deployment can retune buffering, TLS
//! admission, and reader spinning without a rebuild. Each override is resolved
//! once into a `OnceLock` so the hot paths stay a plain load.

use std::time::Duration;

pub(super) const DEFAULT_WRITE_BUFFER_HIGH_WATER: usize = 64 * 1024;
pub(super) const DEFAULT_WRITE_BUFFER_LOW_WATER: usize = DEFAULT_WRITE_BUFFER_HIGH_WATER / 4;
const DEFAULT_MAX_WRITE_BUFFER_SIZE: usize = 64 * 1024 * 1024;
pub(super) const MAX_PENDING_READ_COALESCE_BYTES: usize = 256 * 1024;
pub(super) const MAX_READ_EVENTS_PER_DRAIN: usize = 16;
pub(super) const MAX_READ_BYTES_PER_DRAIN: usize = 128 * 1024;
pub(super) const PENDING_READ_HIGH_WATER: usize = 1024 * 1024;
pub(super) const PENDING_READ_LOW_WATER: usize = PENDING_READ_HIGH_WATER / 4;
// Batch ordinary protocol writes for one loop turn. Besides joining a protocol
// header with its body, this lets the loop finish a group of ready connection
// callbacks before their peer readers wake it again, so one batch of writes
// costs one reader wake instead of one per message. Tie the upper bound to a
// normal socket-read block rather than a header-size guess: complete framed
// messages are often larger than their header, while bulk writes remain direct.
pub(super) const SMALL_WRITE_COALESCE_MIN_BYTES: usize = 64;
pub(super) const SMALL_WRITE_COALESCE_MAX_BYTES: usize = STREAM_READ_BUFFER_SIZE;
pub(super) const BLOCKING_POLL_INTERVAL_MS: i32 = 50;
pub(super) const WORKER_JOIN_TIMEOUT: Duration = Duration::from_secs(5);
// Keep the per-connection allocation modest. Large reads are delivered in
// several owned chunks, while the pending-event drain can still coalesce the
// slow protocol path up to MAX_PENDING_READ_COALESCE_BYTES.
pub(super) const STREAM_READ_BUFFER_SIZE: usize = 16 * 1024;
pub(super) const MAX_STREAM_READ_BUFFER_SIZE: usize = 64 * 1024;
#[cfg(windows)]
pub(super) const SERVER_POLL_READER_WRITE_THRESHOLD: usize = STREAM_READ_BUFFER_SIZE;
#[cfg(windows)]
pub(super) const SERVER_POLL_READER_TINY_TRIGGER_MAX_BYTES: usize = 16;
// Keep enough buffers for one reader, one native stream buffer, and a short
// loop-thread backlog. Buffers are allocated lazily; once all slots exist,
// readers wait for recycling rather than allocating beyond this bound.
pub(super) const READ_BUFFER_POOL_LIMIT: usize = 4;
// Amortize tiny headers without reserving a full 16 KiB block per connection.
pub(super) const MIN_WRITE_BUFFER_CAPACITY: usize = 256;
// This unit defines the pool's slot budget, not its initial allocation size.
pub(super) const WRITE_BUFFER_BLOCK_SIZE: usize = 16 * 1024;
pub(super) const WRITE_BUFFER_POOL_LIMIT: usize =
    DEFAULT_WRITE_BUFFER_HIGH_WATER / WRITE_BUFFER_BLOCK_SIZE + 1;
pub(super) const TLS_WORKER_STACK_SIZE: usize = 256 * 1024;
const DEFAULT_MAX_PENDING_TLS_HANDSHAKES: usize = 256;

#[cfg_attr(feature = "profile", hotpath::measure)]
fn positive_usize_or_default(value: Option<usize>, default: usize) -> usize {
    value.filter(|value| *value > 0).unwrap_or(default)
}

#[cfg_attr(feature = "profile", hotpath::measure)]
fn reader_spin_micros(value: Option<u64>) -> u64 {
    value.unwrap_or(30).min(1_000)
}

#[cfg_attr(feature = "profile", hotpath::measure)]
fn parse_positive_usize(value: Option<&str>, default: usize) -> usize {
    positive_usize_or_default(
        value.and_then(|value| value.trim().parse::<usize>().ok()),
        default,
    )
}

#[cfg_attr(feature = "profile", hotpath::measure)]
fn parse_reader_spin_window(value: Option<&str>) -> Duration {
    Duration::from_micros(reader_spin_micros(
        value.and_then(|value| value.trim().parse::<u64>().ok()),
    ))
}

#[cfg_attr(feature = "profile", hotpath::measure)]
pub(super) fn max_pending_tls_handshakes() -> usize {
    static LIMIT: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *LIMIT.get_or_init(|| {
        let value = std::env::var("RSLOOP_MAX_PENDING_TLS_HANDSHAKES").ok();
        parse_positive_usize(value.as_deref(), DEFAULT_MAX_PENDING_TLS_HANDSHAKES)
    })
}

#[cfg_attr(feature = "profile", hotpath::measure)]
pub(super) fn max_write_buffer_size() -> usize {
    static LIMIT: std::sync::OnceLock<usize> = std::sync::OnceLock::new();
    *LIMIT.get_or_init(|| {
        let value = std::env::var("RSLOOP_MAX_WRITE_BUFFER_BYTES").ok();
        parse_positive_usize(value.as_deref(), DEFAULT_MAX_WRITE_BUFFER_SIZE)
    })
}

// After a successful read on a blocking reader worker, retry non-blocking
// reads for this long before falling back to poll(). Request/response peers
// usually answer within microseconds, and skipping the poll() sleep/wake
// halves per-roundtrip latency on actively chatting connections.
#[cfg_attr(feature = "profile", hotpath::measure)]
pub(super) fn reader_spin_window() -> Duration {
    static WINDOW: std::sync::OnceLock<Duration> = std::sync::OnceLock::new();
    *WINDOW.get_or_init(|| {
        let value = std::env::var("RSLOOP_READER_SPIN_US").ok();
        parse_reader_spin_window(value.as_deref())
    })
}

#[cfg(kani)]
mod verification {
    use super::{positive_usize_or_default, reader_spin_micros};

    #[kani::proof]
    fn merge_positive_tuning_values_preserve_override_or_default() {
        let value: Option<usize> = kani::any();
        let default: usize = kani::any();
        kani::assume(default > 0);

        let selected = positive_usize_or_default(value, default);
        assert!(selected > 0);
        match value {
            Some(value) if value > 0 => assert_eq!(selected, value),
            _ => assert_eq!(selected, default),
        }
    }

    #[kani::proof]
    fn merge_reader_spin_window_defaults_and_clamps() {
        let value: Option<u64> = kani::any();
        let micros = reader_spin_micros(value);

        assert!(micros <= 1_000);
        assert_eq!(micros, value.unwrap_or(30).min(1_000));
        if value.is_none() {
            assert_eq!(micros, 30);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::{parse_positive_usize, parse_reader_spin_window};

    #[test]
    fn positive_usize_override_uses_only_valid_positive_values() {
        assert_eq!(parse_positive_usize(None, 256), 256);
        assert_eq!(parse_positive_usize(Some(""), 256), 256);
        assert_eq!(parse_positive_usize(Some("invalid"), 256), 256);
        assert_eq!(parse_positive_usize(Some("0"), 256), 256);
        assert_eq!(parse_positive_usize(Some(" 512 "), 256), 512);
    }

    #[test]
    fn reader_spin_window_defaults_and_clamps() {
        assert_eq!(parse_reader_spin_window(None), Duration::from_micros(30));
        assert_eq!(
            parse_reader_spin_window(Some("invalid")),
            Duration::from_micros(30)
        );
        assert_eq!(
            parse_reader_spin_window(Some(" 125 ")),
            Duration::from_micros(125)
        );
        assert_eq!(
            parse_reader_spin_window(Some("1001")),
            Duration::from_millis(1)
        );
    }
}
