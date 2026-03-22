use std::io;
use std::os::fd::RawFd;

#[cfg(target_os = "linux")]
use std::future::Future;
#[cfg(target_os = "linux")]
use std::os::fd::FromRawFd;
#[cfg(target_os = "linux")]
use std::thread;

#[cfg(target_os = "linux")]
use futures::channel::oneshot;
#[cfg(target_os = "linux")]
use futures_lite::io::{AsyncReadExt, AsyncWriteExt};
#[cfg(target_os = "linux")]
use glommio::LocalExecutorBuilder;

#[cfg(target_os = "linux")]
use crate::fd_ops;

#[cfg(target_os = "linux")]
const MIN_GLOMMIO_IO_MEMORY: usize = 64 * 1024;

#[cfg(target_os = "linux")]
enum StreamSocketKind {
    Tcp,
    Unix,
}

#[cfg(target_os = "linux")]
pub async fn recv_stream_socket(fd: RawFd, len: usize) -> Option<io::Result<Vec<u8>>> {
    if len == 0 {
        return Some(Ok(Vec::new()));
    }

    run_short_lived("rsloop-uring-recv", move || async move {
        let dup = fd_ops::dup_raw_fd(fd)?;
        let kind = detect_stream_socket_kind(dup)?;
        let mut buf = vec![0_u8; len];
        let read = match kind {
            StreamSocketKind::Tcp => {
                let mut stream = unsafe { glommio::net::TcpStream::from_raw_fd(dup) };
                stream
                    .read(&mut buf)
                    .await
                    .map_err(map_glommio_error_to_io)?
            }
            StreamSocketKind::Unix => {
                let mut stream = unsafe { glommio::net::UnixStream::from_raw_fd(dup) };
                stream
                    .read(&mut buf)
                    .await
                    .map_err(map_glommio_error_to_io)?
            }
        };
        buf.truncate(read);
        Ok(buf)
    })
    .await
}

#[cfg(target_os = "linux")]
pub async fn send_all_stream_socket(fd: RawFd, data: Vec<u8>) -> Option<io::Result<()>> {
    run_short_lived("rsloop-uring-send", move || async move {
        let dup = fd_ops::dup_raw_fd(fd)?;
        let kind = detect_stream_socket_kind(dup)?;
        match kind {
            StreamSocketKind::Tcp => {
                let mut stream = unsafe { glommio::net::TcpStream::from_raw_fd(dup) };
                stream
                    .write_all(&data)
                    .await
                    .map_err(map_glommio_error_to_io)?;
                stream.flush().await.map_err(map_glommio_error_to_io)?;
            }
            StreamSocketKind::Unix => {
                let mut stream = unsafe { glommio::net::UnixStream::from_raw_fd(dup) };
                stream
                    .write_all(&data)
                    .await
                    .map_err(map_glommio_error_to_io)?;
                stream.flush().await.map_err(map_glommio_error_to_io)?;
            }
        }
        Ok(())
    })
    .await
}

#[cfg(not(target_os = "linux"))]
pub async fn recv_stream_socket(_fd: RawFd, _len: usize) -> Option<io::Result<Vec<u8>>> {
    None
}

#[cfg(not(target_os = "linux"))]
pub async fn send_all_stream_socket(_fd: RawFd, _data: Vec<u8>) -> Option<io::Result<()>> {
    None
}

#[cfg(target_os = "linux")]
async fn run_short_lived<T, G, F>(name: &'static str, fut_gen: G) -> Option<io::Result<T>>
where
    T: Send + 'static,
    G: FnOnce() -> F + Send + 'static,
    F: Future<Output = io::Result<T>> + 'static,
{
    let (tx, rx) = oneshot::channel();
    let spawn_result = thread::Builder::new().name(name.to_owned()).spawn(move || {
        let result = run_glommio_future(name, fut_gen);
        let _ = tx.send(result);
    });
    if spawn_result.is_err() {
        return None;
    }

    rx.await.ok()
}

#[cfg(target_os = "linux")]
fn run_glommio_future<T, G, F>(name: &'static str, fut_gen: G) -> io::Result<T>
where
    G: FnOnce() -> F,
    F: Future<Output = io::Result<T>> + 'static,
{
    let executor = LocalExecutorBuilder::default()
        .name(name)
        .io_memory(MIN_GLOMMIO_IO_MEMORY)
        .make()
        .map_err(map_glommio_error_to_io)?;
    executor.run(fut_gen())
}

#[cfg(target_os = "linux")]
fn detect_stream_socket_kind(fd: RawFd) -> io::Result<StreamSocketKind> {
    let socket_type = getsockopt_i32(fd, libc::SOL_SOCKET, libc::SO_TYPE)?;
    if socket_type != libc::SOCK_STREAM {
        return Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "glommio backend only supports stream sockets",
        ));
    }

    let mut addr = std::mem::MaybeUninit::<libc::sockaddr_storage>::zeroed();
    let mut len = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
    let rc = unsafe { libc::getsockname(fd, addr.as_mut_ptr().cast::<libc::sockaddr>(), &mut len) };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }

    let family = unsafe { addr.assume_init() }.ss_family as i32;
    match family {
        libc::AF_INET | libc::AF_INET6 => Ok(StreamSocketKind::Tcp),
        libc::AF_UNIX => Ok(StreamSocketKind::Unix),
        _ => Err(io::Error::new(
            io::ErrorKind::Unsupported,
            format!("unsupported socket family {family}"),
        )),
    }
}

#[cfg(target_os = "linux")]
fn getsockopt_i32(fd: RawFd, level: i32, optname: i32) -> io::Result<i32> {
    let mut value = 0_i32;
    let mut len = std::mem::size_of::<i32>() as libc::socklen_t;
    let rc = unsafe {
        libc::getsockopt(
            fd,
            level,
            optname,
            (&mut value as *mut i32).cast(),
            &mut len,
        )
    };
    if rc != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(value)
}

#[cfg(target_os = "linux")]
fn map_glommio_error_to_io<E: std::fmt::Display>(err: E) -> io::Error {
    io::Error::new(io::ErrorKind::Other, err.to_string())
}
