# vibeio

A high-performance, cross-platform asynchronous runtime for Rust.

`vibeio` provides an efficient I/O event loop that leverages the best available driver for each operating system:

- **Linux** - uses `io_uring` for true asynchronous I/O.
- **Windows** - uses I/O Completion Ports (IOCP) for scalable I/O.
- **macOS / BSD / Others** - uses `kqueue` or `epoll` via `mio` for event notification.

## Core features

- **Networking** - asynchronous TCP, UDP, and Unix Domain Sockets.
- **File system** - asynchronous file operations.
- **Timers** - efficient timer and sleep functionality.
- **Signals** - handling of OS signals.
- **Process management** - spawning and managing child processes.
- **Blocking tasks** - offload CPU-intensive or blocking operations to a thread pool.

## Concurrency model: thread-per-core

`vibeio` is designed as a **single-threaded** runtime. To utilize multiple cores, you should employ a **thread-per-core** architecture, where a separate `Runtime` is pinned to each processor core. This approach minimizes synchronization overhead and maximizes cache locality.

Shared state can be communicated between runtimes using message passing (e.g., channels) or shared atomic structures, but I/O resources are typically owned by the thread that created them.

## But why not Tokio?

Tokio is a popular asynchronous runtime for Rust, but it uses a **work-stealing** model and may introduce additional synchronization overhead when using it as a thread-per-core runtime. `vibeio` is more specialized for thread-per-core architectures that are optimized for low overhead and cache locality.

## Getting started

Add `vibeio` to your `Cargo.toml`:

```toml
[dependencies]
vibeio = "0.2"
```

### Example: TCP echo server

```rust
use vibeio::RuntimeBuilder;
use vibeio::net::TcpListener;

fn main() -> std::io::Result<()> {
    // 1. Build the runtime
    let runtime = RuntimeBuilder::new()
        .enable_timer(true)
        .build()?;

    // 2. Run the main future
    runtime.block_on(async {
        let listener = TcpListener::bind("127.0.0.1:8080")?;
        println!("Listening on 127.0.0.1:8080");

        loop {
            let (mut stream, _) = listener.accept().await?;

            vibeio::spawn(async move {
                let (mut reader, mut writer) = vibeio::io::split(stream);
                if let Err(e) = vibeio::io::copy(&mut reader, &mut writer).await {
                    eprintln!("Echo failed: {}", e);
                }
            });
        }
    })
}
```

## Feature flags

The following features are available (most are enabled by default):

- `fs` - enables asynchronous file system operations.
- `time` - enables time and timer functionality.
- `signal` - enables signal handling.
- `process` - enables child process management.
- `pipe` - enables pipe support.
- `stdio` - enables standard I/O support.
- `splice` - enables splice support (Linux).
- `blocking-default` - enables the default blocking thread pool.

## "vibeio" name

The name `vibeio` has two parts - "vibe" (from "vibe coding", as originally this project was coded with help of AI), and "io" (common suffix for Rust async runtime names).

## License

[MIT](./LICENSE)
