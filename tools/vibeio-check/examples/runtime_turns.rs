//! Scheduler entry/exit benchmark, without Python or socket traffic noise.
//! Run the same source against both revisions, in alternating fresh processes.

use rsloop_vibeio_check::vibeio::RuntimeBuilder;
use std::hint::black_box;
use std::task::Poll;
use std::time::Instant;

fn main() {
    let mut args = std::env::args().skip(1);
    let iterations: usize = args
        .next()
        .unwrap_or_else(|| "3000000".into())
        .parse()
        .expect("positive iteration count");
    let tasks: usize = args
        .next()
        .unwrap_or_else(|| "0".into())
        .parse()
        .expect("number of ready tasks");
    assert!(iterations > 0);
    let runtime = RuntimeBuilder::new().rsloop_profile().build().unwrap();
    let _tasks: Vec<_> = (0..tasks)
        .map(|_| {
            runtime.spawn(std::future::poll_fn(|cx| {
                cx.waker().wake_by_ref();
                Poll::<()>::Pending
            }))
        })
        .collect();
    for (measured, count) in [(false, 10_000), (true, iterations)] {
        let started = Instant::now();
        for _ in 0..count {
            let mut polled = false;
            black_box(&runtime).block_on(std::future::poll_fn(move |cx| {
                if polled {
                    Poll::Ready(())
                } else {
                    polled = true;
                    cx.waker().wake_by_ref();
                    Poll::Pending
                }
            }));
        }
        if measured {
            println!(
                "{{\"iterations\":{count},\"tasks\":{tasks},\"seconds\":{}}}",
                started.elapsed().as_secs_f64()
            );
        }
    }
}
