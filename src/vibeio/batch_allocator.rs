//! One cached allocation for successive scheduler batches on an owner thread.

#[cfg(all(feature = "scheduler-batch-cache", not(kani)))]
mod native {
    use std::alloc::{AllocError, Allocator, Layout, System};
    use std::cell::Cell;
    use std::ptr::NonNull;

    #[derive(Clone, Copy)]
    struct Block {
        ptr: NonNull<u8>,
        layout: Layout,
    }

    /// Not Sync: this cache belongs to the runtime's owner thread.
    #[derive(Default)]
    pub(crate) struct BatchAllocator {
        cached: Cell<Option<Block>>,
        #[cfg(test)]
        system_allocations: Cell<usize>,
    }

    #[cfg(test)]
    impl BatchAllocator {
        pub(crate) fn system_allocations(&self) -> usize {
            self.system_allocations.get()
        }
    }

    const MAX_RETAINED: usize = 16 * 1024;

    // SAFETY: all blocks are System allocations with exact layouts. Only
    // deallocated blocks enter the cache; live blocks remain disjoint and are
    // never inspected. Cell operations cannot unwind or invoke user code.
    unsafe impl Allocator for BatchAllocator {
        #[inline]
        fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
            if let Some(block) = self.cached.get()
                && block.layout == layout
            {
                self.cached.set(None);
                return Ok(NonNull::slice_from_raw_parts(block.ptr, layout.size()));
            }
            #[cfg(test)]
            self.system_allocations
                .set(self.system_allocations.get().wrapping_add(1));
            System.allocate(layout)
        }

