mod listener;
mod stream;

pub use listener::*;
pub use stream::*;

#[cfg(test)]
mod tests {
    use std::io::{self as std_io};
    use std::net::{Shutdown, SocketAddr};

    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    use crate::io::{AsyncRead, AsyncWrite};
    use crate::{driver::AnyDriver, executor::spawn};

    use super::{PollTcpStream, TcpListener, TcpStream};

    #[inline]
    fn try_bind_listener(address: SocketAddr) -> Option<TcpListener> {
        match TcpListener::bind(address) {
            Ok(listener) => Some(listener),
            Err(err) if err.kind() == std_io::ErrorKind::PermissionDenied => None,
            Err(err) => panic!("listener should bind: {err}"),
        }
    }

    #[test]
    fn tcp_listener_and_stream_exchange_data() {
        let runtime = crate::executor::Runtime::new(
            #[cfg(unix)]
            AnyDriver::new_mio().expect("mio driver should initialize"),
            #[cfg(windows)]
            AnyDriver::new_iocp().expect("iocp driver should initialize"),
        );
        runtime.block_on(async {
            let address = "127.0.0.1:0"
                .parse::<SocketAddr>()
                .expect("address should parse");
            let Some(listener) = try_bind_listener(address) else {
                return;
            };
            let server_address = listener
                .local_addr()
                .expect("listener should expose address");

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

            let mut client = TcpStream::connect(server_address)
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
            assert_eq!(
                client
                    .peer_addr()
                    .expect("peer address should be available"),
                server_address
            );
            client
                .shutdown(Shutdown::Both)
                .expect("shutdown should succeed");

            server.await.expect("server task should complete");
        });
    }

    #[test]
    fn poll_tcp_stream_uses_readiness_path() {
        let runtime = crate::executor::Runtime::new(
            #[cfg(unix)]
            AnyDriver::new_mio().expect("mio driver should initialize"),
            #[cfg(windows)]
            AnyDriver::new_iocp().expect("iocp driver should initialize"),
        );
        runtime.block_on(async {
            let address = "127.0.0.1:0"
                .parse::<SocketAddr>()
                .expect("address should parse");
            let Some(listener) = try_bind_listener(address) else {
                return;
            };
            let server_address = listener
                .local_addr()
                .expect("listener should expose address");

            let server = spawn(async move {
                let (mut stream, _) = listener.accept().await?;
                let received = [0u8; 4];
                let (read, received) = stream.read(received).await;
                let read = read?;
                assert_eq!(&received[..read], b"mio!");
                stream.write(b"ok".to_vec()).await.0?;
                Ok::<(), std_io::Error>(())
            });

            let mut client = PollTcpStream::connect(server_address)
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
