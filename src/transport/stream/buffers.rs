//! Owned buffers shared by stream reader and writer paths.

use std::ops::{Deref, DerefMut};
#[cfg(test)]
use std::sync::atomic::AtomicUsize;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;

use futures::future::poll_fn;
use futures::task::AtomicWaker;

use super::tuning::{
    MAX_STREAM_READ_BUFFER_SIZE, READ_BUFFER_POOL_LIMIT, WRITE_BUFFER_BLOCK_SIZE,
    WRITE_BUFFER_POOL_LIMIT,
};

pub(super) struct OwnedWriteBuffer {
    bytes: Vec<u8>,
    offset: usize,
    pool: Option<Arc<WriteBufferPool>>,
}

/// A socket-read allocation whose pool slot follows the bytes into the
/// consumer.  Native stream readers can retain this buffer directly instead
/// of copying it and still return the allocation to the bounded transport
/// pool when they replace or drop it.
pub(super) struct OwnedReadBuffer {
    bytes: Vec<u8>,
    pool: Option<Arc<ReadBufferPool>>,
}

impl OwnedReadBuffer {
    pub(super) fn with_capacity(capacity: usize) -> Self {
        Self {
            bytes: Vec::with_capacity(capacity),
            pool: None,
        }
    }

    pub(super) fn from_pooled(bytes: Vec<u8>, pool: &Arc<ReadBufferPool>) -> Self {
        Self {
            bytes,
            pool: Some(Arc::clone(pool)),
        }
    }

    #[cfg(test)]
    pub(super) fn from_vec(bytes: Vec<u8>) -> Self {
        Self { bytes, pool: None }
    }
}

impl Deref for OwnedReadBuffer {
    type Target = Vec<u8>;

    fn deref(&self) -> &Self::Target {
        &self.bytes
    }
}

impl DerefMut for OwnedReadBuffer {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.bytes
    }
}

impl Drop for OwnedReadBuffer {
    fn drop(&mut self) {
        if let Some(pool) = self.pool.take() {
            pool.release(std::mem::take(&mut self.bytes));
        }
    }
}

pub(super) struct PendingReadBuffer<'a> {
    bytes: Vec<u8>,
    home: &'a Mutex<Vec<u8>>,
    pool: Option<&'a ReadBufferPool>,
}

impl<'a> PendingReadBuffer<'a> {
    pub(super) fn from_pooled(
        bytes: Vec<u8>,
        pool: &'a ReadBufferPool,
        home: &'a Mutex<Vec<u8>>,
    ) -> Self {
        Self {
            bytes,
            home,
            pool: Some(pool),
        }
    }

    #[inline]
    pub(super) fn len(&self) -> usize {
        self.bytes.len()
    }

    #[inline]
    pub(super) fn as_slice(&self) -> &[u8] {
        &self.bytes
    }

    pub(super) fn extend(&mut self, data: &[u8]) {
        // Most drains contain one read. Keep that allocation until delivery;
        // only acquire and copy into the coalescing buffer for a second chunk.
        if let Some(pool) = self.pool.take() {
            let mut joined =
                std::mem::take(&mut *self.home.lock().expect("poisoned read coalesce buffer"));
            joined.clear();
            joined.extend_from_slice(&self.bytes);
            pool.release(std::mem::replace(&mut self.bytes, joined));
        }
        self.bytes.extend_from_slice(data);
    }
}

impl Drop for PendingReadBuffer<'_> {
    fn drop(&mut self) {
        if let Some(pool) = self.pool.take() {
            pool.release(std::mem::take(&mut self.bytes));
            return;
        }
        self.bytes.clear();
        // This is only an allocation cache. Do not wait or panic during drop
        // if another consumer holds it or a previous consumer poisoned it.
        if let Ok(mut home) = self.home.try_lock() {
            *home = std::mem::take(&mut self.bytes);
        }
    }
}

impl OwnedWriteBuffer {
    #[inline]
    #[cfg(test)]
    pub(super) fn from_slice(data: &[u8]) -> Self {
        Self {
            bytes: data.to_vec(),
            offset: 0,
            pool: None,
        }
    }

