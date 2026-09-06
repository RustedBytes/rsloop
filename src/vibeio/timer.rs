use std::cell::RefCell;
use std::task::Waker;
use std::time::{Duration, Instant};

use slab::Slab;

/// Stable identifier for a timer. The generation prevents a stale cancellation
/// from removing a newer timer that reused the same slab slot.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct TimerHandle {
    slab_index: usize,
    generation: u64,
}

struct TimerEntry {
    deadline: Instant,
    waker: Waker,
    generation: u64,
    heap_index: usize,
}

/// Indexed four-ary min heap. Four children per node reduce heap depth and keep
/// the nodes inspected during sift operations close together in cache.
struct DeadlineHeap {
    entries: Slab<TimerEntry>,
    heap: Vec<usize>,
    next_generation: u64,
}

impl DeadlineHeap {
    #[inline]
    fn new() -> Self {
        Self {
            entries: Slab::new(),
            heap: Vec::new(),
            next_generation: 0,
        }
    }

    #[inline]
    fn less(&self, left: usize, right: usize) -> bool {
        let left = &self.entries[self.heap[left]];
        let right = &self.entries[self.heap[right]];
        (left.deadline, left.generation) < (right.deadline, right.generation)
    }

    #[inline]
    fn swap_nodes(&mut self, left: usize, right: usize) {
        self.heap.swap(left, right);
        self.entries[self.heap[left]].heap_index = left;
        self.entries[self.heap[right]].heap_index = right;
    }

    fn sift_up(&mut self, mut index: usize) {
        while index != 0 {
            let parent = (index - 1) / 4;
            if !self.less(index, parent) {
                break;
            }
            self.swap_nodes(index, parent);
            index = parent;
        }
    }

    fn sift_down(&mut self, mut index: usize) {
        loop {
            let first_child = index * 4 + 1;
            if first_child >= self.heap.len() {
                return;
            }

            let mut smallest = first_child;
            for child in (first_child + 1)..(first_child + 4).min(self.heap.len()) {
                if self.less(child, smallest) {
                    smallest = child;
                }
            }
            if !self.less(smallest, index) {
                return;
            }
            self.swap_nodes(index, smallest);
            index = smallest;
        }
    }

    #[inline]
    fn insert(&mut self, deadline: Instant, waker: Waker) -> TimerHandle {
        self.next_generation = self.next_generation.wrapping_add(1);
        if self.next_generation == 0 {
            self.next_generation = 1;
        }
        let generation = self.next_generation;
        let heap_index = self.heap.len();
        let slab_index = self.entries.insert(TimerEntry {
            deadline,
            waker,
            generation,
            heap_index,
        });
        self.heap.push(slab_index);
        self.sift_up(heap_index);
        TimerHandle {
            slab_index,
            generation,
        }
    }

    #[inline]
    fn deadline(&self) -> Option<Instant> {
        self.heap.first().map(|index| self.entries[*index].deadline)
    }

    fn remove(&mut self, handle: TimerHandle) -> Option<Waker> {
        let entry = self.entries.get(handle.slab_index)?;
        if entry.generation != handle.generation {
            return None;
        }

        let heap_index = entry.heap_index;
        let last = self.heap.len() - 1;
        if heap_index != last {
            self.swap_nodes(heap_index, last);
        }
        self.heap.pop();
        let entry = self.entries.remove(handle.slab_index);

        if heap_index < self.heap.len() {
            let parent = heap_index.checked_sub(1).map(|index| index / 4);
            if parent.is_some_and(|parent| self.less(heap_index, parent)) {
                self.sift_up(heap_index);
            } else {
                self.sift_down(heap_index);
            }
        }
        Some(entry.waker)
    }

    #[inline]
    fn pop_expired(&mut self, now: Instant, output: &mut Vec<Waker>) {
        while self.deadline().is_some_and(|deadline| deadline <= now) {
            let slab_index = self.heap[0];
            let generation = self.entries[slab_index].generation;
            if let Some(waker) = self.remove(TimerHandle {
                slab_index,
                generation,
            }) {
                output.push(waker);
            }
        }
    }
}

pub struct Timer {
    deadlines: RefCell<DeadlineHeap>,
    expired: RefCell<Vec<Waker>>,
}

impl Timer {
    #[inline]
    pub fn new() -> Self {
        Self {
            deadlines: RefCell::new(DeadlineHeap::new()),
            expired: RefCell::new(Vec::with_capacity(16)),
        }
    }

    #[inline]
    pub fn submit(&self, deadline: Instant, waker: Waker) -> Option<TimerHandle> {
        if deadline <= Instant::now() {
            waker.wake();
            return None;
        }
        Some(self.deadlines.borrow_mut().insert(deadline, waker))
    }

    #[inline]
    pub fn cancel(&self, handle: TimerHandle) {
        let waker = self.deadlines.borrow_mut().remove(handle);
        // A waker destructor may reenter the timer.
        drop(waker);
    }

