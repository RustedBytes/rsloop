mod listener;
mod stream;

pub use listener::*;
pub use stream::*;

#[cfg(test)]
mod tests {
    use std::fs;
    use std::io::{self as std_io};
    use std::net::Shutdown;
    use std::path::{Path, PathBuf};
    use std::sync::atomic::{AtomicU64, Ordering};
    use std::time::{SystemTime, UNIX_EPOCH};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use crate::io::{AsyncRead, AsyncWrite};
    use crate::net::PollUnixStream;
    use crate::{driver::AnyDriver, executor::spawn};

    use super::{UnixListener, UnixStream};

    fn unique_socket_path(name: &str) -> PathBuf {
        static NEXT_ID: AtomicU64 = AtomicU64::new(0);
        let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap_or_default()
            .as_nanos();
        std::env::temp_dir().join(format!("vibeio-{name}-{nanos}-{id}.sock"))
    }

    fn cleanup_socket(path: &Path) {
        match fs::remove_file(path) {
            Ok(()) => {}
            Err(err) if err.kind() == std_io::ErrorKind::NotFound => {}
            Err(err) => panic!("failed to cleanup socket {}: {err}", path.display()),
        }
    }

    #[inline]
    fn try_bind_listener(path: &Path) -> Option<UnixListener> {
        match UnixListener::bind(path) {
            Ok(listener) => Some(listener),
            Err(err) if err.kind() == std_io::ErrorKind::PermissionDenied => None,
            Err(err) => panic!("listener should bind: {err}"),
        }
    }

    #[test]
    fn unix_listener_and_stream_exchange_data() {
        let runtime = crate::executor::Runtime::new(
            AnyDriver::new_mio().expect("mio driver should initialize"),
        );
        runtime.block_on(async {
            let socket_path = unique_socket_path("exchange");
            cleanup_socket(&socket_path);

            let Some(listener) = try_bind_listener(&socket_path) else {
                return;
            };
            assert_eq!(
                listener
                    .local_addr()
                    .expect("listener local addr should be available")
                    .as_pathname(),
                Some(socket_path.as_path())
            );

            let server = spawn(async move {
                let (mut stream, _) = listener.accept().await?;
                let buffer = [0u8; 4];
                let (read, buffer) = stream.read(buffer).await;
                let read = read?;
                assert_eq!(&buffer[..read], b"ping");
                stream.write(b"pong".to_vec()).await.0?;
                stream.shutdown(Shutdown::Both)?;
                Ok::<(), std_io::Error>(())
            });

            let mut client = UnixStream::connect(&socket_path)
                .await
                .expect("client should connect");
            client
                .write(b"ping".to_vec())
                .await
                .0
                .expect("client should write");
            let response = [0u8; 4];
            let (read, response) = client.read(response).await;
            let read = read.expect("client should read");
            assert_eq!(&response[..read], b"pong");
            #[cfg(not(target_os = "macos"))]
            assert_eq!(
                client
                    .peer_addr()
                    .expect("peer address should be available")
                    .as_pathname(),
                Some(socket_path.as_path())
            );
            client
                .shutdown(Shutdown::Both)
                .expect("shutdown should succeed");

            server.await.expect("server task should complete");
            cleanup_socket(&socket_path);
        });
    }

    #[test]
    fn poll_unix_stream_uses_readiness_path() {
        let runtime = crate::executor::Runtime::new(
            #[cfg(unix)]
            AnyDriver::new_mio().expect("mio driver should initialize"),
        );
        runtime.block_on(async {
            let socket_path = unique_socket_path("vectored");
            cleanup_socket(&socket_path);
            let Some(listener) = try_bind_listener(&socket_path) else {
                return;
            };

            let server = spawn(async move {
                let (mut stream, _) = listener.accept().await?;
                let received = [0u8; 4];
                let (read, received) = stream.read(received).await;
                let read = read?;
                assert_eq!(&received[..read], b"mio!");
                stream.write(b"ok".to_vec()).await.0?;
                Ok::<(), std_io::Error>(())
            });

            let mut client = PollUnixStream::connect(socket_path)
                .await
                .expect("client should connect");
            AsyncWriteExt::write_all(&mut client, b"mio!")
                .await
                .expect("tokio write_all should succeed");
            let mut response = [0u8; 2];
            AsyncReadExt::read_exact(&mut client, &mut response)
                .await
                .expect("tokio read_exact should succeed");
            assert_eq!(&response, b"ok");

            server.await.expect("server task should complete");
        });
    }
}
