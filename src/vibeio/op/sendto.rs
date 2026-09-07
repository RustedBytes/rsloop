#![deny(unsafe_op_in_unsafe_fn)]
#![warn(clippy::undocumented_unsafe_blocks)]

use std::io;
use std::net::SocketAddr;
use std::task::{Context, Poll};

use mio::Interest;
#[cfg(windows)]
use windows_sys::Win32::{
    Networking::WinSock::{
        self as WinSock, SOCKADDR, SOCKADDR_STORAGE, SOCKET, WSA_IO_PENDING, WSABUF,
    },
    System::IO::OVERLAPPED,
};

use crate::vibeio::driver::AnyDriver;
use crate::vibeio::driver::CompletionIoResult;
use crate::vibeio::fd_inner::InnerRawHandle;
#[cfg(windows)]
use crate::vibeio::fd_inner::RawOsHandle;
use crate::vibeio::io::IoBuf;
use crate::vibeio::op::io_util::{CompletionBuffer, poll_result_or_wait};
use crate::vibeio::op::{Op, socket_addr_to_raw};

#[cfg(windows)]
#[inline]
fn socket_sendto<B: IoBuf>(socket: SOCKET, buf: &B, addr: SocketAddr) -> io::Result<usize> {
    use windows_sys::Win32::Networking::WinSock::{self as WinSock, SOCKET_ERROR, WSABUF};

    let len = crate::vibeio::op::io_util::completion_len(buf.buf_len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "write buffer is too large for Windows socket I/O",
        )
    })?;

    let mut wsabuf = WSABUF {
        len,
        buf: buf.as_buf_ptr().cast_mut().cast(),
    };
    let (raw_addr, raw_addr_len) = socket_addr_to_raw(addr);
    let mut bytes: u32 = 0;

    // SAFETY: IoBuf supplies a live initialized payload; raw_addr and all output
    // locals remain live through this synchronous, null-OVERLAPPED call.
    let send_result = unsafe {
        WinSock::WSASendTo(
            socket,
            &mut wsabuf,
            1,
            &mut bytes,
            0,
            (&raw_addr as *const SOCKADDR_STORAGE).cast::<SOCKADDR>(),
            raw_addr_len,
            std::ptr::null_mut(),
            None,
        )
    };
    if send_result == SOCKET_ERROR {
        // SAFETY: reads the calling thread's Winsock error without pointers.
        return Err(io::Error::from_raw_os_error(unsafe {
            WinSock::WSAGetLastError()
        }));
    }

    Ok(bytes as usize)
}

#[cfg(windows)]
struct SendtoWindowsCompletion {
    socket_buf: WSABUF,
    addr: SOCKADDR_STORAGE,
    addr_len: i32,
}

#[cfg(target_os = "linux")]
struct SendtoLinuxCompletion {
    addr: libc::sockaddr_storage,
    addr_len: libc::socklen_t,
    iovec: libc::iovec,
    msghdr: libc::msghdr,
}

pub struct SendtoOp<'a, B: IoBuf> {
    handle: &'a InnerRawHandle,
    buf: Option<CompletionBuffer<B>>,
    addr: SocketAddr,
    completion_token: Option<usize>,
    #[cfg(windows)]
    completion_state: Option<Box<SendtoWindowsCompletion>>,
    #[cfg(target_os = "linux")]
    completion_state: Option<Box<SendtoLinuxCompletion>>,
}

