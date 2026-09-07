#![deny(unsafe_op_in_unsafe_fn)]
#![warn(clippy::undocumented_unsafe_blocks)]

use std::io;
#[cfg(unix)]
use std::mem::MaybeUninit;
use std::net::SocketAddr;
use std::task::{Context, Poll};

use mio::Interest;
#[cfg(windows)]
use windows_sys::Win32::{
    Networking::WinSock::{
        self as WinSock, MSG_PEEK, SOCKADDR, SOCKADDR_STORAGE, SOCKET, WSA_IO_PENDING, WSABUF,
    },
    System::IO::OVERLAPPED,
};

use crate::vibeio::driver::AnyDriver;
use crate::vibeio::driver::CompletionIoResult;
use crate::vibeio::fd_inner::InnerRawHandle;
#[cfg(windows)]
use crate::vibeio::fd_inner::RawOsHandle;
use crate::vibeio::io::IoBufMut;
use crate::vibeio::op::Op;
use crate::vibeio::op::io_util::CompletionBuffer;
use crate::vibeio::op::socket_addr::sockaddr_storage_to_socketaddr;

#[cfg(windows)]
#[inline]
fn socket_recvfrom(
    socket: SOCKET,
    buf: &mut impl IoBufMut,
    peek: bool,
) -> io::Result<(usize, SocketAddr)> {
    use windows_sys::Win32::Networking::WinSock::{
        self as WinSock, MSG_PEEK, SOCKADDR, SOCKADDR_STORAGE, SOCKET_ERROR, WSABUF,
    };

    let len = crate::vibeio::op::io_util::completion_len(buf.buf_capacity()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "read buffer is too large for Windows socket I/O",
        )
    })?;

    let mut wsabuf = WSABUF {
        len,
        buf: buf.as_buf_mut_ptr().cast(),
    };
    let mut bytes: u32 = 0;
    let mut flags: u32 = if peek { MSG_PEEK as u32 } else { 0 };
    let mut addr = SOCKADDR_STORAGE::default();
    let mut addr_len = std::mem::size_of::<SOCKADDR_STORAGE>() as i32;

    // SAFETY: IoBufMut grants exclusively borrowed writable capacity. All
    // output fields are live initialized stack storage; null OVERLAPPED makes
    // this synchronous, so no pointers survive the call.
    let recv_result = unsafe {
        WinSock::WSARecvFrom(
            socket,
            &mut wsabuf,
            1,
            &mut bytes,
            &mut flags,
            (&mut addr as *mut SOCKADDR_STORAGE).cast::<SOCKADDR>(),
            &mut addr_len,
            std::ptr::null_mut(),
            None,
        )
    };
    if recv_result == SOCKET_ERROR {
        // SAFETY: retrieves the calling thread's Winsock error; takes no pointers.
        return Err(io::Error::from_raw_os_error(unsafe {
            WinSock::WSAGetLastError()
        }));
    }

    let address = sockaddr_storage_to_socketaddr(&addr, addr_len as usize)?;
    Ok((bytes as usize, address))
}

#[cfg(windows)]
struct RecvfromWindowsCompletion {
    socket_buf: WSABUF,
    addr: SOCKADDR_STORAGE,
    addr_len: i32,
    flags: u32,
}

#[cfg(target_os = "linux")]
struct RecvfromLinuxCompletion {
    addr: libc::sockaddr_storage,
    iovec: libc::iovec,
    msghdr: libc::msghdr,
}

pub struct RecvfromOp<'a, B: IoBufMut> {
    handle: &'a InnerRawHandle,
    buf: Option<CompletionBuffer<B>>,
    completion_token: Option<usize>,
    #[cfg(windows)]
    completion_state: Option<Box<RecvfromWindowsCompletion>>,
    #[cfg(target_os = "linux")]
    completion_state: Option<Box<RecvfromLinuxCompletion>>,
    peek: bool,
}

impl<'a, B: IoBufMut> RecvfromOp<'a, B> {
    #[inline]
    pub fn new(handle: &'a InnerRawHandle, buf: B) -> Self {
        Self {
            handle,
            buf: Some(CompletionBuffer::new(buf, handle.uses_completion())),
            completion_token: None,
            #[cfg(windows)]
            completion_state: None,
            #[cfg(target_os = "linux")]
            completion_state: None,
            peek: false,
        }
    }

