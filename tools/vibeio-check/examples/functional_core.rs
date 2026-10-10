//! Compare uninstrumented builds: cargo run --release --manifest-path
//! tools/vibeio-check/Cargo.toml --example functional_core -- 100000 7
use std::{
    future::Future,
    hint::black_box,
    pin::Pin,
    task::{Context, Waker},
    time::{Duration, Instant},
};

use rsloop_vibeio_check::vibeio::{
    DriverKind, RuntimeBuilder,
    time::{Interval, MissedTickBehavior},
};

// Exercise the actual private notification implementation, not a copy.
#[path = "../../../src/async_event.rs"]
mod async_event;

fn notifications(rounds: usize, listeners: usize) -> f64 {
    let event = async_event::AsyncEvent::new();
    let mut pending = Vec::with_capacity(listeners);
    let mut cx = Context::from_waker(Waker::noop());
    let started = Instant::now();
    for _ in 0..rounds {
        for _ in 0..listeners {
            let mut listener = event.listen();
            assert!(Pin::new(&mut listener).poll(&mut cx).is_pending());
            pending.push(listener);
        }
        event.notify_all();
        for mut listener in pending.drain(..) {
            assert_eq!(listener.try_recv().unwrap(), Some(()));
        }
    }
    started.elapsed().as_nanos() as f64 / rounds as f64
}

fn catch_up_ticks(rounds: usize) -> f64 {
    RuntimeBuilder::new()
        .driver(DriverKind::Mock)
        .enable_timer(true)
        .build()
        .unwrap()
        .block_on(async move {
            let mut interval = Interval::new(Duration::from_nanos(1));
            interval.set_missed_tick_behavior(MissedTickBehavior::CatchUp);
            let original = Instant::now() - Duration::from_secs(1);
            let started = Instant::now();
            for _ in 0..rounds {
                interval.next_deadline = Some(original);
                assert!(black_box(interval.tick().await) >= 1_000_000_000);
            }
            started.elapsed().as_nanos() as f64 / rounds as f64
        })
}

fn main() {
    let mut args = std::env::args().skip(1);
    let rounds: usize = args.next().map_or(100_000, |n| n.parse().unwrap());
    let samples: usize = args.next().map_or(7, |n| n.parse().unwrap());
    assert!(rounds > 0 && samples > 0);
    for listeners in [0, 1, 4, 16] {
        notifications(1000, listeners);
        for sample in 0..samples {
            let elapsed = notifications(rounds, listeners);
            println!("event-{listeners},{sample},{elapsed:.2}");
        }
    }
    catch_up_ticks(1000);
    for sample in 0..samples {
        println!("catch-up,{sample},{:.2}", catch_up_ticks(rounds));
    }
}
