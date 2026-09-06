#![deny(unsafe_op_in_unsafe_fn)]
#![warn(clippy::undocumented_unsafe_blocks)]

use std::io;
use std::mem::MaybeUninit;
use std::net::SocketAddr;
use std::task::{Context, Poll};

use mio::Interest;
#[cfg(windows)]
use windows_sys::Win32::{
    Networking::WinSock::{
        self as WinSock, AF_INET, AF_INET6, MSG_PEEK, SOCKADDR, SOCKADDR_IN, SOCKADDR_IN6,
        SOCKADDR_STORAGE, SOCKET, WSA_IO_PENDING, WSABUF,
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

fn validate_address_length(length: usize, expected: usize, capacity: usize) -> io::Result<()> {
    if length < expected || length > capacity {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid source address length",
        ));
    }
    Ok(())
}

#[cfg(unix)]
#[inline]
fn sockaddr_storage_to_socketaddr(
    storage: &libc::sockaddr_storage,
    length: usize,
) -> Result<SocketAddr, io::Error> {
    let family = storage.ss_family as libc::c_int;

    if family == libc::AF_INET {
        validate_address_length(
            length,
            std::mem::size_of::<libc::sockaddr_in>(),
            std::mem::size_of_val(storage),
        )?;
        // SAFETY: sockaddr_storage has sufficient size/alignment for sockaddr_in;
        // the family and returned length establish that its IPv4 fields are present.
        let addr_in: &libc::sockaddr_in =
            unsafe { &*(storage as *const _ as *const libc::sockaddr_in) };
        let port = u16::from_be(addr_in.sin_port);
        let ip_u32 = u32::from_be(addr_in.sin_addr.s_addr);
        let ip = std::net::Ipv4Addr::from(ip_u32);
        Ok(SocketAddr::V4(std::net::SocketAddrV4::new(ip, port)))
    } else if family == libc::AF_INET6 {
        validate_address_length(
            length,
            std::mem::size_of::<libc::sockaddr_in6>(),
            std::mem::size_of_val(storage),
        )?;
        // SAFETY: storage is aligned/sized for sockaddr_in6, and both the family
        // and returned length have been checked before borrowing its fields.
        let addr_in6: &libc::sockaddr_in6 =
            unsafe { &*(storage as *const _ as *const libc::sockaddr_in6) };
        let port = u16::from_be(addr_in6.sin6_port);
        let ip = std::net::Ipv6Addr::from(addr_in6.sin6_addr.s6_addr);
        Ok(SocketAddr::V6(std::net::SocketAddrV6::new(
            ip,
            port,
            addr_in6.sin6_flowinfo,
            addr_in6.sin6_scope_id,
        )))
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported socket family",
        ))
    }
}

#[cfg(windows)]
#[inline]
fn sockaddr_storage_to_socketaddr(
    storage: &SOCKADDR_STORAGE,
    length: usize,
) -> Result<SocketAddr, io::Error> {
    let family = storage.ss_family;

    if family == AF_INET {
        validate_address_length(
            length,
            std::mem::size_of::<SOCKADDR_IN>(),
            std::mem::size_of_val(storage),
        )?;
        // SAFETY: storage is suitably sized/aligned, and the family/length match IPv4.
        let addr_in: &SOCKADDR_IN = unsafe { &*(storage as *const _ as *const SOCKADDR_IN) };
        let port = u16::from_be(addr_in.sin_port);
        // SAFETY: the IPv4 address union contains initialized network-order bytes.
        let ip_u32 = u32::from_be(unsafe { addr_in.sin_addr.S_un.S_addr });
        let ip = std::net::Ipv4Addr::from(ip_u32);
        Ok(SocketAddr::V4(std::net::SocketAddrV4::new(ip, port)))
    } else if family == AF_INET6 {
        validate_address_length(
            length,
            std::mem::size_of::<SOCKADDR_IN6>(),
            std::mem::size_of_val(storage),
        )?;
        // SAFETY: storage is suitably sized/aligned, and the family/length match IPv6.
        let addr_in6: &SOCKADDR_IN6 = unsafe { &*(storage as *const _ as *const SOCKADDR_IN6) };
        let port = u16::from_be(addr_in6.sin6_port);
        // SAFETY: the IPv6 address union contains sixteen initialized address bytes.
        let ip = std::net::Ipv6Addr::from(unsafe { addr_in6.sin6_addr.u.Byte });
        // SAFETY: the validated IPv6 structure includes the initialized scope union.
        let scope_id = unsafe { addr_in6.Anonymous.sin6_scope_id };
        Ok(SocketAddr::V6(std::net::SocketAddrV6::new(
            ip,
            port,
            addr_in6.sin6_flowinfo,
            scope_id,
        )))
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "unsupported socket family",
        ))
    }
}

