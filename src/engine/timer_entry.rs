//! Timer-heap entry with stable ordering for equal deadlines.

use std::{
    cmp::Ordering,
    collections::{BinaryHeap, VecDeque},
    sync::Arc,
    time::Instant,
};

#[cfg_attr(feature = "profile", hotpath::measure)]
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

/// Immediately due timers normally arrive in deadline order and use a FIFO.
/// Positive delays and out-of-order arrivals use the ordinary heap. Keep the
/// two paths separate so scattered future deadlines pay no FIFO comparisons.
/// Neither insertion nor removal drops callbacks while the queue is borrowed.
#[derive(Default)]
pub(super) struct TimerQueue {
    ordered: VecDeque<TimerEntry>,
    unordered: BinaryHeap<TimerEntry>,
}

impl TimerQueue {
    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "TimerQueue"))]
    pub(super) fn new() -> Self {
        Self::default()
    }

    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "TimerQueue"))]
    pub(super) fn is_empty(&self) -> bool {
        self.ordered.is_empty() && self.unordered.is_empty()
    }

    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "TimerQueue"))]
    pub(super) fn push(&mut self, entry: TimerEntry, immediate: bool) {
        // TimerEntry's comparison is reversed for the min-deadline heap.
        // Remote scheduling/merging can reorder even zero-delay timers.
        if immediate && self.ordered.back().is_none_or(|last| entry <= *last) {
            self.ordered.push_back(entry);
        } else {
            self.unordered.push(entry);
        }
    }

    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "TimerQueue"))]
    pub(super) fn peek(&self) -> Option<&TimerEntry> {
        match (self.ordered.front(), self.unordered.peek()) {
            (Some(front), Some(top)) if top > front => Some(top),
            (Some(front), _) => Some(front),
            (None, top) => top,
        }
    }

    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "TimerQueue"))]
    pub(super) fn pop(&mut self) -> Option<TimerEntry> {
        match (self.ordered.front(), self.unordered.peek()) {
            (Some(front), Some(top)) if top > front => self.unordered.pop(),
            (Some(_), _) => self.ordered.pop_front(),
            (None, _) => self.unordered.pop(),
        }
    }

    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "TimerQueue"))]
    pub(super) fn append_heap(&mut self, other: &mut BinaryHeap<TimerEntry>) {
        self.unordered.append(other);
    }

    #[cfg_attr(feature = "profile", hotpath::measure(impl_type = "TimerQueue"))]
    pub(super) fn drain_into(&mut self, other: &mut BinaryHeap<TimerEntry>) {
        other.append(&mut self.unordered);
        other.extend(self.ordered.drain(..));
    }
}

impl PartialEq for TimerEntry {
    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "<TimerEntry as PartialEq>")
    )]
    fn eq(&self, other: &Self) -> bool {
        self.when == other.when && self.seq == other.seq
    }
}

impl Eq for TimerEntry {}

impl PartialOrd for TimerEntry {
    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "<TimerEntry as PartialOrd>")
    )]
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for TimerEntry {
    #[cfg_attr(
        feature = "profile",
        hotpath::measure(impl_type = "<TimerEntry as Ord>")
    )]
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
    use std::{
        collections::BinaryHeap,
        sync::Arc,
        time::{Duration, Instant},
    };

    use pyo3::{prelude::*, types::PyTuple};

    use super::{TimerEntry, TimerQueue};
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

    proptest::proptest! {
        #[test]
        fn timer_queue_matches_heap_through_insert_pop_and_merge(
            operations in proptest::collection::vec((0_u8..6, 0_u64..100), 1..300)
        ) {
            crate::initialize_python_for_tests();
            Python::attach(|py| {
                let now = Instant::now();
                let mut queues = [TimerQueue::new(), TimerQueue::new()];
                let mut heaps = [BinaryHeap::new(), BinaryHeap::new()];
                for (seq, (op, delay)) in operations.into_iter().enumerate() {
                    let index = usize::from(op % 2);
                    match op {
                        0 | 1 => {
                            let when = now + Duration::from_millis(delay);
                            queues[index].push(timer_entry(py, when, seq as u64), seq % 3 == 0);
                            heaps[index].push(timer_entry(py, when, seq as u64));
                        }
                        2 | 3 => {
                            let actual = queues[index].pop().map(|e| (e.when, e.seq));
                            let expected = heaps[index].pop().map(|e| (e.when, e.seq));
                            assert_eq!(actual, expected);
                        }
                        _ => {
                            let mut pending = BinaryHeap::new();
                            queues[1 - index].drain_into(&mut pending);
                            assert!(queues[1 - index].is_empty());
                            queues[index].append_heap(&mut pending);
                            assert!(pending.is_empty());
                            let mut other = std::mem::take(&mut heaps[1 - index]);
                            heaps[index].append(&mut other);
                        }
                    }
                    for i in 0..2 {
                        assert_eq!(queues[i].is_empty(), heaps[i].is_empty());
                        assert_eq!(queues[i].peek().map(|e| (e.when, e.seq)),
                                   heaps[i].peek().map(|e| (e.when, e.seq)));
                    }
                }
                for i in 0..2 {
                    while let Some(expected) = heaps[i].pop() {
                        let actual = queues[i].pop().expect("remaining timer");
                        assert_eq!((actual.when, actual.seq), (expected.when, expected.seq));
                    }
                    assert!(queues[i].pop().is_none());
                }
            });
        }
    }
}