    #[inline]
    pub fn new_peek(handle: &'a InnerRawHandle, buf: B) -> Self {
        Self {
            handle,
            buf: Some(CompletionBuffer::new(buf, handle.uses_completion())),
            completion_token: None,
            #[cfg(windows)]
            completion_state: None,
            #[cfg(target_os = "linux")]
            completion_state: None,
            peek: true,
        }
    }

    #[inline]
    pub fn take_bufs(mut self) -> B {
        assert!(
            self.completion_token.is_none(),
            "cannot reclaim a buffer while I/O is pending"
        );
        self.buf.take().unwrap().into_inner()
    }
}

impl<B: IoBufMut> Op for RecvfromOp<'_, B> {
    type Output = (usize, SocketAddr);

    #[cfg(any(unix, windows))]
    #[inline]
    fn poll_poll(
        &mut self,
        cx: &mut Context<'_>,
        driver: &AnyDriver,
    ) -> Poll<io::Result<Self::Output>> {
        let buf = self.buf.as_mut().unwrap().as_mut();

        #[cfg(unix)]
        let result = {
            let mut addr = MaybeUninit::<libc::sockaddr_storage>::zeroed();
            let mut addr_len = std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
            // SAFETY: IoBufMut provides exclusive writable capacity, and addr
            // and addr_len are live output storage of the supplied sizes. This
            // synchronous call retains no pointers and does not request MSG_TRUNC.
            let read = unsafe {
                libc::recvfrom(
                    self.handle.handle,
                    buf.as_buf_mut_ptr().cast::<libc::c_void>(),
                    buf.buf_capacity(),
                    if self.peek { libc::MSG_PEEK } else { 0 },
                    addr.as_mut_ptr().cast::<libc::sockaddr>(),
                    &mut addr_len,
                )
            };

            if read == -1 {
                Err(io::Error::last_os_error())
            } else {
                // SAFETY: storage was fully zero-initialized (all-zero integer
                // fields are valid); recvfrom only overwrites bytes within it.
                let addr = unsafe { addr.assume_init() };
                let address = sockaddr_storage_to_socketaddr(&addr, addr_len as usize)?;
                Ok((read as usize, address))
            }
        };

        #[cfg(windows)]
        let result = match self.handle.handle {
            RawOsHandle::Socket(socket) => socket_recvfrom(socket as SOCKET, buf, self.peek),
            RawOsHandle::Handle(_) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "poll-based recvfrom currently supports sockets only on Windows",
            )),
        };

        match result {
            Ok((read, address)) => {
                // SAFETY: successful recvfrom initialized exactly the reported
                // prefix of the supplied capacity; MSG_TRUNC was not requested.
                unsafe { buf.set_buf_init(read) };
                Poll::Ready(Ok((read, address)))
            }
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                match driver.submit_poll(self.handle, cx.waker().clone(), Interest::READABLE) {
                    Ok(_) => Poll::Pending,
                    Err(err) => Poll::Ready(Err(err)),
                }
            }
            Err(err) => Poll::Ready(Err(err)),
        }
    }

    #[cfg(any(unix, windows))]
    #[inline]
    fn poll_completion(
        &mut self,
        cx: &mut Context<'_>,
        driver: &AnyDriver,
    ) -> Poll<io::Result<Self::Output>> {
        let result = if let Some(completion_token) = self.completion_token {
            match driver.get_completion_result(completion_token) {
                Some(result) => {
                    self.completion_token = None;
                    result
                }
                None => {
                    driver.set_completion_waker(completion_token, cx.waker().clone());
                    return Poll::Pending;
                }
            }
        } else {
            match driver.submit_completion(self, cx.waker().clone()) {
                CompletionIoResult::Ok(result) => result,
                CompletionIoResult::Retry(token) => {
                    self.completion_token = Some(token);
                    return Poll::Pending;
                }
                CompletionIoResult::SubmitErr(err) => return Poll::Ready(Err(err)),
            }
        };
        if result < 0 {
            return Poll::Ready(Err(crate::vibeio::op::io_util::completion_error(result)));
        }
        #[cfg(any(target_os = "linux", windows))]
        let read = result as usize;

        #[cfg(target_os = "linux")]
        {
            let address = self
                .completion_state
                .as_ref()
                .ok_or_else(|| io::Error::other("recvfrom completion missing source address"))
                .and_then(|state| {
                    sockaddr_storage_to_socketaddr(&state.addr, state.msghdr.msg_namelen as usize)
                });
            let buf = self.buf.as_mut().unwrap().as_mut();
            // SAFETY: the successful CQE acknowledges initialization of this
            // many bytes in the retained stable buffer. No MSG_TRUNC was requested.
            unsafe { buf.set_buf_init(read) };
            Poll::Ready(address.map(|address| (read, address)))
        }

        #[cfg(all(unix, not(target_os = "linux")))]
        {
            Poll::Ready(Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "completion-based recvfrom is unsupported on this Unix platform",
            )))
        }

        #[cfg(windows)]
        {
            let address = self
                .completion_state
                .as_ref()
                .ok_or_else(|| {
                    io::Error::new(
                        io::ErrorKind::Other,
                        "recvfrom completion missing source address",
                    )
                })
                .and_then(|state| {
                    sockaddr_storage_to_socketaddr(&state.addr, state.addr_len as usize)
                });
            let buf = self.buf.as_mut().unwrap().as_mut();
            // SAFETY: the successful overlapped completion reports initialized
            // bytes within the WSABUF capacity retained through its acknowledgement.
            unsafe { buf.set_buf_init(read) };
            Poll::Ready(address.map(|address| (read, address)))
        }
    }

    #[cfg(windows)]
    #[inline]
    fn submit_windows(&mut self, overlapped: *mut OVERLAPPED) -> Result<(), io::Error> {
        let buf = self.buf.as_mut().unwrap().as_mut();
        let RawOsHandle::Socket(socket) = self.handle.handle else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "WSARecvFrom can be used only with sockets",
            ));
        };

        let read_len =
            crate::vibeio::op::io_util::completion_len(buf.buf_capacity()).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "read buffer is too large for Windows socket I/O",
                )
            })?;

        let completion = self.completion_state.get_or_insert_with(|| {
            Box::new(RecvfromWindowsCompletion {
                socket_buf: WSABUF {
                    len: 0,
                    buf: std::ptr::null_mut(),
                },
                addr: SOCKADDR_STORAGE::default(),
                addr_len: 0,
                flags: 0,
            })
        });
        completion.socket_buf.len = read_len;
        completion.socket_buf.buf = buf.as_buf_mut_ptr().cast();
        completion.addr_len = std::mem::size_of::<SOCKADDR_STORAGE>() as i32;
        completion.flags = if self.peek { MSG_PEEK as u32 } else { 0 };

        // SAFETY: the boxed completion state and CompletionBuffer have stable
        // addresses and retain all writable regions through completion. The
        // driver supplies live OVERLAPPED storage; Drop transfers operation
        // storage to that driver if cancellation precedes acknowledgement.
        let recv_result = unsafe {
            WinSock::WSARecvFrom(
                socket as SOCKET,
                &mut completion.socket_buf as *mut WSABUF,
                1,
                std::ptr::null_mut(),
                &mut completion.flags,
                (&mut completion.addr as *mut SOCKADDR_STORAGE).cast::<SOCKADDR>(),
                &mut completion.addr_len,
                overlapped,
                None,
            )
        };

        if recv_result == 0 {
            return Ok(());
        }

        // SAFETY: reads this thread's last Winsock error without pointers.
        let err = unsafe { WinSock::WSAGetLastError() };
        if err == WSA_IO_PENDING {
            Ok(())
        } else {
            self.completion_state = None;
            Err(io::Error::from_raw_os_error(err))
        }
    }

    #[cfg(target_os = "linux")]
    #[inline]
    fn build_completion_entry(
        &mut self,
        user_data: u64,
    ) -> Result<io_uring::squeue::Entry, io::Error> {
        use io_uring::{opcode, types};

        let buf = self.buf.as_mut().unwrap().as_mut();
        let completion = self.completion_state.get_or_insert_with(|| {
            Box::new(RecvfromLinuxCompletion {
                // SAFETY: sockaddr_storage consists of integer and byte fields,
                // for which all-zero initialization is valid.
                addr: unsafe { std::mem::zeroed() },
                iovec: libc::iovec {
                    iov_base: std::ptr::null_mut(),
                    iov_len: 0,
                },
                // SAFETY: msghdr contains integers and raw pointers, all valid
                // when zeroed. Its live pointers are installed after boxing.
                msghdr: unsafe { std::mem::zeroed() },
            })
        });
        // Reuse initialized address storage. The decoder checks the returned
        // length, and every input/output msghdr field is reset below.
        completion.iovec = libc::iovec {
            iov_base: buf.as_buf_mut_ptr().cast::<libc::c_void>(),
            iov_len: buf.buf_capacity(),
        };
        completion.msghdr.msg_name =
            &mut completion.addr as *mut libc::sockaddr_storage as *mut libc::c_void;
        completion.msghdr.msg_namelen =
            std::mem::size_of::<libc::sockaddr_storage>() as libc::socklen_t;
        completion.msghdr.msg_iov = &mut completion.iovec as *mut libc::iovec;
        completion.msghdr.msg_iovlen = 1;
        completion.msghdr.msg_control = std::ptr::null_mut();
        completion.msghdr.msg_controllen = 0;
        completion.msghdr.msg_flags = 0;

        let entry = opcode::RecvMsg::new(
            types::Fd(self.handle.handle),
            &mut completion.msghdr as *mut libc::msghdr,
        )
        .flags(if self.peek { libc::MSG_PEEK as u32 } else { 0 })
        .build()
        .user_data(user_data);

        Ok(entry)
    }
}

