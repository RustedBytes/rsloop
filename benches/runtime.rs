//! Benchmarks the embedded runtime without exposing it in rsloop's public API.
#[path = "../src/vibeio/lib.rs"]
mod vibeio;

use std::future::poll_fn;
use std::hint::black_box;
use std::task::Poll;
use std::time::Instant;

fn dispatch(tasks: usize, yields: usize) {
    let runtime = vibeio::RuntimeBuilder::new()
        .rsloop_profile()
        .enable_timer(true)
        .build()
        .unwrap();
    runtime.block_on(async move {
        let handles: Vec<_> = (0..tasks)
            .map(|_| {
                vibeio::spawn(async move {
                    let mut remaining = yields;
                    poll_fn(move |cx| {
                        if remaining == 0 {
                            Poll::Ready(())
                        } else {
                            remaining -= 1;
                            cx.waker().wake_by_ref();
                            Poll::Pending
                        }
                    })
                    .await;
                })
            })
            .collect();
        for handle in handles {
            handle.await;
        }
    });
}

fn main() {
    println!("workload,sample,operations,seconds");
    for (name, tasks, yields) in [
        ("spawn_join", 100_000, 0),
        ("single_task_yield", 1, 1_000_000),
        ("batch_yield", 256, 4_000),
    ] {
        for sample in 0..10 {
            let start = Instant::now();
            dispatch(black_box(tasks), black_box(yields));
            let elapsed = start.elapsed().as_secs_f64();
            if sample >= 3 {
                println!(
                    "{name},{},{},{elapsed:.9}",
                    sample - 3,
                    tasks * (yields + 1)
                );
            }
        }
    }
}