    pub(super) fn from_pooled_slice(data: &[u8], pool: &Arc<WriteBufferPool>) -> Self {
        let (mut bytes, pooled) = pool.acquire(data.len());
        bytes.extend_from_slice(data);
        Self {
            bytes,
            offset: 0,
            pool: pooled.then(|| Arc::clone(pool)),
        }
    }

    pub(super) fn with_pooled_capacity(capacity: usize, pool: &Arc<WriteBufferPool>) -> Self {
        let (bytes, pooled) = pool.acquire(capacity);
        Self {
            bytes,
            offset: 0,
            pool: pooled.then(|| Arc::clone(pool)),
        }
    }

    pub(super) fn extend_from_slice(&mut self, data: &[u8]) {
        self.bytes.extend_from_slice(data);
    }

    pub(super) fn try_append(&mut self, data: &[u8]) -> bool {
        if !can_append(self.offset, self.bytes.len(), data.len()) {
            return false;
        }
        self.bytes.extend_from_slice(data);
        true
    }

    #[inline]
    #[cfg(any(test, kani))]
    pub(super) fn from_vec(data: Vec<u8>) -> Self {
        Self {
            bytes: data,
            offset: 0,
            pool: None,
        }
    }

    #[inline]
    pub(super) fn remaining(&self) -> &[u8] {
        &self.bytes[self.offset..]
    }

    #[inline]
    pub(super) fn len(&self) -> usize {
        self.remaining().len()
    }

    #[inline]
    pub(super) fn advance(&mut self, written: usize) {
        self.offset += written;
    }

    #[inline]
    pub(super) fn is_empty(&self) -> bool {
        self.offset == self.bytes.len()
    }
}

#[inline]
fn can_append(offset: usize, current_len: usize, incoming_len: usize) -> bool {
    offset == 0
        && current_len <= super::tuning::DEFAULT_WRITE_BUFFER_HIGH_WATER
        && incoming_len
            <= super::tuning::DEFAULT_WRITE_BUFFER_HIGH_WATER.saturating_sub(current_len)
}

#[inline]
fn retain_write_buffer(capacity: usize) -> bool {
    capacity <= super::tuning::DEFAULT_WRITE_BUFFER_HIGH_WATER
}