        #[inline]
        unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
            if layout.size() != 0 && layout.size() <= MAX_RETAINED && self.cached.get().is_none() {
                self.cached.set(Some(Block { ptr, layout }));
            } else {
                // SAFETY: the caller supplies a live System-backed block with
                // its exact layout; zero-sized blocks also delegate to System.
                unsafe { System.deallocate(ptr, layout) };
            }
        }
    }

    impl Drop for BatchAllocator {
        fn drop(&mut self) {
            if let Some(block) = self.cached.take() {
                // SAFETY: the cached block was returned by its previous owner;
                // outstanding allocations are never freed by allocator drop.
                unsafe { System.deallocate(block.ptr, block.layout) };
            }
        }
    }

    pub(crate) type Batch<'a, T> = Vec<T, &'a BatchAllocator>;

    #[inline]
    pub(crate) fn batch<T>(allocator: &BatchAllocator, capacity: usize) -> Batch<'_, T> {
        Vec::with_capacity_in(capacity, allocator)
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use crate::vibeio::batch_allocator::drain_batch;

        #[test]
        fn partial_drain_drops_remaining_elements_in_order_and_keeps_capacity() {
            let allocator = BatchAllocator::default();
            let drops = std::cell::RefCell::new(Vec::new());
            struct Tracked<'a>(usize, &'a std::cell::RefCell<Vec<usize>>);
            impl Drop for Tracked<'_> {
                fn drop(&mut self) {
                    self.1.borrow_mut().push(self.0);
                }
            }
            let mut values = batch(&allocator, 8);
            values.extend((0..4).map(|i| Tracked(i, &drops)));
            let address = values.as_ptr();
            let mut drain = drain_batch(&mut values);
            assert_eq!(drain.len(), 4);
            drop(drain.next().unwrap());
            assert_eq!(drain.size_hint(), (3, Some(3)));
            drop(drain);
            assert_eq!(*drops.borrow(), [0, 1, 2, 3]);
            assert!(values.is_empty());
            assert_eq!(values.capacity(), 8);
            values.push(Tracked(4, &drops));
            assert_eq!(values.as_ptr(), address);
            drop(values);
            assert_eq!(*drops.borrow(), [0, 1, 2, 3, 4]);
            assert_eq!(allocator.system_allocations(), 1);
        }

        #[test]
        fn drain_cleanup_continues_after_element_destructor_panics() {
            let allocator = BatchAllocator::default();
            let drops = Cell::new(0);
            struct Tracked<'a>(bool, &'a Cell<usize>);
            impl Drop for Tracked<'_> {
                fn drop(&mut self) {
                    self.1.set(self.1.get() + 1);
                    assert!(!self.0, "element destructor panic");
                }
            }
            let mut values = batch(&allocator, 4);
            values.extend([
                Tracked(false, &drops),
                Tracked(true, &drops),
                Tracked(false, &drops),
            ]);
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut drain = drain_batch(&mut values);
                drop(drain.next());
                drop(drain);
            }));
            assert!(result.is_err());
            assert_eq!(drops.get(), 3);
            assert!(values.is_empty());
            drop(values);
            assert_eq!(drops.get(), 3);
        }

        #[test]
        fn drain_handles_zero_sized_elements_empty_batches_and_forgetting() {
            // Thread-local state avoids interference from parallel tests while
            // allowing a genuinely zero-sized element with a destructor.
            thread_local! { static DROPS: Cell<usize> = const { Cell::new(0) }; }
            struct Zst;
            impl Drop for Zst {
                fn drop(&mut self) {
                    DROPS.with(|drops| drops.set(drops.get() + 1));
                }
            }
            DROPS.with(|drops| drops.set(0));
            let allocator = BatchAllocator::default();
            let mut values = batch(&allocator, 4);
            values.extend([Zst, Zst, Zst]);
            let mut drain = drain_batch(&mut values);
            drop(drain.next());
            drop(drain);
            DROPS.with(|drops| assert_eq!(drops.get(), 3));
            assert!(drain_batch(&mut values).next().is_none());
            values.push(Zst);
            std::mem::forget(drain_batch(&mut values));
            assert!(values.is_empty());
            values.push(Zst);
            drop(values);
            DROPS.with(|drops| assert_eq!(drops.get(), 4));

            let mut numbers = batch(&allocator, 4);
            numbers.extend([1, 2, 3]);
            std::mem::forget(drain_batch(&mut numbers));
            numbers.push(42);
            assert_eq!(&numbers[..], &[42]);
        }

        #[test]
        fn successive_batches_reuse_storage_but_nested_batches_are_disjoint() {
            let allocator = BatchAllocator::default();
            let mut first = batch::<usize>(&allocator, 256);
            first.extend(0..256);
            let address = first.as_ptr();
            let mut nested = batch::<usize>(&allocator, 256);
            nested.push(42);
            assert_ne!(address, nested.as_ptr());
            drop(first);
            let reused = batch::<usize>(&allocator, 256);
            assert_eq!(address, reused.as_ptr());
            assert_eq!(allocator.system_allocations(), 2);
            assert!(reused.is_empty());
            assert_eq!(nested[0], 42);
        }

        #[test]
        fn growth_alignment_zeroing_and_retention_bound() {
            let allocator = BatchAllocator::default();
            let mut values = batch::<u64>(&allocator, 1);
            values.extend(0..4096);
            assert!(values.iter().copied().eq(0..4096));
            drop(values);
            assert!(allocator.cached.get().unwrap().layout.size() <= MAX_RETAINED);
            let allocator = BatchAllocator::default();
            let layout = Layout::from_size_align(32, 64).unwrap();
            let block = allocator.allocate_zeroed(layout).unwrap();
            let ptr = block.cast::<u8>();
            assert_eq!(ptr.as_ptr().addr() % 64, 0);
            // SAFETY: allocate_zeroed initialized all 32 bytes of this block.
            unsafe {
                assert!(
                    std::slice::from_raw_parts(ptr.as_ptr(), 32)
                        .iter()
                        .all(|b| *b == 0)
                );
                allocator.deallocate(ptr, layout);
            }
            assert_eq!(allocator.cached.get().unwrap().layout, layout);
            let empty = batch::<()>(&allocator, 256);
            drop(empty);
        }

        #[test]
        fn unwinding_drops_elements_before_recycling_storage() {
            let allocator = BatchAllocator::default();
            let drops = Cell::new(0);
            struct Tracked<'a>(&'a Cell<usize>);
            impl Drop for Tracked<'_> {
                fn drop(&mut self) {
                    self.0.set(self.0.get() + 1);
                }
            }
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let mut values = batch(&allocator, 256);
                values.push(Tracked(&drops));
                panic!("exercise batch unwinding");
            }));
            assert_eq!(drops.get(), 1);
            assert!(allocator.cached.get().is_some());
        }

        #[test]
        fn reused_storage_is_zeroed_and_mismatched_layouts_do_not_consume_cache() {
            let allocator = BatchAllocator::default();
            let layout = Layout::from_size_align(32, 8).unwrap();
            let block = allocator.allocate(layout).unwrap().cast::<u8>();
            // SAFETY: this is a live 32-byte allocation made with layout.
            unsafe {
                block.as_ptr().write_bytes(0xA5, 32);
                allocator.deallocate(block, layout);
            }
            let different = Layout::from_size_align(32, 64).unwrap();
            let aligned = allocator.allocate(different).unwrap().cast::<u8>();
            assert_eq!(allocator.cached.get().unwrap().ptr, block);
            let zeroed = allocator.allocate_zeroed(layout).unwrap().cast::<u8>();
            assert_eq!(zeroed, block);
            // SAFETY: both blocks remain live with the respective exact layouts;
            // allocate_zeroed initialized every byte read below.
            unsafe {
                assert!(
                    std::slice::from_raw_parts(zeroed.as_ptr(), 32)
                        .iter()
                        .all(|b| *b == 0)
                );
                allocator.deallocate(zeroed, layout);
                allocator.deallocate(aligned, different);
            }
        }

        #[test]
        fn zero_and_oversized_allocations_are_never_retained() {
            let allocator = BatchAllocator::default();
            for size in [0, MAX_RETAINED + 1] {
                let layout = Layout::from_size_align(size, 8).unwrap();
                let block = allocator.allocate(layout).unwrap().cast::<u8>();
                // SAFETY: this live allocation was returned for layout above.
                unsafe { allocator.deallocate(block, layout) };
                assert!(allocator.cached.get().is_none());
            }
        }
    }
}

