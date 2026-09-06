# Executable embedded-runtime examples

These examples import the unpublished harness, not a separately installed
`vibeio` package. They run against this repository's embedded source with:

```text
cargo test --manifest-path tools/vibeio-check/Cargo.toml --doc --locked
cargo test --manifest-path tools/vibeio-check/Cargo.toml --doc --all-features --locked
```

## Spawning and joining tasks

The builder returns a Result. Free-standing `spawn` requires an entered runtime;
`block_on` enters it while driving the future. Cancel consumes a handle, whereas
dropping a handle alone does not cancel its task.

```rust
use rsloop_vibeio_check::vibeio::{DriverKind, RuntimeBuilder, spawn};

let runtime = RuntimeBuilder::new().driver(DriverKind::Mock).build()?;
let value = runtime.block_on(async {
    let cancelled = spawn(async { panic!("cancelled before the first poll") });
    cancelled.cancel();
    let handle = spawn(async { 42 });
    handle.await + 10
});
assert_eq!(value, 52);
# Ok::<(), std::io::Error>(())
```

## Blocking work with an explicit pool

This runs with `blocking-default`; other configurations compile an empty gated
block. Unlike `spawn`, blocking tasks return a fallible result and need a pool.
Dropping a pending blocking future does not stop work already running.

```rust
# #[cfg(feature = "blocking-default")]
# {
use rsloop_vibeio_check::vibeio::{DriverKind, RuntimeBuilder, spawn_blocking};

let runtime = RuntimeBuilder::new()
    .driver(DriverKind::Mock)
    .default_blocking_pool(1)
    .build()?;
runtime.block_on(async {
    let sum = spawn_blocking(|| (1..=10).sum::<u64>()).await?;
    assert_eq!(sum, 55);
    Ok::<(), Box<dyn std::error::Error>>(())
})?;
# }
# Ok::<(), Box<dyn std::error::Error>>(())
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

## Filesystem offload

This example runs with `fs`, independently of `blocking-default`. The small
demonstration pool starts one thread per operation; applications can provide a
bounded pool instead. The mock driver deliberately exercises filesystem offload,
not completion-based I/O. Only the newly created scratch directory is modified.
The symlink portion runs only on Unix, avoiding Windows symlink privilege
requirements; the regular-file and directory checks run on all platforms.

```rust
# #[cfg(feature = "fs")]
# {
use rsloop_vibeio_check::vibeio::{DriverKind, RuntimeBuilder, fs};
use rsloop_vibeio_check::vibeio::blocking::BlockingThreadPool;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

struct DemoPool;
impl BlockingThreadPool for DemoPool {
    fn spawn(&self, task: Box<dyn FnOnce() + Send>) {
        std::thread::spawn(task);
    }
}
struct Scratch(PathBuf);
impl Drop for Scratch {
    fn drop(&mut self) {
        // Each target belongs to this example; never remove recursively.
        let _ = std::fs::remove_file(self.0.join("hello.txt"));
        let _ = std::fs::remove_file(self.0.join("created.txt"));
        let _ = std::fs::remove_file(self.0.join("hello-link"));
        let _ = std::fs::remove_dir(self.0.join("nested"));
        let _ = std::fs::remove_dir(&self.0);
    }
}
let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
let root = std::env::temp_dir().join(format!("vibeio-doc-{}-{stamp}", std::process::id()));
std::fs::create_dir(&root)?; // Fail before acquiring cleanup ownership if it exists.
let _scratch = Scratch(root.clone());
let runtime = RuntimeBuilder::new()
    .driver(DriverKind::Mock)
    .blocking_pool(Box::new(DemoPool))
    .build()?;
