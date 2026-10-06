//! Embedded scheduler turn timing and allocation counts, without Python.
use std::{
    alloc::{GlobalAlloc, Layout, System},
    hint::black_box,
    sync::atomic::{AtomicUsize, Ordering},
    task::Poll,
    time::Instant,
};

use rsloop_vibeio_check::{poll_once, vibeio::RuntimeBuilder};

struct CountingAllocator;
static ALLOCATIONS: AtomicUsize = AtomicUsize::new(0);
// SAFETY: every allocation operation delegates unchanged to System.
unsafe impl GlobalAlloc for CountingAllocator {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.alloc(layout) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, size: usize) -> *mut u8 {
        ALLOCATIONS.fetch_add(1, Ordering::Relaxed);
        unsafe { System.realloc(ptr, layout, size) }
    }
}
#[global_allocator]
static ALLOCATOR: CountingAllocator = CountingAllocator;

fn main() {
    let mut args = std::env::args().skip(1);
    let rounds: usize = args
        .next()
        .unwrap_or_else(|| "1000000".into())
        .parse()
        .unwrap();
    let tasks: usize = args.next().unwrap_or_else(|| "0".into()).parse().unwrap();
    let runtime = RuntimeBuilder::new().rsloop_profile().build().unwrap();
    let _tasks: Vec<_> = (0..tasks)
        .map(|_| {
            runtime.spawn(std::future::poll_fn(|cx| {
                cx.waker().wake_by_ref();
                Poll::<()>::Pending
            }))
        })
        .collect();
    for (measured, count) in [(false, 1000), (true, rounds)] {
        let before = ALLOCATIONS.load(Ordering::Relaxed);
        let start = Instant::now();
        for _ in 0..count {
            poll_once(black_box(&runtime));
        }
        let seconds = start.elapsed().as_secs_f64();
        let allocations = ALLOCATIONS.load(Ordering::Relaxed) - before;
        if measured {
            println!(
                "{{\"rounds\":{rounds},\"tasks\":{tasks},\"seconds\":{seconds},\"allocations\":{allocations}}}"
            );
        }
    }
}