#[cfg(all(feature = "scheduler-batch-cache", not(kani)))]
pub(crate) use native::{Batch, BatchAllocator, batch};

#[cfg(all(feature = "scheduler-batch-cache", not(kani)))]
pub(crate) use draining::drain_batch;

#[cfg(all(feature = "scheduler-batch-cache", not(kani)))]
mod draining {
    use super::Batch;

    /// Full-range drain using stabilized Vec primitives. The allocation stays with
    /// its Vec, and the iterator owns the removed elements until they are yielded.
    pub(crate) struct BatchDrain<'a, T> {
        remaining: &'a mut [T],
    }

    pub(crate) fn drain_batch<'a, T>(batch: &'a mut Batch<'_, T>) -> BatchDrain<'a, T> {
        let len = batch.len();
        // SAFETY: the old len elements are initialized. Setting len to zero transfers
        // their drop responsibility to the iterator, whose borrow prevents the Vec
        // from moving/freeing its buffer until the iterator is dropped or forgotten.
        unsafe {
            batch.set_len(0);
            BatchDrain {
                remaining: std::slice::from_raw_parts_mut(batch.as_mut_ptr(), len),
            }
        }
    }

    impl<T> Iterator for BatchDrain<'_, T> {
        type Item = T;

        #[inline]
        fn next(&mut self) -> Option<T> {
            let (first, remaining) = std::mem::take(&mut self.remaining).split_first_mut()?;
            self.remaining = remaining;
            // SAFETY: first is initialized and has been removed from the iterator's
            // remaining slice. Neither the iterator nor Vec will drop it again.
            Some(unsafe { std::ptr::read(first) })
        }

        fn size_hint(&self) -> (usize, Option<usize>) {
            (self.remaining.len(), Some(self.remaining.len()))
        }
    }

    impl<T> ExactSizeIterator for BatchDrain<'_, T> {}

    impl<T> Drop for BatchDrain<'_, T> {
        fn drop(&mut self) {
            // SAFETY: only initialized, unyielded elements remain. Slice drop glue
            // also drops later elements if one destructor panics.
            unsafe { std::ptr::drop_in_place(self.remaining) };
        }
    }
}

// Default builds preserve the original allocation/drain path. Kani's compiler
// also predates stabilization, so its scheduler proofs use this implementation.
#[cfg(any(kani, not(feature = "scheduler-batch-cache")))]
mod ordinary {
    #[derive(Default)]
    pub(crate) struct BatchAllocator {}
    pub(crate) type Batch<'a, T> = Vec<T>;
    #[inline]
    pub(crate) fn batch<T>(_: &BatchAllocator, capacity: usize) -> Batch<'_, T> {
        Vec::with_capacity(capacity)
    }
    #[inline]
    pub(crate) fn drain_batch<'a, T>(batch: &'a mut Batch<'_, T>) -> std::vec::Drain<'a, T> {
        batch.drain(..)
    }
}

#[cfg(any(kani, not(feature = "scheduler-batch-cache")))]
pub(crate) use ordinary::{Batch, BatchAllocator, batch, drain_batch};