runtime.block_on(async move {
    let path = root.join("hello.txt");
    fs::write(&path, b"Hello, world!").await?;
    assert_eq!(fs::read_to_string(&path).await?, "Hello, world!");
    let file = fs::File::open(&path).await?;
    let (read, buffer) = file.read_at(Vec::with_capacity(5), 7).await;
    let count = read?;
    assert_eq!(count, 5);
    assert_eq!(&buffer[..count], b"world");
    let (read, buffer) = file.read_exact_at(Vec::with_capacity(5), 7).await;
    read?;
    assert_eq!(buffer, b"world");
    drop(file);

    let file = fs::OpenOptions::new()
        .write(true)
        .truncate(true)
        .open(&path)
        .await?;
    let (written, buffer) = file.write_exact_at(b"replacement".to_vec(), 0).await;
    written?;
    assert_eq!(buffer, b"replacement");
    drop(file);
    assert_eq!(fs::read_to_string(&path).await?, "replacement");
    let metadata = fs::metadata(&path).await?;
    assert_eq!(metadata.len(), b"replacement".len() as u64);
    assert!(metadata.file_type().is_file());
    assert!(!metadata.file_type().is_symlink());
    fs::create_dir(root.join("nested")).await?;
    assert!(fs::metadata(root.join("nested")).await?.file_type().is_dir());
    let created = fs::File::create(root.join("created.txt")).await?;
    let (written, buffer) = created.write_at(b"created".to_vec(), 0).await;
    let count = written?;
    // A single write may be short. Finish only the unacknowledged suffix at
    // the matching offset; use write_exact_at directly when no count is needed.
    let (finished, _) = created.write_exact_at(buffer[count..].to_vec(), count as u64).await;
    finished?;
    created.sync_data().await?;
    created.sync_all().await?;
    assert_eq!(created.metadata().await?.len(), 7);
    drop(created);
    assert_eq!(fs::read_to_string(root.join("created.txt")).await?, "created");
    #[cfg(unix)]
    {
        let link = root.join("hello-link");
        fs::symlink_file("hello.txt", &link).await?;
        assert!(fs::symlink_metadata(&link).await?.file_type().is_symlink());
        // Following the same path reports the regular target, not the symlink.
        let followed = fs::metadata(&link).await?;
        assert!(followed.file_type().is_file());
        assert!(!followed.file_type().is_symlink());
    }
    Ok::<(), std::io::Error>(())
})?;
# }
# Ok::<(), Box<dyn std::error::Error>>(())
```

## TCP loopback with the Tokio I/O adapter

This finite exchange needs no external server or Tokio runtime. `AsyncWrap`
provides Tokio I/O traits while vibeio drives the sockets. `write_all` and
`read_exact` handle partial transfers; explicit flushes deliver buffered writes.
The adapter's shutdown only flushes, so dropping the client is used to produce
EOF here. This is sequential request/response, not simultaneous full-duplex I/O.

```rust
use rsloop_vibeio_check::vibeio::{RuntimeBuilder, net::{TcpListener, TcpStream}, time, util::AsyncWrap};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use std::time::Duration;

let runtime = RuntimeBuilder::new().enable_timer(true).build()?;
runtime.block_on(async {
    time::timeout(Duration::from_secs(5), async {
        let listener = TcpListener::bind("127.0.0.1:0")?;
        let address = listener.local_addr()?;
        let (client, (server, peer)) = futures_util::try_join!(
            TcpStream::connect(address), listener.accept()
        )?;
        assert_eq!(client.local_addr()?, peer);
        let mut client = AsyncWrap::new(client);
        let mut server = AsyncWrap::new(server);
        client.write_all(b"hello").await?;
        client.flush().await?;
        let mut message = [0; 5];
        server.read_exact(&mut message).await?;
        assert_eq!(&message, b"hello");
        server.write_all(b"reply").await?;
        server.flush().await?;
        client.read_exact(&mut message).await?;
        assert_eq!(&message, b"reply");
        drop(client);
        let mut tail = Vec::new();
        assert_eq!(server.read_to_end(&mut tail).await?, 0);
        Ok::<(), std::io::Error>(())
    }).await??;
    Ok::<(), Box<dyn std::error::Error>>(())
})?;
# Ok::<(), Box<dyn std::error::Error>>(())
```

## Unix socket exchange and path cleanup

This runs only on Unix. A newly created short directory under `/tmp` keeps the
socket path within Unix-domain address limits, including on macOS. Closing a
listener does not unlink its pathname; the guard removes only this example's
socket and directory, including on an I/O error. Bind is synchronous.

```rust
# #[cfg(unix)]
# {
use rsloop_vibeio_check::vibeio::{DriverKind, RuntimeBuilder, net::{UnixListener, UnixStream}, time, util::AsyncWrap};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use std::{path::PathBuf, time::{Duration, SystemTime, UNIX_EPOCH}};

