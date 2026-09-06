//! Wall-clock cost of repolling pending sleeps; includes setup and teardown.
#[path = "../src/vibeio/lib.rs"]
// Harness-free benches compile cfg(test) modules but omit their #[test]
// functions, leaving test-only imports unused. Normal test targets lint them.
#[allow(unused_imports)]
#[allow(
    dead_code,
    reason = "This benchmark includes only a subset of the embedded runtime API"
)]
mod vibeio;

use std::future::Future;
use std::hint::black_box;
use std::pin::Pin;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::task::{Context, Wake, Waker};
use std::time::{Duration, Instant};

struct CountWake(AtomicUsize);
impl Wake for CountWake {
    fn wake(self: Arc<Self>) {
        self.0.fetch_add(1, Ordering::Relaxed);
    }
}

fn repoll(timers: usize, rounds: usize, change_waker: bool) {
    let runtime = vibeio::RuntimeBuilder::new()
        .driver(vibeio::DriverKind::Mock)
        .enable_timer(true)
        .build()
        .unwrap();
    runtime.block_on(async move {
        let first = Waker::from(Arc::new(CountWake(AtomicUsize::new(0))));
        let second = Waker::from(Arc::new(CountWake(AtomicUsize::new(0))));
        let deadline = Instant::now() + Duration::from_secs(3600);
        let mut sleeps: Vec<_> = (0..timers)
            .map(|_| vibeio::time::Sleep::sleep_until(deadline))
            .collect();
        let mut cx = Context::from_waker(&first);
        for sleep in &mut sleeps {
            assert!(Pin::new(sleep).poll(&mut cx).is_pending());
        }
        for round in 0..rounds {
            let waker = if change_waker && round % 2 == 0 {
                &second
            } else {
                &first
            };
            let mut cx = Context::from_waker(waker);
            for sleep in &mut sleeps {
                assert!(black_box(Pin::new(sleep).poll(&mut cx)).is_pending());
            }
        }
    });
}

fn main() {
    println!("workload,sample,operations,seconds");
    for (name, timers, rounds, change_waker) in [
        ("single_same", 1, 1_000_000, false),
        ("single_changed", 1, 1_000_000, true),
        ("heap_same", 1024, 1000, false),
        ("heap_changed", 1024, 1000, true),
    ] {
        for sample in 0..10 {
            let start = Instant::now();
            repoll(
                black_box(timers),
                black_box(rounds),
                black_box(change_waker),
            );
            let seconds = start.elapsed().as_secs_f64();
            if sample >= 3 {
                println!("{name},{},{},{seconds:.9}", sample - 3, timers * rounds);
            }
        }
    }
}
