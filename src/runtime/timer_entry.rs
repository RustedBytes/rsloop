use std::cmp::Ordering;
use std::sync::Arc;
use std::time::Instant;

pub(super) struct TimerEntry {
    pub(super) when: Instant,
    pub(super) seq: u64,
    pub(super) callback: Arc<crate::callbacks::ReadyCallback>,
}

impl PartialEq for TimerEntry {
    fn eq(&self, other: &Self) -> bool {
        self.when == other.when && self.seq == other.seq
    }
}

impl Eq for TimerEntry {}

impl PartialOrd for TimerEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TimerEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        other
            .when
            .cmp(&self.when)
            .then_with(|| other.seq.cmp(&self.seq))
    }
}