struct SocketDirectory(PathBuf);
impl Drop for SocketDirectory {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(self.0.join("socket"));
        let _ = std::fs::remove_dir(&self.0);
    }
}
let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_nanos();
let directory = PathBuf::from(format!("/tmp/vb-{}-{stamp:x}", std::process::id()));
std::fs::create_dir(&directory)?;
let _cleanup = SocketDirectory(directory.clone());
let path = directory.join("socket");
let runtime = RuntimeBuilder::new().driver(DriverKind::Mio).enable_timer(true).build()?;
runtime.block_on(async move {
    time::timeout(Duration::from_secs(5), async {
        let listener = UnixListener::bind(&path)?;
        assert_eq!(listener.local_addr()?.as_pathname(), Some(path.as_path()));
        let (client, (server, _)) = futures_util::try_join!(
            UnixStream::connect(&path), listener.accept()
        )?;
        let mut client = AsyncWrap::new(client);
        let mut server = AsyncWrap::new(server);
        client.write_all(b"local").await?;
        client.flush().await?;
        let mut message = [0; 5];
        server.read_exact(&mut message).await?;
        assert_eq!(&message, b"local");
        Ok::<(), std::io::Error>(())
    }).await??;
    Ok::<(), Box<dyn std::error::Error>>(())
})?;
# }
# Ok::<(), Box<dyn std::error::Error>>(())
```

## UDP loopback exchange

Bind is synchronous and fallible; send/receive return both their result and the
owned buffer. This example uses numeric loopback addresses and ephemeral ports,
requires no external server, and bounds the exchange with a timeout. The builder
selects an available I/O driver. Converting a socket into poll mode changes its
registration without changing the local address.

```rust
use rsloop_vibeio_check::vibeio::{RuntimeBuilder, net::UdpSocket, time};
use std::time::Duration;

let runtime = RuntimeBuilder::new().enable_timer(true).build()?;
runtime.block_on(async {
    time::timeout(Duration::from_secs(5), async {
        let receiver = UdpSocket::bind("127.0.0.1:0")?;
        let destination = receiver.local_addr()?;
        let mut sender = UdpSocket::bind("127.0.0.1:0")?;
        let source = sender.local_addr()?;
        sender.connect(destination).await?;
        let (sent, buffer) = sender.send(b"hello".to_vec()).await;
        assert_eq!(sent?, buffer.len());
        let (received, buffer) = receiver.recv_from(Vec::with_capacity(32)).await;
        let (count, address) = received?;
        assert_eq!(address, source);
        assert_eq!(&buffer[..count], b"hello");

        let sender = sender.into_poll()?;
        assert_eq!(sender.local_addr()?, source);
        let (sent, _) = receiver.send_to(b"reply".to_vec(), source).await;
        assert_eq!(sent?, 5);
        let (received, buffer) = sender.recv(Vec::with_capacity(32)).await;
        assert_eq!(&buffer[..received?], b"reply");
        Ok::<(), std::io::Error>(())
    }).await??;
    Ok::<(), Box<dyn std::error::Error>>(())
})?;
# Ok::<(), Box<dyn std::error::Error>>(())
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

## Splice through EOF

This Linux-only example runs with `splice` and does not need the separate `pipe`
or `fs` features. A standard pipe supplies a tiny payload; closing its writer
allows splice_exact to return the actual count at EOF, before its requested limit.
The payload fits in the destination socket before its peer starts reading.
Large transfers need a concurrent consumer to relieve backpressure.

