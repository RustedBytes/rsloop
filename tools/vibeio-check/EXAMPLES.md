# Executable embedded-runtime examples

These examples import the unpublished harness, not a separately installed
`vibeio` package. They run against this repository's embedded source with:

```text
cargo test --manifest-path tools/vibeio-check/Cargo.toml --doc --locked
cargo test --manifest-path tools/vibeio-check/Cargo.toml --doc --all-features --locked
```

## Buffer length and capacity

Initialized length and writable capacity are different. An empty Vec can receive
data into its spare capacity, while a fixed array starts fully initialized.
Neither initialized length nor capacity replaces the byte count returned by I/O.

```rust
use rsloop_vibeio_check::vibeio::io::IoBuf;

let mut buffer = Vec::<u8>::with_capacity(32);
assert_eq!(buffer.buf_len(), 0);
assert!(buffer.buf_capacity() >= 32);
buffer.extend_from_slice(b"hello");
assert_eq!(buffer.buf_len(), 5);
let array = [0_u8; 8];
assert_eq!(array.buf_len(), 8);
assert_eq!(array.buf_capacity(), 8);
```

## Pipe buffer ownership

This example runs on Unix with the `pipe` feature (included in `--all-features`).
Other configurations compile an empty gated block. A read consumes its buffer
and returns it with the result; Vec capacity is available even when its length
starts at zero. Always use the returned buffer and check the reported count.

```rust
# #[cfg(all(unix, feature = "pipe"))]
# {
use rsloop_vibeio_check::vibeio::{DriverKind, RuntimeBuilder};
use rsloop_vibeio_check::vibeio::io::{self, AsyncRead, AsyncWrite};

let runtime = RuntimeBuilder::new().driver(DriverKind::Mio).build()?;
runtime.block_on(async {
    let (mut reader, mut writer) = io::pipe()?;
    // A tiny write into an empty pipe fits without backpressure.
    let (written, _) = writer.write(b"hello".to_vec()).await;
    assert_eq!(written?, 5);
    drop(writer);
    let (read, buffer) = reader.read(Vec::with_capacity(32)).await;
    let count = read?;
    assert_eq!(count, 5);
    assert_eq!(&buffer[..count], b"hello");
    let (read, _) = reader.read(buffer).await;
    assert_eq!(read?, 0);
    Ok::<(), std::io::Error>(())
})?;
# }
# Ok::<(), std::io::Error>(())
```

## Copy through EOF

This Unix/`pipe` example uses a small payload so the destination pipe can hold
the whole transfer before it is read. Larger transfers need a concurrent reader
to relieve backpressure. Closing the source writer is what allows copy to reach
EOF; leaving it open would keep copy waiting for more input.

```rust
# #[cfg(all(unix, feature = "pipe"))]
# {
use rsloop_vibeio_check::vibeio::{DriverKind, RuntimeBuilder};
use rsloop_vibeio_check::vibeio::io::{self, AsyncRead, AsyncWrite};

let runtime = RuntimeBuilder::new().driver(DriverKind::Mio).build()?;
runtime.block_on(async {
    let (mut source, mut producer) = io::pipe()?;
    let (mut consumer, mut destination) = io::pipe()?;
    let (written, _) = producer.write(b"copy me".to_vec()).await;
    assert_eq!(written?, 7);
    drop(producer);
    assert_eq!(io::copy(&mut source, &mut destination).await?, 7);
    drop(destination);
    let (read, buffer) = consumer.read(Vec::with_capacity(32)).await;
    assert_eq!(read?, 7);
    assert_eq!(&buffer, b"copy me");
    let (read, _) = consumer.read(buffer).await;
    assert_eq!(read?, 0);
    Ok::<(), std::io::Error>(())
})?;
# }
# Ok::<(), std::io::Error>(())
```

## Standard input echo

With the `stdio` feature, use `copy` to handle read counts, partial writes,
interruptions, and the final flush. Inside a runtime, stdio requires a configured
blocking pool. Outside a runtime it performs synchronous I/O when polled.
This example is compile-checked but not run: executing it would consume the
test runner's standard input and could wait indefinitely for EOF.

```no_run
# #[cfg(feature = "stdio")]
# {
use rsloop_vibeio_check::vibeio::io::{self, stdin, stdout};

async fn echo() -> std::io::Result<u64> {
    let mut input = stdin();
    let mut output = stdout();
    io::copy(&mut input, &mut output).await
}
# drop(echo());
# }
```

## Registering and cancelling a Ctrl-C wait

`ctrl_c()` registers a listener and returns a `Result`; apply `?` before
awaiting the returned future. Its completion is also fallible. This example
uses an immediate timeout to exercise cancellation without requiring a user
to send a console signal. It runs with the `signal` feature; other feature
configurations compile an empty block.

```rust
# #[cfg(feature = "signal")]
# {
use rsloop_vibeio_check::vibeio::{DriverKind, RuntimeBuilder, signal, time};
use std::time::Duration;

let runtime = RuntimeBuilder::new()
    .driver(DriverKind::Mock)
    .enable_timer(true)
    .build()?;
runtime.block_on(async {
    let listener = signal::ctrl_c()?;
    let wait = async move { listener.await };
    // No signal is sent: expiry drops the listener and its pending waker.
    assert!(time::timeout(Duration::ZERO, wait).await.is_err());
    Ok::<(), std::io::Error>(())
})?;
# }
# Ok::<(), std::io::Error>(())
```

## Sleep

Timers must be enabled explicitly when using the builder. The mock I/O driver
keeps this timer-only example independent of OS I/O availability.

```rust
use rsloop_vibeio_check::vibeio::{DriverKind, RuntimeBuilder, time};
use std::time::Duration;

let runtime = RuntimeBuilder::new()
    .driver(DriverKind::Mock)
    .enable_timer(true)
    .build()?;
runtime.block_on(async {
    time::sleep(Duration::from_millis(1)).await;
});
# Ok::<(), std::io::Error>(())
```

## Timeout and cancellation

```rust
use rsloop_vibeio_check::vibeio::{DriverKind, RuntimeBuilder, time};
use std::time::Duration;

let runtime = RuntimeBuilder::new()
    .driver(DriverKind::Mock)
    .enable_timer(true)
    .build()?;
runtime.block_on(async {
    assert_eq!(time::timeout(Duration::from_secs(1), async { 42 }).await, Ok(42));
    let pending = std::future::pending::<()>();
    assert_eq!(time::timeout(Duration::from_millis(1), pending).await, Err(time::TimeoutError));
});
# Ok::<(), std::io::Error>(())
```

## Interval

Use a finite loop in tests. A tick can represent multiple elapsed periods under
catch-up behavior, so consumers should not assume an exact count under load.

```rust
use rsloop_vibeio_check::vibeio::{DriverKind, RuntimeBuilder, time};
use std::time::Duration;

let runtime = RuntimeBuilder::new()
    .driver(DriverKind::Mock)
    .enable_timer(true)
    .build()?;
runtime.block_on(async {
    let mut interval = time::interval(Duration::from_millis(1));
    for _ in 0..2 {
        assert!(interval.tick().await >= 1);
    }
});
# Ok::<(), std::io::Error>(())
```
