//! Socket readers hosted on the loop's `vibeio` runtime.
//!
//! These are the readers that actually run for TCP and Unix connections. They
//! read into a pooled buffer and hand it to the transport by move whenever the
//! chunk is large enough to be worth it, so a busy connection recycles one
//! allocation instead of allocating per read.
//!
//! Windows needs both modes: overlapped receives are fastest for bulk reads,
//! but a server that is mid-`WSARecv` blocks a duplicated writer socket. When a
//! write asks for it, the reader cancels the overlapped receive (surfacing as
//! `ERROR_OPERATION_ABORTED`) and rebinds the same socket in readiness mode
//! until the write is through.

use std::io;
use std::net::TcpStream as StdTcpStream;
#[cfg(unix)]
use std::os::unix::net::UnixStream as StdUnixStream;
use std::sync::Arc;
#[cfg(windows)]
use std::sync::atomic::Ordering;

#[cfg(windows)]
use crate::vibeio::io::AsyncRead as VibeAsyncRead;
use crate::vibeio::net::PollTcpStream as VibePollTcpStream;
#[cfg(unix)]
use crate::vibeio::net::PollUnixStream as VibePollUnixStream;
#[cfg(windows)]
use crate::vibeio::net::TcpStream as VibeTcpStream;
use tokio::io::AsyncReadExt;
#[cfg(windows)]
use windows_sys::Win32::Foundation::ERROR_OPERATION_ABORTED;

#[cfg(windows)]
use super::tuning::SERVER_POLL_READER_TINY_TRIGGER_MAX_BYTES;
use super::tuning::{MAX_STREAM_READ_BUFFER_SIZE, STREAM_READ_BUFFER_SIZE};
use super::{PendingReadEvent, StreamTransportCore};