    /// Replace a live registration's waiter without changing its heap position.
    /// Returns false if the handle has expired, been cancelled, or been reused.
    pub(crate) fn update_waker(&self, handle: TimerHandle, waker: &Waker) -> bool {
        {
            let deadlines = self.deadlines.borrow();
            let Some(entry) = deadlines
                .entries
                .get(handle.slab_index)
                .filter(|entry| entry.generation == handle.generation)
            else {
                return false;
            };
            if entry.waker.will_wake(waker) {
                return true;
            }
        }
        // Custom clone/drop callbacks may reenter the timer. Revalidate after
        // cloning and keep discarded ownership outside the heap borrow.
        let mut incoming = waker.clone();
        let mut deadlines = self.deadlines.borrow_mut();
        let updated = if let Some(entry) = deadlines
            .entries
            .get_mut(handle.slab_index)
            .filter(|entry| entry.generation == handle.generation)
        {
            std::mem::swap(&mut entry.waker, &mut incoming);
            true
        } else {
            false
        };
        drop(deadlines);
        drop(incoming);
        updated
    }

    /// Wakes every expired timer and returns the exact duration until the next
    /// deadline. Unlike the old millisecond wheel this never discards partial
    /// elapsed time, so frequent scheduler spins cannot freeze timer progress.
    #[inline]
    pub fn spin_and_get_deadline(&self) -> (Option<Duration>, bool) {
        let now = Instant::now();
        let mut expired = std::mem::take(&mut *self.expired.borrow_mut());
        {
            let mut deadlines = self.deadlines.borrow_mut();
            deadlines.pop_expired(now, &mut expired);
        }
        let woken_up = !expired.is_empty();
        for waker in expired.drain(..) {
            waker.wake();
        }
        // Keep reusable capacity without holding a RefMut through callbacks.
        // A nested spin may have returned its own allocation in the meantime.
        let mut cache = self.expired.borrow_mut();
        if expired.capacity() > cache.capacity() {
            *cache = expired;
        }
        // Callbacks may have inserted/cancelled deadlines or taken time to run.
        let deadline = self.deadlines.borrow().deadline();
        let now = Instant::now();
        (
            deadline.map(|deadline| deadline.saturating_duration_since(now)),
            woken_up,
        )
    }
}