#[cfg(windows)]
#[inline]
fn socket_recvfrom(
    socket: SOCKET,
    buf: &mut [MaybeUninit<u8>],
    peek: bool,
) -> io::Result<(usize, SocketAddr)> {
    use windows_sys::Win32::Networking::WinSock::{
        self as WinSock, MSG_PEEK, SOCKADDR, SOCKADDR_STORAGE, SOCKET_ERROR, WSABUF,
    };

    let len = u32::try_from(buf.len()).map_err(|_| {
        io::Error::new(
            io::ErrorKind::InvalidInput,
            "read buffer is too large for Windows socket I/O",
        )
    })?;

    let mut wsabuf = WSABUF {
        len,
        buf: buf.as_mut_ptr().cast(),
    };
    let mut bytes: u32 = 0;
    let mut flags: u32 = if peek { MSG_PEEK as u32 } else { 0 };
    let mut addr = SOCKADDR_STORAGE::default();
    let mut addr_len = std::mem::size_of::<SOCKADDR_STORAGE>() as i32;

    // SAFETY: wsabuf describes the exclusively borrowed writable slice. All
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
            RawOsHandle::Socket(socket) => {
                // SAFETY: IoBufMut guarantees exclusive writable capacity, but
                // not initialized bytes. The synchronous call retains no pointer.
                let slice = unsafe {
                    std::slice::from_raw_parts_mut(buf.as_buf_mut_ptr().cast(), buf.buf_capacity())
                };
                socket_recvfrom(socket as SOCKET, slice, self.peek)
            }
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
            return Poll::Ready(Err(io::Error::from_raw_os_error(-result)));
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

        let read_len = u32::try_from(buf.buf_capacity()).map_err(|_| {
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
    fn source_address_length_is_checked_before_decoding() {
        #[cfg(unix)]
        type Storage = libc::sockaddr_storage;
        #[cfg(windows)]
        type Storage = SOCKADDR_STORAGE;
        #[cfg(unix)]
        let families = [
            (libc::AF_INET, std::mem::size_of::<libc::sockaddr_in>()),
            (libc::AF_INET6, std::mem::size_of::<libc::sockaddr_in6>()),
        ];
        #[cfg(windows)]
        let families = [
            (AF_INET as i32, std::mem::size_of::<SOCKADDR_IN>()),
            (AF_INET6 as i32, std::mem::size_of::<SOCKADDR_IN6>()),
        ];
        for (index, (family, required)) in families.into_iter().enumerate() {
            // SAFETY: socket address storage consists of integer/byte fields;
            // zero initializes its entire storage before the family is assigned.
            let mut storage: Storage = unsafe { std::mem::zeroed() };
            storage.ss_family = family as _;
            let capacity = std::mem::size_of::<Storage>();
            for length in [0, required - 1, capacity + 1, usize::MAX] {
                assert_eq!(
                    sockaddr_storage_to_socketaddr(&storage, length)
                        .unwrap_err()
                        .kind(),
                    io::ErrorKind::InvalidData
                );
            }
            let address = sockaddr_storage_to_socketaddr(&storage, required).unwrap();
            assert_eq!(address.is_ipv4(), index == 0);
            assert!(address.ip().is_unspecified());
            assert_eq!(address.port(), 0);
        }
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