#[cfg(not(windows))]
pub(crate) async fn run_tcp_socket_reader_task(
    core: Arc<StreamTransportCore>,
    stream: Arc<StdTcpStream>,
) {
    let mut reader = match VibePollTcpStream::from_shared(stream) {
        Ok(reader) => reader,
        Err(err) => {
            core.enqueue_pending_read_event(PendingReadEvent::ConnectionLost(Some(
                err.to_string(),
            )));
            return;
        }
    };
    // `read_buf` writes into spare capacity, so idle transports reserve this
    // address space without eagerly zero-filling and faulting in every page.
    let Some(mut buf) = core
        .acquire_read_buffer_async(STREAM_READ_BUFFER_SIZE)
        .await
    else {
        return;
    };

    loop {
        if core.is_closing() {
            core.enqueue_pending_read_event(PendingReadEvent::ConnectionLost(None));
            return;
        }

        core.wait_until_async_readable().await;
        if core.is_closing() {
            core.enqueue_pending_read_event(PendingReadEvent::ConnectionLost(None));
            return;
        }

        buf.clear();
        match reader.read_buf(&mut buf).await {
            Ok(0) => {
                core.enqueue_pending_read_event(PendingReadEvent::Eof);
                return;
            }
            Ok(_) => {
                let next_capacity = next_read_capacity(&buf);
                core.enqueue_pending_read_event(PendingReadEvent::Data(buf));
                let Some(next) = core.acquire_read_buffer_async(next_capacity).await else {
                    return;
                };
                buf = next;
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => continue,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => {
                core.enqueue_pending_read_event(PendingReadEvent::ConnectionLost(Some(
                    err.to_string(),
                )));
                return;
            }
        }
    }
}

#[cfg(windows)]
pub(crate) async fn run_tcp_socket_reader_task(
    core: Arc<StreamTransportCore>,
    stream: Arc<StdTcpStream>,
) {
    // Client protocols can initiate an unbounded stream of writes without
    // receiving anything first. Start their shared socket in nonblocking poll
    // mode so a full send buffer can never block the event-loop thread.
    if !core.server_side {
        let reader = match VibePollTcpStream::from_shared(stream) {
            Ok(reader) => reader,
            Err(err) => {
                core.mark_poll_reader_ready(false);
                core.enqueue_pending_read_event(PendingReadEvent::ConnectionLost(Some(
                    err.to_string(),
                )));
                return;
            }
        };
        let Some(buf) = core
            .acquire_read_buffer_async(STREAM_READ_BUFFER_SIZE)
            .await
        else {
            core.mark_poll_reader_ready(false);
            return;
        };
        core.poll_reader_requested.store(true, Ordering::Release);
        core.mark_poll_reader_ready(true);
        run_windows_poll_tcp_reader(core, reader, buf).await;
        return;
    }

    // Server protocols start in completion mode on Windows. Besides avoiding
    // readiness polling for callback-driven protocols (aiohttp, websockets,
    // uvicorn), this keeps their hot path aligned with native fast streams.
    // start_tls and large writes request a rebind to poll mode before they
    // reclaim or heavily write through the shared socket.
    let mut reader =
        match VibeTcpStream::from_shared(stream, crate::vibeio::RegistrationMode::Completion) {
            Ok(reader) => reader,
            Err(err) => {
                core.mark_poll_reader_ready(false);
                core.enqueue_pending_read_event(PendingReadEvent::ConnectionLost(Some(
                    err.to_string(),
                )));
                return;
            }
        };
    let Some(mut buf) = core
        .acquire_read_buffer_async(STREAM_READ_BUFFER_SIZE)
        .await
    else {
        core.mark_poll_reader_ready(false);
        return;
    };

    loop {
        if core.is_closing() {
            core.enqueue_pending_read_event(PendingReadEvent::ConnectionLost(None));
            return;
        }
        core.wait_until_async_readable().await;
        if core.is_closing() {
            core.enqueue_pending_read_event(PendingReadEvent::ConnectionLost(None));
            return;
        }

        if core.poll_reader_requested() {
            match reader.into_poll() {
                Ok(poll_reader) => {
                    core.mark_poll_reader_ready(true);
                    run_windows_poll_tcp_reader(core, poll_reader, buf).await;
                }
                Err(err) => {
                    core.mark_poll_reader_ready(false);
                    core.enqueue_pending_read_event(PendingReadEvent::ConnectionLost(Some(
                        err.to_string(),
                    )));
                }
            }
            return;
        }

        buf.clear();
        let (result, returned_buf) = VibeAsyncRead::read(&mut reader, buf).await;
        buf = returned_buf;
        match result {
            Ok(0) => {
                if core.poll_reader_requested() {
                    core.mark_poll_reader_ready(false);
                }
                core.enqueue_pending_read_event(PendingReadEvent::Eof);
                return;
            }
            Ok(read) => {
                let saturated = read == buf.capacity();
                let next_capacity = next_read_capacity(&buf);
                let data = buf;
                let tiny_server_trigger =
                    core.server_side && read <= SERVER_POLL_READER_TINY_TRIGGER_MAX_BYTES;

                // Completion reads win on normal request/response traffic. A
                // full inbound buffer indicates sustained transfer; a tiny
                // server command commonly triggers a large outbound response.
                // Rebind before delivering either event so the protocol cannot
                // begin bulk writes while an overlapped receive is outstanding.
                if saturated || tiny_server_trigger {
                    if core.server_side {
                        core.poll_reader_requested.store(true, Ordering::Release);
                    }
                    match reader.into_poll() {
                        Ok(poll_reader) => {
                            core.mark_poll_reader_ready(true);
                            core.enqueue_pending_read_event(PendingReadEvent::Data(data));
                            let Some(next) = core.acquire_read_buffer_async(next_capacity).await
                            else {
                                return;
                            };
                            run_windows_poll_tcp_reader(core, poll_reader, next).await;
                        }
                        Err(err) => {
                            core.mark_poll_reader_ready(false);
                            core.enqueue_pending_read_event(PendingReadEvent::Data(data));
                            core.enqueue_pending_read_event(PendingReadEvent::ConnectionLost(
                                Some(err.to_string()),
                            ));
                        }
                    }
                    return;
                }

                core.enqueue_pending_read_event(PendingReadEvent::Data(data));
                let Some(next) = core.acquire_read_buffer_async(next_capacity).await else {
                    return;
                };
                buf = next;
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => continue,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err)
                if core.poll_reader_requested()
                    && err.raw_os_error() == Some(ERROR_OPERATION_ABORTED.cast_signed()) =>
            {
                match reader.into_poll() {
                    Ok(poll_reader) => {
                        core.mark_poll_reader_ready(true);
                        run_windows_poll_tcp_reader(core, poll_reader, buf).await;
                    }
                    Err(err) => {
                        core.mark_poll_reader_ready(false);
                        core.enqueue_pending_read_event(PendingReadEvent::ConnectionLost(Some(
                            err.to_string(),
                        )));
                    }
                }
                return;
            }
            Err(err) => {
                core.mark_poll_reader_ready(false);
                core.enqueue_pending_read_event(PendingReadEvent::ConnectionLost(Some(
                    err.to_string(),
                )));
                return;
            }
        }
    }
}

#[cfg(windows)]
pub(super) async fn run_windows_poll_tcp_reader(
    core: Arc<StreamTransportCore>,
    mut reader: VibePollTcpStream,
    mut buf: Vec<u8>,
) {
    loop {
        if core.is_closing() {
            core.enqueue_pending_read_event(PendingReadEvent::ConnectionLost(None));
            return;
        }
        core.wait_until_async_readable().await;
        if core.is_closing() {
            core.enqueue_pending_read_event(PendingReadEvent::ConnectionLost(None));
            return;
        }

        buf.clear();
        match reader.read_buf(&mut buf).await {
            Ok(0) => {
                core.enqueue_pending_read_event(PendingReadEvent::Eof);
                return;
            }
            Ok(_) => {
                let next_capacity = next_read_capacity(&buf);
                core.enqueue_pending_read_event(PendingReadEvent::Data(buf));
                let Some(next) = core.acquire_read_buffer_async(next_capacity).await else {
                    return;
                };
                buf = next;
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => continue,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => {
                core.enqueue_pending_read_event(PendingReadEvent::ConnectionLost(Some(
                    err.to_string(),
                )));
                return;
            }
        }
    }
}

#[cfg(unix)]
pub(crate) async fn run_unix_socket_reader_task(
    core: Arc<StreamTransportCore>,
    stream: StdUnixStream,
) {
    let mut reader = match VibePollUnixStream::from_std(stream) {
        Ok(reader) => reader,
        Err(err) => {
            core.enqueue_pending_read_event(PendingReadEvent::ConnectionLost(Some(
                err.to_string(),
            )));
            return;
        }
    };
    let Some(mut buf) = core
        .acquire_read_buffer_async(STREAM_READ_BUFFER_SIZE)
        .await
    else {
        return;
    };

    loop {
        if core.is_closing() {
            core.enqueue_pending_read_event(PendingReadEvent::ConnectionLost(None));
            return;
        }
        core.wait_until_async_readable().await;
        if core.is_closing() {
            core.enqueue_pending_read_event(PendingReadEvent::ConnectionLost(None));
            return;
        }
        buf.clear();
        match reader.read_buf(&mut buf).await {
            Ok(0) => {
                core.enqueue_pending_read_event(PendingReadEvent::Eof);
                return;
            }
            Ok(_) => {
                let next_capacity = next_read_capacity(&buf);
                core.enqueue_pending_read_event(PendingReadEvent::Data(buf));
                let Some(next) = core.acquire_read_buffer_async(next_capacity).await else {
                    return;
                };
                buf = next;
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => continue,
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => {
                core.enqueue_pending_read_event(PendingReadEvent::ConnectionLost(Some(
                    err.to_string(),
                )));
                return;
            }
        }
    }
}

fn next_read_capacity(buf: &Vec<u8>) -> usize {
    next_read_capacity_for(buf.len(), buf.capacity())
}

fn next_read_capacity_for(len: usize, allocated_capacity: usize) -> usize {
    let capacity = allocated_capacity.max(STREAM_READ_BUFFER_SIZE);
    if len == capacity && capacity < MAX_STREAM_READ_BUFFER_SIZE {
        (capacity * 2).min(MAX_STREAM_READ_BUFFER_SIZE)
    } else if capacity > STREAM_READ_BUFFER_SIZE && len < capacity / 4 {
        (capacity / 2).max(STREAM_READ_BUFFER_SIZE)
    } else {
        capacity
    }
}

#[cfg(kani)]
mod verification {
    use super::*;

    #[kani::proof]
    fn merge_next_read_capacity_grows_and_shrinks_within_bounds() {
        let len: usize = kani::any();
        let allocated_capacity: usize = kani::any();
        kani::assume(len <= allocated_capacity);

        let capacity = allocated_capacity.max(STREAM_READ_BUFFER_SIZE);
        let next = next_read_capacity_for(len, allocated_capacity);

        assert!(next >= STREAM_READ_BUFFER_SIZE);
        assert!(next <= capacity.max(MAX_STREAM_READ_BUFFER_SIZE));
        if capacity <= MAX_STREAM_READ_BUFFER_SIZE {
            assert!(next <= MAX_STREAM_READ_BUFFER_SIZE);
        }

        if len == capacity && capacity < MAX_STREAM_READ_BUFFER_SIZE {
            assert_eq!(next, (capacity * 2).min(MAX_STREAM_READ_BUFFER_SIZE));
            assert!(next > capacity);
        } else if capacity > STREAM_READ_BUFFER_SIZE && len < capacity / 4 {
            assert_eq!(next, (capacity / 2).max(STREAM_READ_BUFFER_SIZE));
            assert!(next <= capacity);
        } else {
            assert_eq!(next, capacity);
        }
    }
}