impl Default for Timer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicUsize, Ordering};

    thread_local! {
        static REENTER: RefCell<Option<Box<dyn Fn()>>> = RefCell::new(None);
    }
    struct ReentryScope;
    impl Drop for ReentryScope {
        fn drop(&mut self) {
            let callback = REENTER.with(|slot| slot.borrow_mut().take());
            drop(callback);
        }
    }
    fn reenter() {
        REENTER.with(|slot| {
            if let Some(callback) = slot.borrow().as_ref() {
                callback();
            }
        });
    }
    struct ReenterOnWake;
    impl std::task::Wake for ReenterOnWake {
        fn wake(self: Arc<Self>) {
            reenter();
        }
    }
    struct ReenterOnDrop;
    #[allow(
        clippy::manual_noop_waker,
        reason = "This waker must own a reentrant destructor; Waker::noop cannot exercise cancellation cleanup"
    )]
    impl std::task::Wake for ReenterOnDrop {
        fn wake(self: Arc<Self>) {}
    }
    impl Drop for ReenterOnDrop {
        fn drop(&mut self) {
            reenter();
        }
    }

    #[test]
    fn updating_waiter_preserves_heap_and_rejects_stale_handles() {
        let timer = Timer::new();
        let deadline = Instant::now() + Duration::from_secs(60);
        let first = Arc::new(AtomicUsize::new(0));
        let second = Arc::new(AtomicUsize::new(0));
        let handle = timer.submit(deadline, test_waker(&first)).unwrap();
        let layout = timer.deadlines.borrow().heap.clone();
        assert!(timer.update_waker(handle, &test_waker(&second)));
        assert_eq!(timer.deadlines.borrow().heap, layout);
        assert_eq!(timer.deadlines.borrow().deadline(), Some(deadline));
        timer.cancel(handle);
        let next = timer.submit(deadline, test_waker(&first)).unwrap();
        assert_eq!(handle.slab_index, next.slab_index);
        assert!(!timer.update_waker(handle, &test_waker(&second)));
        let mut expired = Vec::new();
        timer
            .deadlines
            .borrow_mut()
            .pop_expired(deadline, &mut expired);
        for waker in expired {
            waker.wake();
        }
        assert_eq!(first.load(Ordering::SeqCst), 1);
        assert_eq!(second.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn updating_waiter_drops_replaced_waker_outside_heap_borrow() {
        let timer = std::rc::Rc::new(Timer::new());
        let inner = timer.clone();
        let calls = std::rc::Rc::new(std::cell::Cell::new(0));
        let called = calls.clone();
        REENTER.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                assert!(inner.spin_and_get_deadline().0.is_some());
                called.set(called.get() + 1);
            }));
        });
        let _scope = ReentryScope;
        let handle = timer
            .submit(
                Instant::now() + Duration::from_secs(60),
                Waker::from(Arc::new(ReenterOnDrop)),
            )
            .unwrap();
        assert!(timer.update_waker(handle, Waker::noop()));
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn cancellation_drops_wakers_outside_heap_borrow() {
        let timer = std::rc::Rc::new(Timer::new());
        let calls = std::rc::Rc::new(std::cell::Cell::new(0));
        let inner = timer.clone();
        let called = calls.clone();
        REENTER.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                assert_eq!(inner.spin_and_get_deadline(), (None, false));
                called.set(called.get() + 1);
            }))
        });
        let _scope = ReentryScope;
        let handle = timer
            .submit(
                Instant::now() + Duration::from_secs(60),
                Waker::from(Arc::new(ReenterOnDrop)),
            )
            .unwrap();
        timer.cancel(handle);
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn expiry_allows_nested_spin_and_returns_callback_inserted_deadline() {
        let timer = std::rc::Rc::new(Timer::new());
        let inner = timer.clone();
        let next = Instant::now() + Duration::from_secs(60);
        REENTER.with(|slot| {
            *slot.borrow_mut() = Some(Box::new(move || {
                assert_eq!(inner.spin_and_get_deadline(), (None, false));
                inner.submit(next, Waker::noop().clone()).unwrap();
            }))
        });
        let _scope = ReentryScope;
        // Insert directly to model an expired registration without sleeping.
        timer
            .deadlines
            .borrow_mut()
            .insert(Instant::now(), Waker::from(Arc::new(ReenterOnWake)));
        let (deadline, woke) = timer.spin_and_get_deadline();
        assert!(woke);
        assert!(deadline.is_some());
        assert_eq!(timer.deadlines.borrow().deadline(), Some(next));
        assert!(timer.expired.borrow().is_empty());
    }

    fn test_waker(counter: &Arc<AtomicUsize>) -> Waker {
        struct WakeCounter(Arc<AtomicUsize>);
        impl std::task::Wake for WakeCounter {
            fn wake(self: Arc<Self>) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
            fn wake_by_ref(self: &Arc<Self>) {
                self.0.fetch_add(1, Ordering::Relaxed);
            }
        }
        Waker::from(Arc::new(WakeCounter(Arc::clone(counter))))
    }

    #[test]
    fn nearest_deadline_and_cancel_are_exact() {
        let timer = Timer::new();
        let counter = Arc::new(AtomicUsize::new(0));
        let later = timer
            .submit(
                Instant::now() + Duration::from_secs(2),
                test_waker(&counter),
            )
            .unwrap();
        let sooner = timer
            .submit(
                Instant::now() + Duration::from_secs(1),
                test_waker(&counter),
            )
            .unwrap();
        let (deadline, woke) = timer.spin_and_get_deadline();
        assert!(!woke);
        assert!(deadline.is_some_and(|duration| duration < Duration::from_secs(2)));
        timer.cancel(sooner);
        timer.cancel(later);
        assert_eq!(timer.spin_and_get_deadline(), (None, false));
    }

    #[test]
    fn stale_handle_cannot_cancel_reused_slot() {
        let timer = Timer::new();
        let counter = Arc::new(AtomicUsize::new(0));
        let stale = timer
            .submit(
                Instant::now() + Duration::from_secs(1),
                test_waker(&counter),
            )
            .unwrap();
        timer.cancel(stale);
        let current = timer
            .submit(
                Instant::now() + Duration::from_secs(2),
                test_waker(&counter),
            )
            .unwrap();
        timer.cancel(stale);
        assert!(timer.spin_and_get_deadline().0.is_some());
        timer.cancel(current);
    }

    #[test]
    fn expired_timers_wake_as_a_batch() {
        let timer = Timer::new();
        let counter = Arc::new(AtomicUsize::new(0));
        let deadline = Instant::now() + Duration::from_millis(2);
        for _ in 0..8 {
            timer.submit(deadline, test_waker(&counter)).unwrap();
        }
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(timer.spin_and_get_deadline(), (None, true));
        assert_eq!(counter.load(Ordering::Relaxed), 8);
    }

    #[test]
    fn sub_millisecond_deadline_is_not_rounded_to_immediate() {
        let counter = Arc::new(AtomicUsize::new(0));
        let mut deadlines = DeadlineHeap::new();
        let deadline = Instant::now() + Duration::from_micros(500);
        let handle = deadlines.insert(deadline, test_waker(&counter));
        assert_eq!(deadlines.deadline(), Some(deadline));
        deadlines.remove(handle);
    }
}