```rust
# #[cfg(all(target_os = "linux", feature = "splice"))]
# {
use rsloop_vibeio_check::vibeio::{DriverKind, RuntimeBuilder, io::splice_exact, net::UnixStream, time};
use std::{io::Write, time::Duration};
use tokio::io::AsyncReadExt;

let runtime = RuntimeBuilder::new().driver(DriverKind::Mio).enable_timer(true).build()?;
runtime.block_on(async {
    time::timeout(Duration::from_secs(5), async {
        let (source, mut producer) = std::io::pipe()?;
        producer.write_all(b"spliced")?;
        drop(producer);
        let (sender, receiver) = std::os::unix::net::UnixStream::pair()?;
        let sender = UnixStream::from_std_poll(sender)?;
        let mut receiver = UnixStream::from_std_poll(receiver)?;
        assert_eq!(splice_exact(&source, &sender, 64).await?, 7);
        drop(sender);
        // PollUnixStream implements Tokio's I/O traits directly.
        let mut bytes = Vec::new();
        assert_eq!(receiver.read_to_end(&mut bytes).await?, 7);
        assert_eq!(bytes, b"spliced");
        Ok::<(), std::io::Error>(())
    }).await??;
    Ok::<(), Box<dyn std::error::Error>>(())
})?;
# }
# Ok::<(), Box<dyn std::error::Error>>(())
```

## Child-process pipes and exit status

This example runs on Unix with `process` and a POSIX `sh` available. It explicitly
provides a demonstration blocking pool, so `blocking-default` is not required.
The child echoes one input line and writes a separate stderr message. Close stdin
after flushing, then drain stdout/stderr concurrently with waiting to avoid pipe
backpressure deadlocks. Spawn itself is synchronous; the I/O is timeout-bounded.
Timeout does not promise to kill a child or stop an offloaded operation.

```rust
# #[cfg(all(unix, feature = "process"))]
# {
use rsloop_vibeio_check::vibeio::{DriverKind, RuntimeBuilder, blocking::BlockingThreadPool, process::{Command, Stdio}, time, util::AsyncWrap};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use std::time::Duration;

struct DemoPool;
impl BlockingThreadPool for DemoPool {
    fn spawn(&self, task: Box<dyn FnOnce() + Send>) { std::thread::spawn(task); }
}
let runtime = RuntimeBuilder::new().driver(DriverKind::Mio)
    .blocking_pool(Box::new(DemoPool)).enable_timer(true).build()?;
runtime.block_on(async {
    time::timeout(Duration::from_secs(5), async {
        let mut child = Command::new("sh")
            .args(["-c", r#"IFS= read -r line; printf '%s' "$line"; printf 'notice' >&2"#])
            .stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::piped())
            .spawn()?;
        let mut input = AsyncWrap::new(child.stdin.take().expect("piped stdin"));
        input.write_all(b"hello\n").await?;
        input.flush().await?;
        drop(input);
        let mut output = AsyncWrap::new(child.stdout.take().expect("piped stdout"));
        let mut errors = AsyncWrap::new(child.stderr.take().expect("piped stderr"));
        let mut out = Vec::new();
        let mut err = Vec::new();
        let (_, _, status) = futures_util::try_join!(
            output.read_to_end(&mut out), errors.read_to_end(&mut err), child.wait()
        )?;
        assert!(status.success());
        assert_eq!(out, b"hello");
        assert_eq!(err, b"notice");
        assert!(Command::new("sh").args(["-c", "exit 0"]).status().await?.success());
        let captured = Command::new("sh").args(["-c", "printf captured"]).output().await?;
        assert!(captured.status.success());
        assert_eq!(captured.stdout, b"captured");
        Ok::<(), std::io::Error>(())
    }).await??;
    Ok::<(), Box<dyn std::error::Error>>(())
})?;
# }
# Ok::<(), Box<dyn std::error::Error>>(())
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

## Registering and cancelling signal waits

`ctrl_c()` registers a listener and returns a `Result`; apply `?` before
awaiting the returned future. Its completion is also fallible. This example
uses an immediate timeout to exercise cancellation without requiring a user
to send a console signal. It runs with the `signal` feature; other feature
configurations compile an empty block.
On Unix, the example also times out a borrowed SIGTERM receive. The listener
is still owned afterward and is explicitly dropped to unregister it. No signals
are sent, so this checks registration/cancellation rather than signal delivery.

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
    #[cfg(unix)]
    {
        let mut termination = signal::Signal::new(signal::SignalKind::terminate())?;
        assert!(time::timeout(Duration::ZERO, termination.recv()).await.is_err());
        drop(termination);
    }
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