#[inline]
fn retain_read_buffer(capacity: usize) -> bool {
    capacity <= MAX_STREAM_READ_BUFFER_SIZE
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PoolAcquire {
    Reuse,
    Allocate,
    Fallback,
    Unavailable,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum PoolRelease {
    Store,
    Discard,
}

fn pool_acquire(
    available: usize,
    allocated: usize,
    limit: usize,
    allow_fallback: bool,
) -> PoolAcquire {
    if available > 0 {
        PoolAcquire::Reuse
    } else if allocated < limit {
        PoolAcquire::Allocate
    } else if allow_fallback {
        PoolAcquire::Fallback
    } else {
        PoolAcquire::Unavailable
    }
}

fn pool_release(retain: bool, available: usize, limit: usize) -> PoolRelease {
    if retain && available < limit {
        PoolRelease::Store
    } else {
        PoolRelease::Discard
    }
}

impl Drop for OwnedWriteBuffer {
    fn drop(&mut self) {
        if let Some(pool) = self.pool.take() {
            pool.release(std::mem::take(&mut self.bytes));
        }
    }
}

/// Lazily populated storage for writes that cannot complete directly.
pub(super) struct WriteBufferPool {
    state: Mutex<WriteBufferPoolState>,
    #[cfg(test)]
    allocations: AtomicUsize,
    #[cfg(test)]
    fallbacks: AtomicUsize,
}

struct WriteBufferPoolState {
    buffers: Vec<Vec<u8>>,
    allocated: usize,
}

impl WriteBufferPoolState {
    fn new() -> Self {
        Self {
            buffers: Vec::with_capacity(WRITE_BUFFER_POOL_LIMIT),
            allocated: 0,
        }
    }

    fn acquire(&mut self, capacity: usize) -> (Vec<u8>, bool, usize, bool) {
        let (mut buffer, pooled, mut allocation_events, fallback) = match pool_acquire(
            self.buffers.len(),
            self.allocated,
            WRITE_BUFFER_POOL_LIMIT,
            true,
        ) {
            PoolAcquire::Reuse => (
                self.buffers.pop().expect("available buffer"),
                true,
                0,
                false,
            ),
            PoolAcquire::Allocate => {
                self.allocated += 1;
                (
                    Vec::with_capacity(capacity.max(WRITE_BUFFER_BLOCK_SIZE)),
                    true,
                    1,
                    false,
                )
            }
            PoolAcquire::Fallback => (Vec::with_capacity(capacity), false, 0, true),
            PoolAcquire::Unavailable => unreachable!("write pools permit fallback allocations"),
        };
        buffer.clear();
        if buffer.capacity() < capacity {
            allocation_events += 1;
            buffer.reserve(capacity);
        }
        (buffer, pooled, allocation_events, fallback)
    }

    fn release(&mut self, mut buffer: Vec<u8>) {
        match pool_release(
            retain_write_buffer(buffer.capacity()),
            self.buffers.len(),
            WRITE_BUFFER_POOL_LIMIT,
        ) {
            PoolRelease::Store => {
                buffer.clear();
                self.buffers.push(buffer);
            }
            PoolRelease::Discard => {
                self.allocated = self.allocated.saturating_sub(1);
            }
        }
    }
}

impl WriteBufferPool {
    pub(super) fn new() -> Self {
        Self {
            state: Mutex::new(WriteBufferPoolState::new()),
            #[cfg(test)]
            allocations: AtomicUsize::new(0),
            #[cfg(test)]
            fallbacks: AtomicUsize::new(0),
        }
    }

    fn acquire(&self, capacity: usize) -> (Vec<u8>, bool) {
        let mut state = self.state.lock().expect("poisoned write buffer pool");
        let (buffer, pooled, allocation_events, fallback) = state.acquire(capacity);
        drop(state);
        #[cfg(test)]
        self.allocations
            .fetch_add(allocation_events, Ordering::Relaxed);
        #[cfg(test)]
        self.fallbacks
            .fetch_add(usize::from(fallback), Ordering::Relaxed);
        #[cfg(not(test))]
        let _ = (allocation_events, fallback);
        (buffer, pooled)
    }

    pub(super) fn release(&self, buffer: Vec<u8>) {
        let mut state = self.state.lock().expect("poisoned write buffer pool");
        state.release(buffer);
    }
}

/// Small transport-local pool shared by the runtime reader and Python-loop
/// delivery path. Framed protocols repeatedly exchange similarly sized
/// buffers; recycling avoids allocating a fresh Vec after every owned handoff.
pub(super) struct ReadBufferPool {
    state: Mutex<ReadBufferPoolState>,
    available: Condvar,
    async_available: AtomicWaker,
    closed: AtomicBool,
    #[cfg(test)]
    allocations: AtomicUsize,
}

struct ReadBufferPoolState {
    buffers: Vec<Vec<u8>>,
    allocated: usize,
}

impl ReadBufferPoolState {
    fn new() -> Self {
        Self {
            buffers: Vec::with_capacity(READ_BUFFER_POOL_LIMIT),
            allocated: 0,
        }
    }

    fn try_acquire(&mut self, capacity: usize) -> (Option<Vec<u8>>, usize) {
        let (mut buffer, mut allocation_events) = match pool_acquire(
            self.buffers.len(),
            self.allocated,
            READ_BUFFER_POOL_LIMIT,
            false,
        ) {
            PoolAcquire::Reuse => (self.buffers.pop().expect("available buffer"), 0),
            PoolAcquire::Allocate => {
                self.allocated += 1;
                (Vec::with_capacity(capacity), 1)
            }
            PoolAcquire::Unavailable => return (None, 0),
            PoolAcquire::Fallback => unreachable!("read pools do not permit fallback allocations"),
        };
        buffer.clear();
        if buffer.capacity() < capacity {
            allocation_events += 1;
            buffer.reserve(capacity);
        }
        (Some(buffer), allocation_events)
    }

    fn release(&mut self, mut buffer: Vec<u8>) {
        match pool_release(
            retain_read_buffer(buffer.capacity()),
            self.buffers.len(),
            READ_BUFFER_POOL_LIMIT,
        ) {
            PoolRelease::Store => {
                buffer.clear();
                self.buffers.push(buffer);
            }
            PoolRelease::Discard => {
                self.allocated = self.allocated.saturating_sub(1);
            }
        }
    }
}

impl ReadBufferPool {
    pub(super) fn new() -> Self {
        Self {
            state: Mutex::new(ReadBufferPoolState::new()),
            available: Condvar::new(),
            async_available: AtomicWaker::new(),
            closed: AtomicBool::new(false),
            #[cfg(test)]
            allocations: AtomicUsize::new(0),
        }
    }

    pub(super) fn try_acquire(&self, capacity: usize) -> Option<Vec<u8>> {
        let mut state = self.state.lock().expect("poisoned stream read buffer pool");
        let (buffer, allocation_events) = state.try_acquire(capacity);
        drop(state);
        #[cfg(test)]
        self.allocations
            .fetch_add(allocation_events, Ordering::Relaxed);
        #[cfg(not(test))]
        let _ = allocation_events;
        buffer
    }

    fn has_available(&self) -> bool {
        let state = self.state.lock().expect("poisoned stream read buffer pool");
        !state.buffers.is_empty() || state.allocated < READ_BUFFER_POOL_LIMIT
    }

    pub(super) async fn wait_async(&self) {
        poll_fn(|context| {
            if self.closed.load(Ordering::Acquire) || self.has_available() {
                return std::task::Poll::Ready(());
            }
            self.async_available.register(context.waker());
            if self.closed.load(Ordering::Acquire) || self.has_available() {
                std::task::Poll::Ready(())
            } else {
                std::task::Poll::Pending
            }
        })
        .await;
    }

    pub(super) fn wait_timeout(&self, timeout: Duration) {
        let state = self.state.lock().expect("poisoned stream read buffer pool");
        if state.buffers.is_empty() && state.allocated >= READ_BUFFER_POOL_LIMIT {
            let _ = self
                .available
                .wait_timeout(state, timeout)
                .expect("poisoned stream read buffer pool");
        }
    }

    pub(super) fn notify_all(&self) {
        self.available.notify_all();
        self.async_available.wake();
    }

    pub(super) fn close(&self) {
        self.closed.store(true, Ordering::Release);
        self.notify_all();
    }

    pub(super) fn release(&self, buffer: Vec<u8>) {
        let mut state = self.state.lock().expect("poisoned stream read buffer pool");
        state.release(buffer);
        drop(state);
        self.notify_all();
    }
}

#[cfg(kani)]
mod verification {
    use super::{
        MAX_STREAM_READ_BUFFER_SIZE, OwnedWriteBuffer, PoolAcquire, PoolRelease,
        READ_BUFFER_POOL_LIMIT, WRITE_BUFFER_POOL_LIMIT, can_append, pool_acquire, pool_release,
        retain_read_buffer, retain_write_buffer,
    };
    use crate::verification::{MAX_BYTES, MAX_POOL_OPERATIONS, any_bytes};

    #[kani::proof]
    #[kani::unwind(12)]
    fn merge_owned_write_buffer_preserves_remaining_bytes() {
        let data = any_bytes::<MAX_BYTES>();
        let first: usize = kani::any();
        kani::assume(first <= data.len());
        let mut buffer = OwnedWriteBuffer::from_vec(data.clone());

        buffer.advance(first);
        assert_eq!(buffer.remaining(), &data[first..]);
        assert_eq!(buffer.len(), data.len() - first);
        assert_eq!(buffer.is_empty(), first == data.len());

        let second: usize = kani::any();
        kani::assume(second <= buffer.len());
        buffer.advance(second);
        assert_eq!(buffer.remaining(), &data[first + second..]);
        assert_eq!(buffer.is_empty(), first + second == data.len());
    }

    #[kani::proof]
    #[kani::unwind(12)]
    fn merge_owned_write_buffer_append_preserves_or_rejects_content_atomically() {
        const APPEND_BYTES: usize = 4;
        let initial = any_bytes::<APPEND_BYTES>();
        let incoming = any_bytes::<APPEND_BYTES>();
        let offset: usize = kani::any();
        kani::assume(offset <= initial.len());
        let mut buffer = OwnedWriteBuffer::from_vec(initial.clone());
        buffer.advance(offset);

        let accepted = buffer.try_append(&incoming);
        assert_eq!(accepted, offset == 0);
        if accepted {
            let mut expected = initial;
            expected.extend_from_slice(&incoming);
            assert_eq!(buffer.remaining(), expected.as_slice());
        } else {
            assert_eq!(buffer.remaining(), &initial[offset..]);
        }
    }

    #[kani::proof]
    fn merge_append_and_retention_decisions_are_overflow_free() {
        let offset: usize = kani::any();
        let current_len: usize = kani::any();
        let incoming_len: usize = kani::any();
        let high_water = super::super::tuning::DEFAULT_WRITE_BUFFER_HIGH_WATER;

        let expected =
            offset == 0 && current_len <= high_water && incoming_len <= high_water - current_len;
        assert_eq!(can_append(offset, current_len, incoming_len), expected);

        let capacity: usize = kani::any();
        assert_eq!(
            retain_read_buffer(capacity),
            capacity <= MAX_STREAM_READ_BUFFER_SIZE
        );
        assert_eq!(retain_write_buffer(capacity), capacity <= high_water);
    }

    fn verify_pool_accounting(allow_fallback: bool, limit: usize) {
        let acquire: [bool; MAX_POOL_OPERATIONS] = kani::any();
        let retain: [bool; MAX_POOL_OPERATIONS] = kani::any();
        let mut available = 0;
        let mut allocated = 0;
        let mut held = 0;

        for index in 0..MAX_POOL_OPERATIONS {
            if acquire[index] {
                match pool_acquire(available, allocated, limit, allow_fallback) {
                    PoolAcquire::Reuse => {
                        available -= 1;
                        held += 1;
                    }
                    PoolAcquire::Allocate => {
                        allocated += 1;
                        held += 1;
                    }
                    PoolAcquire::Fallback | PoolAcquire::Unavailable => {}
                }
            } else if held > 0 {
                held -= 1;
                match pool_release(retain[index], available, limit) {
                    PoolRelease::Store => available += 1,
                    PoolRelease::Discard => allocated -= 1,
                }
            }

            assert!(available <= allocated);
            assert!(allocated <= limit);
            assert_eq!(available + held, allocated);
        }
    }

    #[kani::proof]
    #[kani::unwind(8)]
    fn extended_read_pool_preserves_sequential_slot_accounting() {
        verify_pool_accounting(false, READ_BUFFER_POOL_LIMIT);
    }

    #[kani::proof]
    #[kani::unwind(8)]
    fn extended_write_pool_preserves_sequential_slot_accounting() {
        verify_pool_accounting(true, WRITE_BUFFER_POOL_LIMIT);
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;
    use std::sync::{Arc, Mutex};

    use super::{
        MAX_STREAM_READ_BUFFER_SIZE, OwnedReadBuffer, OwnedWriteBuffer, PendingReadBuffer,
        READ_BUFFER_POOL_LIMIT, ReadBufferPool, WRITE_BUFFER_POOL_LIMIT, WriteBufferPool,
    };

    #[test]
    fn owned_write_buffer_tracks_remaining_bytes() {
        let mut buffer = OwnedWriteBuffer::from_slice(b"abcdef");
        buffer.advance(2);
        assert_eq!(buffer.remaining(), b"cdef");
        assert!(!buffer.is_empty());
        buffer.advance(4);
        assert!(buffer.is_empty());
    }

    #[test]
    fn owned_write_buffer_preserves_invariants_across_all_split_points() {
        for len in 0..=128 {
            let data = (0_u8..=127).take(len).collect::<Vec<_>>();
            for split in 0..=len {
                let mut buffer = OwnedWriteBuffer::from_vec(data.clone());
                buffer.advance(split);
                assert_eq!(buffer.remaining(), &data[split..]);
                assert_eq!(buffer.is_empty(), split == len);

                buffer.advance(len - split);
                assert!(buffer.remaining().is_empty());
                assert!(buffer.is_empty());
            }
        }
    }

    #[test]
    fn pending_read_buffer_coalesces_and_returns_storage_home() {
        let home = Mutex::new(vec![1, 2]);
        let pool = ReadBufferPool::new();
        let mut input = pool.try_acquire(16).unwrap();
        input.extend_from_slice(&[3, 4]);
        let pointer = input.as_ptr();
        let mut pending = PendingReadBuffer::from_pooled(input, &pool, &home);

        pending.extend(&[5]);

        assert_eq!(pending.len(), 3);
        assert_eq!(pending.as_slice(), &[3, 4, 5]);
        let recycled = pool.try_acquire(16).unwrap();
        assert_eq!(recycled.as_ptr(), pointer);
        pool.release(recycled);
        drop(pending);
        assert!(home.lock().expect("coalesce buffer").capacity() >= 3);
    }

    #[test]
    fn pending_coalesced_read_drop_does_not_wait_for_cache() {
        let home = Mutex::new(Vec::new());
        let pending = PendingReadBuffer {
            bytes: vec![1, 2, 3],
            home: &home,
            pool: None,
        };
        std::thread::scope(|scope| {
            let guard = home.lock().unwrap();
            let (done, receiver) = std::sync::mpsc::channel();
            let worker = scope.spawn(move || {
                drop(pending);
                done.send(()).unwrap();
            });
            let result = receiver.recv_timeout(std::time::Duration::from_secs(2));
            // Release the lock before joining even if the regression occurs.
            assert!(guard.is_empty());
            drop(guard);
            worker.join().unwrap();
            assert!(result.is_ok(), "drop waited for the coalescing cache");
        });
    }

    #[test]
    fn pending_coalesced_read_drop_tolerates_poisoned_cache() {
        let home = Mutex::new(Vec::new());
        let _ = std::panic::catch_unwind(|| {
            let _guard = home.lock().unwrap();
            panic!("poison cache");
        });
        assert!(home.is_poisoned());
        drop(PendingReadBuffer {
            bytes: vec![1, 2, 3],
            home: &home,
            pool: None,
        });
    }

    #[test]
    fn pending_single_read_keeps_allocation_and_returns_it_to_pool() {
        let home = Mutex::new(Vec::new());
        let pool = ReadBufferPool::new();
        let mut input = pool.try_acquire(16).unwrap();
        input.extend_from_slice(b"frame");
        let pointer = input.as_ptr();
        let pending = PendingReadBuffer::from_pooled(input, &pool, &home);
        assert_eq!(pending.as_slice().as_ptr(), pointer);
        assert_eq!(pending.as_slice(), b"frame");
        drop(pending);
        let recycled = pool.try_acquire(16).unwrap();
        assert_eq!(recycled.as_ptr(), pointer);
        assert!(recycled.is_empty());
        assert_eq!(home.lock().unwrap().capacity(), 0);
        pool.release(recycled);
    }

    #[test]
    fn read_buffer_pool_clears_and_reuses_released_buffers() {
        let pool = ReadBufferPool::new();
        let mut buffer = pool.try_acquire(128).expect("initial buffer");
        buffer.extend_from_slice(b"stale data");
        let pointer = buffer.as_ptr();
        pool.release(buffer);

        let acquired = pool.try_acquire(64).expect("recycled buffer");

        assert!(acquired.is_empty());
        assert_eq!(acquired.as_ptr(), pointer);
        assert!(acquired.capacity() >= 128);
        pool.release(acquired);
        let allocation_count = pool.allocations.load(Ordering::Relaxed);
        let grown = pool.try_acquire(4096).expect("grown buffer");
        pool.release(grown);
        assert!(pool.allocations.load(Ordering::Relaxed) > allocation_count);
        let warmed_count = pool.allocations.load(Ordering::Relaxed);
        let warmed = pool.try_acquire(4096).expect("warmed buffer");
        pool.release(warmed);
        assert_eq!(pool.allocations.load(Ordering::Relaxed), warmed_count);
    }

    #[test]
    fn read_buffer_pool_enforces_count_and_capacity_limits() {
        let pool = ReadBufferPool::new();
        let mut held = Vec::new();
        for capacity in 1..=READ_BUFFER_POOL_LIMIT {
            held.push(pool.try_acquire(capacity * 32).expect("pool slot"));
        }
        for buffer in held {
            pool.release(buffer);
        }
        assert_eq!(
            pool.state.lock().expect("read buffer pool").buffers.len(),
            READ_BUFFER_POOL_LIMIT
        );

        let mut oversized = pool
            .try_acquire(MAX_STREAM_READ_BUFFER_SIZE)
            .expect("oversized pool slot");
        oversized.resize(MAX_STREAM_READ_BUFFER_SIZE, 0);
        oversized.reserve(1);
        pool.release(oversized);
        let state = pool.state.lock().expect("read buffer pool");
        assert_eq!(state.buffers.len(), READ_BUFFER_POOL_LIMIT - 1);
        assert_eq!(state.allocated, READ_BUFFER_POOL_LIMIT - 1);
    }

    #[test]
    fn read_buffer_pool_stops_allocating_at_the_slot_limit() {
        let pool = ReadBufferPool::new();
        let held = (0..READ_BUFFER_POOL_LIMIT)
            .map(|_| pool.try_acquire(64).expect("pool slot"))
            .collect::<Vec<_>>();

        assert!(pool.try_acquire(64).is_none());
        pool.release(held.into_iter().next().expect("held buffer"));
        assert!(pool.try_acquire(64).is_some());
        assert_eq!(
            pool.allocations.load(Ordering::Relaxed),
            READ_BUFFER_POOL_LIMIT
        );
    }

    #[test]
    fn read_buffer_release_makes_a_slot_available() {
        let pool = ReadBufferPool::new();
        let held = (0..READ_BUFFER_POOL_LIMIT)
            .map(|_| pool.try_acquire(64).expect("pool slot"))
            .collect::<Vec<_>>();
        assert!(!pool.has_available());

        pool.release(held.into_iter().next().expect("held buffer"));

        assert!(pool.has_available());
    }

    #[test]
    fn owned_read_buffer_returns_its_allocation_to_the_pool() {
        let pool = Arc::new(ReadBufferPool::new());
        let mut bytes = pool.try_acquire(128).expect("pool slot");
        bytes.extend_from_slice(b"framed message");
        let pointer = bytes.as_ptr();

        let owned = OwnedReadBuffer::from_pooled(bytes, &pool);
        assert_eq!(owned.as_slice(), b"framed message");
        drop(owned);

        let reused = pool.try_acquire(64).expect("returned pool slot");
        assert_eq!(reused.as_ptr(), pointer);
        pool.release(reused);
    }

    #[test]
    fn write_buffer_pool_reuses_storage_and_bounds_normal_allocations() {
        let pool = Arc::new(WriteBufferPool::new());
        let first = OwnedWriteBuffer::from_pooled_slice(b"first", &pool);
        let pointer = first.bytes.as_ptr();
        drop(first);

        let reused = OwnedWriteBuffer::from_pooled_slice(b"second", &pool);
        assert_eq!(reused.bytes.as_ptr(), pointer);
        assert_eq!(pool.allocations.load(Ordering::Relaxed), 1);
        drop(reused);

        let held = (0..WRITE_BUFFER_POOL_LIMIT)
            .map(|_| OwnedWriteBuffer::from_pooled_slice(b"held", &pool))
            .collect::<Vec<_>>();
        let fallback = OwnedWriteBuffer::from_pooled_slice(b"fallback", &pool);
        assert!(fallback.pool.is_none());
        assert_eq!(pool.fallbacks.load(Ordering::Relaxed), 1);
        drop(held);
    }
}
