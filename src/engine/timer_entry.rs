//! Timer-heap entry with stable ordering for equal deadlines.

use std::cmp::Ordering;
use std::sync::Arc;
use std::time::Instant;

fn compare_timer_parts<T: Ord>(
    left_when: &T,
    left_seq: u64,
    right_when: &T,
    right_seq: u64,
) -> Ordering {
    // `BinaryHeap` is a max-heap, so reverse both keys to pop the earliest
    // deadline first and preserve insertion order for ties.
    right_when
        .cmp(left_when)
        .then_with(|| right_seq.cmp(&left_seq))
}

pub(super) struct TimerEntry {
    pub(super) when: Instant,
    pub(super) seq: u64,
    pub(super) callback: Arc<super::callbacks::ReadyCallback>,
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
        compare_timer_parts(&self.when, self.seq, &other.when, other.seq)
    }
}

#[cfg(kani)]
mod verification {
    use std::cmp::Ordering;

    use super::compare_timer_parts;

    #[derive(Clone, Copy, Debug, Eq, PartialEq)]
    struct TimerKey {
        deadline: u64,
        sequence: u64,
    }

    impl Ord for TimerKey {
        fn cmp(&self, other: &Self) -> Ordering {
            compare_timer_parts(
                &self.deadline,
                self.sequence,
                &other.deadline,
                other.sequence,
            )
        }
    }

    impl PartialOrd for TimerKey {
        fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
            Some(self.cmp(other))
        }
    }

    #[kani::proof]
    fn merge_timer_key_obeys_total_order_laws() {
        let a = TimerKey {
            deadline: kani::any(),
            sequence: kani::any(),
        };
        let b = TimerKey {
            deadline: kani::any(),
            sequence: kani::any(),
        };
        let c = TimerKey {
            deadline: kani::any(),
            sequence: kani::any(),
        };

        assert_eq!(a.cmp(&a), Ordering::Equal);
        assert_eq!(a.cmp(&b), b.cmp(&a).reverse());
        assert_eq!(a.cmp(&b) == Ordering::Equal, a == b);
        if a <= b && b <= c {
            assert!(a <= c);
        }
    }

    #[kani::proof]
    fn merge_timer_key_preserves_equal_deadline_sequence_order() {
        let deadline: u64 = kani::any();
        let earlier: u64 = kani::any();
        let later: u64 = kani::any();
        kani::assume(earlier < later);

        let earlier = TimerKey {
            deadline,
            sequence: earlier,
        };
        let later = TimerKey {
            deadline,
            sequence: later,
        };
        assert!(earlier > later);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BinaryHeap;
    use std::sync::Arc;
    use std::time::{Duration, Instant};

    use pyo3::prelude::*;
    use pyo3::types::PyTuple;

    use super::TimerEntry;
    use crate::engine::callbacks::{CallbackKind, ReadyCallback};

    fn timer_entry(py: Python<'_>, when: Instant, seq: u64) -> TimerEntry {
        TimerEntry {
            when,
            seq,
            callback: Arc::new(ReadyCallback::new(
                py,
                seq,
                CallbackKind::Timer,
                py.None(),
                PyTuple::empty(py).unbind(),
                py.None(),
                false,
            )),
        }
    }

    #[test]
    fn heap_pops_earliest_deadline_and_preserves_tie_order() {
        crate::initialize_python_for_tests();
        Python::attach(|py| {
            let now = Instant::now();
            let mut heap = BinaryHeap::new();
            heap.push(timer_entry(py, now + Duration::from_millis(20), 0));
            heap.push(timer_entry(py, now, 2));
            heap.push(timer_entry(py, now, 1));

            let popped = heap.pop().expect("first timer");
            assert_eq!(popped.when, now);
            assert_eq!(popped.seq, 1);

            let popped = heap.pop().expect("second timer");
            assert_eq!(popped.when, now);
            assert_eq!(popped.seq, 2);

            let popped = heap.pop().expect("third timer");
            assert_eq!(popped.when, now + Duration::from_millis(20));
            assert_eq!(popped.seq, 0);
            assert!(heap.is_empty());
        });
    }
}
