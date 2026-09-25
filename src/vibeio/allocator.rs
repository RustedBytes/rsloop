//! Experimental bounded allocator evaluated for event-loop collections.

#[cfg(not(kani))]
mod native {
    use std::alloc::{AllocError, Allocator, AllocatorClone, Layout, System};
    use std::ptr::NonNull;
    use std::sync::{Arc, Mutex, MutexGuard};

    const MIN_CACHED_SIZE: usize = 8;
    const MAX_CACHED_SIZE: usize = 4 * 1024;
    const MAX_CACHED_ALIGN: usize = 64;
    const MAX_BLOCKS_PER_BIN: usize = 32;
    const MAX_RETAINED_BYTES: usize = 256 * 1024;
    const SIZE_BIN_COUNT: usize = 10;
    const ALIGN_BIN_COUNT: usize = 7;
    const BIN_COUNT: usize = SIZE_BIN_COUNT * ALIGN_BIN_COUNT;

    #[derive(Clone, Copy)]
    struct CachedBlock {
        pointer: NonNull<u8>,
        layout: Layout,
    }

    // SAFETY: a cached block has been invalidated by `deallocate`; its bytes
    // are inaccessible until the pool transfers ownership from under its mutex.
    unsafe impl Send for CachedBlock {}

    struct PoolState {
        bins: Vec<Vec<CachedBlock>>,
        retained_bytes: usize,
        #[cfg(test)]
        hits: usize,
        #[cfg(test)]
        misses: usize,
    }

    impl PoolState {
        fn new() -> Self {
            let bins = (0..BIN_COUNT)
                .map(|_| Vec::with_capacity(MAX_BLOCKS_PER_BIN))
                .collect();
            Self {
                bins,
                retained_bytes: 0,
                #[cfg(test)]
                hits: 0,
                #[cfg(test)]
                misses: 0,
            }
        }
    }

    struct PoolInner {
        state: Mutex<PoolState>,
    }