impl<'a, B: IoBuf> SendtoOp<'a, B> {
    #[inline]
    pub fn new(handle: &'a InnerRawHandle, buf: B, addr: SocketAddr) -> Self {
        Self {
            handle,
            buf: Some(CompletionBuffer::new(buf, handle.uses_completion())),
            addr,
            completion_token: None,
            #[cfg(windows)]
            completion_state: None,
            #[cfg(target_os = "linux")]
            completion_state: None,
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

impl<B: IoBuf> Op for SendtoOp<'_, B> {
    type Output = usize;

    #[cfg(any(unix, windows))]
    #[inline]
    fn poll_poll(
        &mut self,
        cx: &mut Context<'_>,
        driver: &AnyDriver,
    ) -> Poll<io::Result<Self::Output>> {
        let buf = self.buf.as_ref().unwrap().as_ref();

        #[cfg(unix)]
        let result = {
            let (raw_addr, raw_addr_len) = socket_addr_to_raw(self.addr);
            // SAFETY: IoBuf provides initialized readable bytes and raw_addr is
            // live address storage of the supplied size; sendto retains no pointers.
            let written = unsafe {
                libc::sendto(
                    self.handle.handle,
                    buf.as_buf_ptr().cast::<libc::c_void>(),
                    buf.buf_len(),
                    0,
                    (&raw_addr as *const libc::sockaddr_storage).cast::<libc::sockaddr>(),
                    raw_addr_len,
                )
            };
            if written == -1 {
                Err(io::Error::last_os_error())
            } else {
                Ok(written as usize)
            }
        };

        #[cfg(windows)]
        let result = match self.handle.handle {
            RawOsHandle::Socket(socket) => socket_sendto(socket as SOCKET, buf, self.addr),
            RawOsHandle::Handle(_) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "poll-based sendto currently supports sockets only on Windows",
            )),
        };

        poll_result_or_wait(result, self.handle, cx, driver, Interest::WRITABLE)
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
        let written = result as usize;
        Poll::Ready(Ok(written))
    }

    #[cfg(windows)]
    #[inline]
    fn submit_windows(&mut self, overlapped: *mut OVERLAPPED) -> Result<(), io::Error> {
        let buf = self.buf.as_ref().unwrap().as_ref();
        let RawOsHandle::Socket(socket) = self.handle.handle else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "WSASendTo can be used only with sockets",
            ));
        };

        let write_len =
            crate::vibeio::op::io_util::completion_len(buf.buf_len()).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "write buffer is too large for Windows socket I/O",
                )
            })?;

        let (raw_addr, raw_addr_len) = socket_addr_to_raw(self.addr);
        let completion = self.completion_state.get_or_insert_with(|| {
            Box::new(SendtoWindowsCompletion {
                socket_buf: WSABUF {
                    len: 0,
                    buf: std::ptr::null_mut(),
                },
                addr: SOCKADDR_STORAGE::default(),
                addr_len: 0,
            })
        });
        completion.socket_buf.len = write_len;
        completion.socket_buf.buf = buf.as_buf_ptr().cast_mut().cast();
        completion.addr = raw_addr;
        completion.addr_len = raw_addr_len;

        // SAFETY: boxed metadata and the stable payload remain owned through
        // completion, including cancellation retention in Drop. The driver
        // supplies OVERLAPPED storage that lives until acknowledgement.
        let send_result = unsafe {
            WinSock::WSASendTo(
                socket as SOCKET,
                &mut completion.socket_buf as *mut WSABUF,
                1,
                std::ptr::null_mut(),
                0,
                (&completion.addr as *const SOCKADDR_STORAGE).cast::<SOCKADDR>(),
                completion.addr_len,
                overlapped,
                None,
            )
        };

        if send_result == 0 {
            return Ok(());
        }

        // SAFETY: reads this thread's last Winsock error without pointer arguments.
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

        let (raw_addr, raw_addr_len) = socket_addr_to_raw(self.addr);
        let buf = self.buf.as_ref().unwrap().as_ref();
        let completion = self.completion_state.get_or_insert_with(|| {
            Box::new(SendtoLinuxCompletion {
                addr: raw_addr,
                addr_len: raw_addr_len,
                iovec: libc::iovec {
                    iov_base: std::ptr::null_mut(),
                    iov_len: 0,
                },
                // SAFETY: msghdr contains integer fields and raw pointers valid
                // when zeroed. Stable pointers are installed after boxing below.
                msghdr: unsafe { std::mem::zeroed() },
            })
        });
        completion.addr = raw_addr;
        completion.addr_len = raw_addr_len;
        completion.iovec = libc::iovec {
            iov_base: buf.as_buf_ptr().cast_mut().cast::<libc::c_void>(),
            iov_len: buf.buf_len(),
        };

        // Reset every header field, including output flags, when reusing state.
        completion.msghdr.msg_name =
            &mut completion.addr as *mut libc::sockaddr_storage as *mut libc::c_void;
        completion.msghdr.msg_namelen = completion.addr_len;
        completion.msghdr.msg_iov = &mut completion.iovec as *mut libc::iovec;
        completion.msghdr.msg_iovlen = 1;
        completion.msghdr.msg_control = std::ptr::null_mut();
        completion.msghdr.msg_controllen = 0;
        completion.msghdr.msg_flags = 0;

        let entry = opcode::SendMsg::new(
            types::Fd(self.handle.handle),
            &completion.msghdr as *const libc::msghdr,
        )
        .build()
        .user_data(user_data);

        Ok(entry)
    }
}

