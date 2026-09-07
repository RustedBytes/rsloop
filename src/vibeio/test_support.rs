//! Synchronization and complete-transfer helpers for native tests.

use std::io;
use std::time::Duration;
#[cfg(unix)]
use std::time::Instant;

pub(crate) const WATCHDOG: Duration = Duration::from_secs(30);

pub(crate) fn polling_driver() -> std::rc::Rc<super::driver::AnyDriver> {
    #[cfg(unix)]
    let driver = super::driver::AnyDriver::new_mio();
    #[cfg(windows)]
    let driver = super::driver::AnyDriver::new_iocp();
    std::rc::Rc::new(driver.expect("native polling driver should initialize"))
}

/// Drive a native operation through Pending and interrupted syscalls. Keep
/// terminal errors intact instead of misreporting them as readiness failures.
pub(crate) fn poll_io<T>(
    mut poll: impl FnMut() -> std::task::Poll<io::Result<T>>,
    mut drive: impl FnMut(),
) -> io::Result<T> {
    let deadline = std::time::Instant::now() + WATCHDOG;
    loop {
        match poll() {
            std::task::Poll::Pending => {}
            std::task::Poll::Ready(Err(error)) if error.kind() == io::ErrorKind::Interrupted => {}
            std::task::Poll::Ready(result) => return result,
        }
        if std::time::Instant::now() >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "native operation exceeded watchdog",
            ));
        }
        drive();
    }
}

#[test]
fn poll_io_retries_pending_and_interrupted_but_preserves_terminal_errors() {
    use std::task::Poll;
    let mut outcomes = [
        Poll::Pending,
        Poll::Ready(Err(io::ErrorKind::Interrupted.into())),
        Poll::Ready(Ok(42)),
    ]
    .into_iter();
    let mut waits = 0;
    assert_eq!(
        poll_io(|| outcomes.next().unwrap(), || waits += 1).unwrap(),
        42
    );
    assert_eq!(waits, 2);
    let error = poll_io::<()>(
        || Poll::Ready(Err(io::Error::from_raw_os_error(1234))),
        || panic!("terminal errors must not be retried"),
    )
    .unwrap_err();
    assert_eq!(error.raw_os_error(), Some(1234));
}