    impl PoolInner {
        #[inline]
        fn lock(&self) -> MutexGuard<'_, PoolState> {
            match self.state.lock() {
                Ok(state) => state,
                Err(poisoned) => poisoned.into_inner(),
            }
        }
    }

    impl Drop for PoolInner {
        fn drop(&mut self) {
            let state = match self.state.get_mut() {
                Ok(state) => state,
                Err(poisoned) => poisoned.into_inner(),
            };
            for bin in &mut state.bins {
                for block in bin.drain(..) {
                    // SAFETY: cached blocks were allocated from `System` with this
                    // exact layout and were invalidated before entering the cache.
                    unsafe {
                        System.deallocate(block.pointer, block.layout);
                    }
                }
            }
        }
    }

    /// Cloneable allocator handle shared by one event loop and its runtimes.
    #[derive(Clone)]
    pub(crate) struct RuntimeAllocator {
        inner: Arc<PoolInner>,
    }

    impl RuntimeAllocator {
        pub(crate) fn new() -> Self {
            Self {
                inner: Arc::new(PoolInner {
                    state: Mutex::new(PoolState::new()),
                }),
            }
        }

        #[inline]
        fn cached_layout(layout: Layout) -> Option<(usize, Layout)> {
            if layout.size() == 0
                || layout.size() > MAX_CACHED_SIZE
                || layout.align() > MAX_CACHED_ALIGN
            {
                return None;
            }
            let size = layout.size().max(MIN_CACHED_SIZE).next_power_of_two();
            let size_index =
                size.trailing_zeros() as usize - MIN_CACHED_SIZE.trailing_zeros() as usize;
            let align_index = layout.align().trailing_zeros() as usize;
            let index = size_index * ALIGN_BIN_COUNT + align_index;
            // SAFETY: `size` is non-zero and `layout.align()` is a power of two.
            let actual = unsafe { Layout::from_size_align_unchecked(size, layout.align()) };
            Some((index, actual))
        }

        #[inline]
        fn actual_layout(layout: Layout) -> Layout {
            Self::cached_layout(layout).map_or(layout, |(_, actual)| actual)
        }

        #[cfg(test)]
        fn stats(&self) -> (usize, usize, usize) {
            let state = self.inner.lock();
            (state.hits, state.misses, state.retained_bytes)
        }
    }

    // SAFETY: live blocks remain owned by `System`; the mutex-protected cache
    // contains only invalidated blocks, preserves their exact normalized
    // layouts, and never touches them until a new allocation takes ownership.
    unsafe impl Allocator for RuntimeAllocator {
        fn allocate(&self, layout: Layout) -> Result<NonNull<[u8]>, AllocError> {
            let Some((index, actual)) = Self::cached_layout(layout) else {
                return System.allocate(layout);
            };

            {
                let mut state = self.inner.lock();
                if let Some(block) = state.bins[index].pop() {
                    state.retained_bytes = state.retained_bytes.saturating_sub(block.layout.size());
                    #[cfg(test)]
                    {
                        state.hits += 1;
                    }
                    // SAFETY: the cache owns this non-null `System` allocation.
                    return Ok(NonNull::slice_from_raw_parts(block.pointer, actual.size()));
                }
                #[cfg(test)]
                {
                    state.misses += 1;
                }
            }

            let block = System.allocate(actual)?;
            // SAFETY: a successful allocation always returns a non-null data pointer.
            let ptr = unsafe { NonNull::new_unchecked(block.as_ptr().cast::<u8>()) };
            Ok(NonNull::slice_from_raw_parts(ptr, actual.size()))
        }

        unsafe fn deallocate(&self, ptr: NonNull<u8>, layout: Layout) {
            let Some((index, actual)) = Self::cached_layout(layout) else {
                // SAFETY: upheld by this method's caller.
                unsafe { System.deallocate(ptr, layout) };
                return;
            };

            let mut state = self.inner.lock();
            if state.bins[index].len() < MAX_BLOCKS_PER_BIN
                && state.retained_bytes.saturating_add(actual.size()) <= MAX_RETAINED_BYTES
            {
                state.bins[index].push(CachedBlock {
                    pointer: ptr,
                    layout: actual,
                });
                state.retained_bytes += actual.size();
                return;
            }
            drop(state);
            // SAFETY: the block was allocated with the normalized layout above.
            unsafe { System.deallocate(ptr, actual) };
        }

        unsafe fn grow(
            &self,
            ptr: NonNull<u8>,
            old_layout: Layout,
            new_layout: Layout,
        ) -> Result<NonNull<[u8]>, AllocError> {
            let old_actual = Self::actual_layout(old_layout);
            let new_actual = Self::actual_layout(new_layout);
            // SAFETY: every block returned by this allocator is System-backed
            // with its normalized layout, and the caller upholds grow's rules.
            unsafe { System.grow(ptr, old_actual, new_actual) }
        }

        unsafe fn grow_zeroed(
            &self,
            ptr: NonNull<u8>,
            old_layout: Layout,
            new_layout: Layout,
        ) -> Result<NonNull<[u8]>, AllocError> {
            let old_actual = Self::actual_layout(old_layout);
            let new_actual = Self::actual_layout(new_layout);
            // SAFETY: every block returned by this allocator is System-backed
            // with its normalized layout, and the caller upholds grow's rules.
            unsafe { System.grow_zeroed(ptr, old_actual, new_actual) }
        }

        unsafe fn shrink(
            &self,
            ptr: NonNull<u8>,
            old_layout: Layout,
            new_layout: Layout,
        ) -> Result<NonNull<[u8]>, AllocError> {
            let old_actual = Self::actual_layout(old_layout);
            let new_actual = Self::actual_layout(new_layout);
            // SAFETY: every block returned by this allocator is System-backed
            // with its normalized layout, and the caller upholds shrink's rules.
            unsafe { System.shrink(ptr, old_actual, new_actual) }
        }
    }

    // SAFETY: clones share the same pool, so either clone can free an allocation
    // made by the other without invalidating allocations when a clone drops.
    unsafe impl AllocatorClone for RuntimeAllocator {}

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn cached_block_is_reused_and_bounded() {
            let allocator = RuntimeAllocator::new();
            let layout = Layout::from_size_align(24, 8).unwrap();
            let first = allocator.allocate(layout).unwrap();
            let first_ptr = first.as_ptr().cast::<u8>();
            // SAFETY: `first` was allocated above with `layout`.
            unsafe { allocator.deallocate(NonNull::new_unchecked(first_ptr), layout) };
            assert_eq!(allocator.stats().2, 32);

            let second = allocator.allocate(layout).unwrap();
            assert_eq!(second.as_ptr().cast::<u8>(), first_ptr);
            assert_eq!(allocator.stats(), (1, 1, 0));
            // SAFETY: `second` is live and uses the same layout.
            unsafe {
                allocator.deallocate(NonNull::new_unchecked(second.as_ptr().cast::<u8>()), layout);
            }
        }

        #[test]
        fn zeroed_allocation_clears_reused_memory() {
            let allocator = RuntimeAllocator::new();
            let layout = Layout::from_size_align(16, 8).unwrap();
            let block = allocator.allocate(layout).unwrap();
            let ptr = block.as_ptr().cast::<u8>();
            // SAFETY: the allocation contains at least 16 writable bytes.
            unsafe {
                ptr.write_bytes(0xA5, 16);
                allocator.deallocate(NonNull::new_unchecked(ptr), layout);
            }
            let zeroed = allocator.allocate_zeroed(layout).unwrap();
            let ptr = zeroed.as_ptr().cast::<u8>();
            // SAFETY: the returned allocation contains at least 16 initialized bytes.
            let bytes = unsafe { std::slice::from_raw_parts(ptr, 16) };
            assert!(bytes.iter().all(|byte| *byte == 0));
            // SAFETY: `zeroed` is live and uses `layout`.
            unsafe { allocator.deallocate(NonNull::new_unchecked(ptr), layout) };
        }

        #[test]
        fn large_allocations_bypass_the_cache() {
            let allocator = RuntimeAllocator::new();
            let layout = Layout::from_size_align(MAX_CACHED_SIZE + 1, 8).unwrap();
            let block = allocator.allocate(layout).unwrap();
            // SAFETY: `block` was allocated above with `layout`.
            unsafe {
                allocator.deallocate(NonNull::new_unchecked(block.as_ptr().cast::<u8>()), layout);
            }
            assert_eq!(allocator.stats(), (0, 0, 0));
        }

        #[test]
        fn clones_share_cached_blocks_across_threads() {
            let allocator = RuntimeAllocator::new();
            let other = allocator.clone();
            let layout = Layout::from_size_align(64, 16).unwrap();
            let address = std::thread::spawn(move || {
                let block = other.allocate(layout).unwrap();
                let address = block.as_ptr().cast::<u8>() as usize;
                // SAFETY: the block was allocated and is deallocated on this thread.
                unsafe {
                    other.deallocate(NonNull::new_unchecked(block.as_ptr().cast::<u8>()), layout);
                }
                address
            })
            .join()
            .unwrap();
            assert_eq!(allocator.stats().2, 64);
            let reused = allocator.allocate(layout).unwrap();
            assert_eq!(reused.as_ptr().cast::<u8>() as usize, address);
            // SAFETY: `reused` is live and was allocated with `layout`.
            unsafe {
                allocator.deallocate(NonNull::new_unchecked(reused.as_ptr().cast::<u8>()), layout);
            }
        }

        #[test]
        fn bin_limit_bounds_retained_memory() {
            let allocator = RuntimeAllocator::new();
            let layout = Layout::from_size_align(MAX_CACHED_SIZE, 8).unwrap();
            let blocks = (0..(MAX_BLOCKS_PER_BIN + 8))
                .map(|_| allocator.allocate(layout).unwrap())
                .collect::<Vec<_>>();
            for block in blocks {
                // SAFETY: every block is live and was allocated with `layout`.
                unsafe {
                    allocator
                        .deallocate(NonNull::new_unchecked(block.as_ptr().cast::<u8>()), layout);
                }
            }
            assert_eq!(allocator.stats().2, MAX_BLOCKS_PER_BIN * MAX_CACHED_SIZE);
        }

        #[test]
        fn grow_preserves_initialized_bytes() {
            let allocator = RuntimeAllocator::new();
            let old_layout = Layout::from_size_align(24, 8).unwrap();
            let new_layout = Layout::from_size_align(80, 8).unwrap();
            let block = allocator.allocate(old_layout).unwrap();
            let pointer = block.as_ptr().cast::<u8>();
            // SAFETY: the old allocation has 24 writable bytes and is live for grow.
            let grown = unsafe {
                for offset in 0..24 {
                    pointer.add(offset).write(offset as u8);
                }
                allocator
                    .grow(NonNull::new_unchecked(pointer), old_layout, new_layout)
                    .unwrap()
            };
            let pointer = grown.as_ptr().cast::<u8>();
            // SAFETY: grow preserves the first 24 initialized bytes.
            for offset in 0..24 {
                // SAFETY: grow preserves the first 24 initialized bytes.
                assert_eq!(unsafe { pointer.add(offset).read() }, offset as u8);
            }
            // SAFETY: `grown` is live and uses `new_layout`.
            unsafe {
                allocator.deallocate(NonNull::new_unchecked(pointer), new_layout);
            }
        }

        #[test]
        fn poisoned_metadata_lock_is_recovered() {
            let allocator = RuntimeAllocator::new();
            let poisoned = allocator.clone();
            let _ = std::panic::catch_unwind(move || {
                let _guard = poisoned.inner.lock();
                panic!("poison allocator metadata for the recovery test");
            });
            let layout = Layout::from_size_align(32, 8).unwrap();
            let block = allocator.allocate(layout).unwrap();
            // SAFETY: `block` is live and uses `layout`.
            unsafe {
                allocator.deallocate(NonNull::new_unchecked(block.as_ptr().cast::<u8>()), layout);
            }
            assert_eq!(allocator.stats().2, 32);
        }
    }
}
