//! Reusable-buffer TCP vectored polling benchmark, including Rust allocations.
use std::{
    alloc::{GlobalAlloc, Layout, System},
    io::{IoSlice, Read},
    net::{TcpListener, TcpStream},
    pin::Pin,
    sync::atomic::{AtomicUsize, Ordering},
    task::{Context, Poll, Waker},
    time::Instant,
};

use rsloop_vibeio_check::vibeio::{RuntimeBuilder, net::PollTcpStream};
use tokio::io::AsyncWrite;

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
    let rounds: usize = std::env::args()
        .nth(1)
        .unwrap_or_else(|| "200000".into())
        .parse()
        .unwrap();
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let sender = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
    sender.set_nodelay(true).unwrap();
    let (mut receiver, _) = listener.accept().unwrap();
    let runtime = RuntimeBuilder::new().rsloop_profile().build().unwrap();
    runtime.block_on(async move {
        let mut sender = PollTcpStream::from_std(sender).unwrap();
        let payload = [7u8; 32];
        let slices = [IoSlice::new(&payload); 16];
        let mut received = [0u8; 512];
        let mut cx = Context::from_waker(Waker::noop());
        for (measured, count) in [(false, 1000), (true, rounds)] {
            let before = ALLOCATIONS.load(Ordering::Relaxed);
            let start = Instant::now();
            for _ in 0..count {
                let Poll::Ready(Ok(written)) =
                    Pin::new(&mut sender).poll_write_vectored(&mut cx, &slices)
                else {
                    panic!("unexpected backpressure");
                };
                assert_eq!(written, received.len());
                receiver.read_exact(&mut received).unwrap();
                assert_eq!(received, [7; 512]);
            }
            let elapsed = start.elapsed().as_secs_f64();
            let allocations = ALLOCATIONS.load(Ordering::Relaxed) - before;
            if measured {
                println!(
                    "{{\"rounds\":{rounds},\"seconds\":{elapsed},\"allocations\":{allocations}}}"
                );
            }
        }
    });
}