impl<B: IoBufMut> Drop for RecvfromOp<'_, B> {
    #[inline]
    fn drop(&mut self) {
        if let Some(token) = self.completion_token.take() {
            #[cfg(any(windows, target_os = "linux"))]
            let completion_state = self.completion_state.take();
            #[cfg(not(any(windows, target_os = "linux")))]
            let completion_state = ();
            // The owning driver, not the currently entered runtime, must retain
            // every kernel-visible allocation until completion is acknowledged.
            self.handle.cancel_completion(
                token,
                Box::new((
                    completion_state,
                    self.buf.take().map(CompletionBuffer::into_stable_box),
                )),
            );
        }
    }
}

#[cfg(test)]
mod cancellation_tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn recvmsg_completion_truncates_to_capacity_and_preserves_peek() {
        use std::os::fd::AsRawFd;
        use std::rc::Rc;
        use std::time::{Duration, Instant};
        let driver = match AnyDriver::new_uring_custom(io_uring::IoUring::builder()) {
            Ok(driver) => Rc::new(driver),
            Err(error)
                if matches!(
                    error.raw_os_error(),
                    Some(libc::EPERM | libc::ENOSYS | libc::EOPNOTSUPP)
                ) =>
            {
                eprintln!("io_uring unavailable: {error}");
                return;
            }
            Err(error) => panic!("io_uring initialization failed: {error}"),
        };
        for bind in ["127.0.0.1:0", "[::1]:0"] {
            let reader = std::net::UdpSocket::bind(bind).unwrap();
            let writer = std::net::UdpSocket::bind(bind).unwrap();
            reader.set_nonblocking(true).unwrap();
            let handle = InnerRawHandle::new_with_driver_and_mode(
                &driver,
                reader.as_raw_fd(),
                Interest::READABLE,
                crate::vibeio::driver::RegistrationMode::Completion,
            )
            .unwrap();
            let source = writer.local_addr().unwrap();
            let payload = [0x5a; 64];
            for capacity in [0, 8] {
                writer
                    .send_to(&payload, reader.local_addr().unwrap())
                    .unwrap();
                for peek in [true, false] {
                    let buffer = Vec::<u8>::with_capacity(capacity);
                    let expected = buffer.capacity().min(payload.len());
                    let mut op = if peek {
                        RecvfromOp::new_peek(&handle, buffer)
                    } else {
                        RecvfromOp::new(&handle, buffer)
                    };
                    let deadline = Instant::now() + Duration::from_secs(5);
                    let mut cx = Context::from_waker(std::task::Waker::noop());
                    let result = loop {
                        if let Poll::Ready(result) = op.poll_completion(&mut cx, &driver) {
                            break result.unwrap();
                        }
                        assert!(Instant::now() < deadline, "recvmsg completion timed out");
                        driver.wait(Some(Duration::from_millis(10)));
                    };
                    assert_eq!(result, (expected, source));
                    assert_eq!(op.take_bufs(), payload[..expected]);
                }
                assert_eq!(
                    reader.recv_from(&mut [0; 64]).unwrap_err().kind(),
                    io::ErrorKind::WouldBlock
                );
            }
        }
    }

    #[cfg(any(unix, windows))]
    #[test]
    fn polling_datagrams_preserve_peek_address_and_empty_packet() {
        use std::net::UdpSocket;
        #[cfg(unix)]
        use std::os::fd::AsRawFd;
        #[cfg(windows)]
        use std::os::windows::io::AsRawSocket;
        use std::rc::Rc;

        let reader = UdpSocket::bind("127.0.0.1:0").unwrap();
        let writer = UdpSocket::bind("127.0.0.1:0").unwrap();
        reader
            .set_read_timeout(Some(std::time::Duration::from_secs(5)))
            .unwrap();
        let destination = reader.local_addr().unwrap();
        let source = writer.local_addr().unwrap();
        let driver = Rc::new(AnyDriver::new_mock());
        let mut handle = InnerRawHandle::for_mock_completion(driver.clone());
        #[cfg(unix)]
        {
            handle.handle = reader.as_raw_fd();
        }
        #[cfg(windows)]
        {
            handle.handle = RawOsHandle::Socket(reader.as_raw_socket());
        }
        let mut cx = Context::from_waker(std::task::Waker::noop());
        let mut buffer = Vec::<u8>::with_capacity(32);

        for payload in [b"abc".as_slice(), b"", b"after empty"] {
            assert_eq!(writer.send_to(payload, destination).unwrap(), payload.len());
            for peek in [true, false] {
                let mut op = if peek {
                    RecvfromOp::new_peek(&handle, buffer)
                } else {
                    RecvfromOp::new(&handle, buffer)
                };
                assert!(
                    matches!(op.poll_poll(&mut cx, &driver), Poll::Ready(Ok((count, addr)))
                        if count == payload.len() && addr == source)
                );
                buffer = op.take_bufs();
                assert_eq!(buffer, payload);
            }
        }
        reader.set_nonblocking(true).unwrap();
        assert_eq!(
            reader.recv_from(&mut [0; 32]).unwrap_err().kind(),
            io::ErrorKind::WouldBlock
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn reused_recvmsg_state_resets_metadata_and_keeps_stable_addresses() {
        let driver = std::rc::Rc::new(AnyDriver::new_mock());
        let handle = InnerRawHandle::for_mock_completion(driver);
        let mut op = RecvfromOp::new(&handle, Vec::<u8>::with_capacity(32));
        op.build_completion_entry(1).unwrap();
        let state = op.completion_state.as_mut().unwrap();
        let state_address = std::ptr::from_ref(state.as_ref());
        let buffer_address = state.iovec.iov_base;
        let capacity = state.iovec.iov_len;
        // Model output fields changed by a completed receive. Rebuilding must
        // restore the supplied sizes and remove obsolete ancillary-data state.
        state.msghdr.msg_namelen = 0;
        state.msghdr.msg_iovlen = 0;
        state.msghdr.msg_flags = libc::MSG_TRUNC;
        state.msghdr.msg_control = std::ptr::addr_of_mut!(state.addr).cast();
        state.msghdr.msg_controllen = 1;
        op.build_completion_entry(2).unwrap();
        let state = op.completion_state.as_ref().unwrap();
        assert_eq!(std::ptr::from_ref(state.as_ref()), state_address);
        assert_eq!(state.iovec.iov_base, buffer_address);
        assert_eq!(state.iovec.iov_len, capacity);
        assert_eq!(
            state.msghdr.msg_namelen as usize,
            std::mem::size_of_val(&state.addr)
        );
        assert_eq!(
            state.msghdr.msg_name,
            std::ptr::addr_of!(state.addr).cast_mut().cast()
        );
        assert_eq!(
            state.msghdr.msg_iov,
            std::ptr::addr_of!(state.iovec).cast_mut()
        );
        assert_eq!(state.msghdr.msg_iovlen, 1);
        assert_eq!(state.msghdr.msg_flags, 0);
        assert!(state.msghdr.msg_control.is_null());
        assert_eq!(state.msghdr.msg_controllen, 0);
    }

    #[test]
    fn pending_buffer_is_retained_by_owning_driver() {
        crate::vibeio::op::io_util::cancellation_tests::check_cancellation(
            |handle, buffer, reclaim| {
                let mut op = RecvfromOp::new(handle, buffer);
                op.completion_token = Some(41);
                if reclaim {
                    let result =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| op.take_bufs()));
                    assert!(result.is_err(), "pending storage must not be reclaimed");
                } else {
                    drop(op);
                }
            },
        );
    }
}