/// The reader must be nonblocking or have a read timeout. Kernel work and
/// concurrent fork/exec can briefly retain a pipe/socket after its owner drops.
pub(crate) fn assert_eof(reader: &mut impl io::Read) {
    let deadline = std::time::Instant::now() + WATCHDOG;
    loop {
        match reader.read(&mut [0; 1]) {
            Ok(0) => return,
            Ok(_) => panic!("unexpected data while waiting for EOF"),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock
                        | io::ErrorKind::Interrupted
                        | io::ErrorKind::TimedOut
                ) => {}
            Err(error) => panic!("reading EOF failed: {error}"),
        }
        assert!(
            std::time::Instant::now() < deadline,
            "last writer was not released"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

#[test]
fn eof_check_tolerates_interruption_and_deferred_release() {
    struct Closing(usize);
    impl io::Read for Closing {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            self.0 += 1;
            match self.0 {
                1 => Err(io::ErrorKind::Interrupted.into()),
                2 => Err(io::ErrorKind::WouldBlock.into()),
                _ => Ok(0),
            }
        }
    }
    let mut reader = Closing(0);
    assert_eof(&mut reader);
    assert_eq!(reader.0, 3);
}

pub(crate) async fn with_watchdog<F: std::future::Future>(future: F) -> F::Output {
    super::time::timeout(WATCHDOG, future)
        .await
        .expect("native test exceeded watchdog")
}

/// Trigger a remote notification only after the future installs its waiter.
#[cfg(feature = "signal")]
pub(crate) async fn notify_after_pending<F: std::future::Future>(
    future: F,
    notify: impl FnOnce() + Send + 'static,
) -> F::Output {
    let (start, ready) = std::sync::mpsc::channel();
    let worker = std::thread::spawn(move || {
        if ready.recv().is_ok() {
            notify();
        }
    });
    let mut future = std::pin::pin!(future);
    let mut start = Some(start);
    let result = with_watchdog(std::future::poll_fn(move |cx| {
        let result = future.as_mut().poll(cx);
        if let Some(start) = start.take() {
            assert!(
                result.is_pending(),
                "notification future must initially wait"
            );
            start.send(()).unwrap();
        }
        result
    }))
    .await;
    worker.join().expect("notification worker panicked");
    result
}

/// Real Unix signals must not interrupt unrelated tests in the same harness.
#[cfg(all(unix, feature = "signal"))]
pub(crate) fn isolated_signal_test() -> bool {
    const CHILD: &str = "VIBEIO_ISOLATED_SIGNAL_TEST";
    let thread = std::thread::current();
    let name = thread.name().expect("named test thread");
    if std::env::var(CHILD).as_deref() == Ok(name) {
        return false;
    }
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", name, "--nocapture"])
        .env(CHILD, name)
        .spawn()
        .unwrap();
    let deadline = Instant::now() + WATCHDOG;
    loop {
        if let Some(status) = child.try_wait().unwrap() {
            assert!(status.success(), "isolated signal test failed: {status}");
            return true;
        }
        if Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            panic!("isolated signal test exceeded watchdog");
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// A watchdog, not a performance requirement. Success depends on `ready`.
#[cfg(unix)]
pub(crate) fn drive_until(mut ready: impl FnMut() -> bool, mut drive: impl FnMut()) {
    let deadline = Instant::now() + WATCHDOG;
    while !ready() {
        assert!(
            Instant::now() < deadline,
            "native test made no progress within watchdog"
        );
        drive();
    }
}

pub(crate) async fn read_exact(
    reader: &mut impl super::io::AsyncRead,
    len: usize,
) -> io::Result<Vec<u8>> {
    let mut bytes = Vec::with_capacity(len);
    while bytes.len() < len {
        let (result, buffer) = reader.read(vec![0; len - bytes.len()]).await;
        let count = match result {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => result?,
        };
        if count == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        assert!(count <= buffer.len());
        bytes.extend_from_slice(&buffer[..count]);
    }
    Ok(bytes)
}

pub(crate) async fn write_all(
    writer: &mut impl super::io::AsyncWrite,
    bytes: &[u8],
) -> io::Result<()> {
    let mut offset = 0;
    while offset < bytes.len() {
        let (result, _) = writer.write(bytes[offset..].to_vec()).await;
        let count = match result {
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            result => result?,
        };
        if count == 0 {
            return Err(io::ErrorKind::WriteZero.into());
        }
        assert!(count <= bytes.len() - offset);
        offset += count;
    }
    Ok(())
}

#[test]
fn complete_transfers_retry_interruptions_and_handle_single_byte_progress() {
    use super::io::{AsyncRead, AsyncWrite, IoBuf, IoBufMut};
    struct Fragmented {
        bytes: Vec<u8>,
        interrupt: bool,
    }
    impl AsyncRead for Fragmented {
        async fn read<B: IoBufMut>(&mut self, mut buf: B) -> (io::Result<usize>, B) {
            if std::mem::take(&mut self.interrupt) {
                return (Err(io::ErrorKind::Interrupted.into()), buf);
            }
            if self.bytes.is_empty() {
                return (Ok(0), buf);
            }
            assert!(buf.buf_capacity() > 0);
            // SAFETY: capacity was checked and this exclusive buffer is not
            // retained. Initialize exactly the byte reported by this read.
            unsafe {
                buf.as_buf_mut_ptr().write(self.bytes.remove(0));
                buf.set_buf_init(1);
            }
            (Ok(1), buf)
        }
    }
    impl AsyncWrite for Fragmented {
        async fn write<B: IoBuf>(&mut self, buf: B) -> (io::Result<usize>, B) {
            if std::mem::take(&mut self.interrupt) {
                return (Err(io::ErrorKind::Interrupted.into()), buf);
            }
            assert!(buf.buf_len() > 0);
            // SAFETY: the owned buffer exposes at least one initialized byte.
            self.bytes.push(unsafe { *buf.as_buf_ptr() });
            (Ok(1), buf)
        }
    }
    super::executor::Runtime::new(super::driver::AnyDriver::new_mock()).block_on(async {
        let mut source = Fragmented {
            bytes: b"abc".to_vec(),
            interrupt: true,
        };
        assert_eq!(read_exact(&mut source, 3).await.unwrap(), b"abc");
        assert_eq!(
            read_exact(&mut source, 1).await.unwrap_err().kind(),
            io::ErrorKind::UnexpectedEof
        );
        let mut sink = Fragmented {
            bytes: Vec::new(),
            interrupt: true,
        };
        write_all(&mut sink, b"abc").await.unwrap();
        assert_eq!(sink.bytes, b"abc");
        assert!(read_exact(&mut source, 0).await.unwrap().is_empty());
        write_all(&mut sink, b"").await.unwrap();
        assert_eq!(sink.bytes, b"abc");
    });
}