impl<B: IoBuf> Drop for SendtoOp<'_, B> {
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
    fn rebuilt_send_header_resets_metadata_and_preserves_storage() {
        let driver = std::rc::Rc::new(AnyDriver::new_mock());
        let handle = InnerRawHandle::for_mock_completion(driver);
        let mut op = SendtoOp::new(&handle, vec![1, 2, 3], "127.0.0.1:1234".parse().unwrap());
        op.build_completion_entry(1).unwrap();
        let state = op.completion_state.as_mut().unwrap();
        let state_pointer = std::ptr::from_ref(state.as_ref());
        let payload_pointer = state.iovec.iov_base;
        state.msghdr.msg_name = std::ptr::null_mut();
        state.msghdr.msg_namelen = 0;
        state.msghdr.msg_iov = std::ptr::null_mut();
        state.msghdr.msg_iovlen = 0;
        state.msghdr.msg_control = std::ptr::addr_of_mut!(state.addr).cast();
        state.msghdr.msg_controllen = 1;
        state.msghdr.msg_flags = libc::MSG_TRUNC;
        op.build_completion_entry(2).unwrap();
        let state = op.completion_state.as_ref().unwrap();
        assert_eq!(std::ptr::from_ref(state.as_ref()), state_pointer);
        assert_eq!(state.iovec.iov_base, payload_pointer);
        assert_eq!(state.iovec.iov_len, 3);
        assert_eq!(
            state.msghdr.msg_name,
            std::ptr::addr_of!(state.addr).cast_mut().cast()
        );
        assert_eq!(state.msghdr.msg_namelen, state.addr_len);
        assert_eq!(
            state.msghdr.msg_iov,
            std::ptr::addr_of!(state.iovec).cast_mut()
        );
        assert_eq!(state.msghdr.msg_iovlen, 1);
        assert!(state.msghdr.msg_control.is_null());
        assert_eq!(state.msghdr.msg_controllen, 0);
        assert_eq!(state.msghdr.msg_flags, 0);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn completion_send_delivers_datagram_without_ancillary_data() {
        use std::os::fd::AsRawFd;
        use std::rc::Rc;
        let receiver = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        receiver
            .set_read_timeout(Some(crate::vibeio::test_support::WATCHDOG))
            .unwrap();
        let sender = std::net::UdpSocket::bind("127.0.0.1:0").unwrap();
        let mut handle = InnerRawHandle::for_mock_completion(Rc::new(AnyDriver::new_mock()));
        handle.handle = sender.as_raw_fd();
        for payload in [b"datagram".as_slice(), b"".as_slice()] {
            let mut op = SendtoOp::new(&handle, payload.to_vec(), receiver.local_addr().unwrap());
            let entry = op.build_completion_entry(17).unwrap();
            let mut ring = io_uring::IoUring::new(2).unwrap();
            // SAFETY: sender, the boxed message metadata and the payload remain
            // owned and unchanged until the send CQE is observed below.
            unsafe { ring.submission().push(&entry).unwrap() };
            ring.submit_and_wait(1).unwrap();
            let completion = ring.completion().next().unwrap();
            assert_eq!(completion.user_data(), 17);
            assert_eq!(completion.result(), payload.len() as i32);
            let mut received = [0u8; 32];
            let (length, address) = receiver.recv_from(&mut received).unwrap();
            assert_eq!(&received[..length], payload);
            assert_eq!(address, sender.local_addr().unwrap());
        }
    }

    #[test]
    fn pending_buffer_is_retained_by_owning_driver() {
        crate::vibeio::op::io_util::cancellation_tests::check_cancellation(
            |handle, buffer, reclaim| {
                let mut op = SendtoOp::new(handle, buffer, "127.0.0.1:1234".parse().unwrap());
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
