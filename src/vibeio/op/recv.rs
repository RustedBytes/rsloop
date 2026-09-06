#![warn(clippy::undocumented_unsafe_blocks)]

use std::io;
use std::task::{Context, Poll};

use mio::Interest;
#[cfg(windows)]
use windows_sys::Win32::{
    Networking::WinSock::{self, MSG_PEEK, SOCKET, WSA_IO_PENDING, WSABUF},
    System::IO::OVERLAPPED,
};

use crate::vibeio::driver::AnyDriver;
use crate::vibeio::driver::CompletionIoResult;
use crate::vibeio::fd_inner::InnerRawHandle;
#[cfg(windows)]
use crate::vibeio::fd_inner::RawOsHandle;
use crate::vibeio::io::IoBufMut;
use crate::vibeio::op::Op;
#[cfg(target_os = "linux")]
use crate::vibeio::op::io_util::completion_len;
use crate::vibeio::op::io_util::{CompletionBuffer, poll_result_or_wait};

#[cfg(windows)]
#[inline]
fn socket_recv(socket: SOCKET, buf: &mut impl IoBufMut, peek: bool) -> io::Result<usize> {
    use windows_sys::Win32::Networking::WinSock::{
        self as WinSock, MSG_PEEK, SOCKET_ERROR, WSABUF,
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

    // SAFETY: IoBufMut provides exclusive writable capacity. Descriptor and
    // output values live for this synchronous call; null OVERLAPPED means
    // none of these pointers is retained after return.
    let recv_result = unsafe {
        WinSock::WSARecv(
            socket,
            &mut wsabuf,
            1,
            &mut bytes,
            &mut flags,
            std::ptr::null_mut(),
            None,
        )
    };
    if recv_result == SOCKET_ERROR {
        // SAFETY: queries the calling thread's Winsock error without pointers.
        return Err(io::Error::from_raw_os_error(unsafe {
            WinSock::WSAGetLastError()
        }));
    }

    Ok(bytes as usize)
}

pub struct RecvOp<'a, B: IoBufMut> {
    handle: &'a InnerRawHandle,
    buf: Option<CompletionBuffer<B>>,
    completion_token: Option<usize>,
    peek: bool,
}

impl<'a, B: IoBufMut> RecvOp<'a, B> {
    #[inline]
    pub fn new(handle: &'a InnerRawHandle, buf: B) -> Self {
        Self {
            handle,
            buf: Some(CompletionBuffer::new(buf, handle.uses_completion())),
            completion_token: None,
            peek: false,
        }
    }

    pub fn new_peek(handle: &'a InnerRawHandle, buf: B) -> Self {
        Self {
            handle,
            buf: Some(CompletionBuffer::new(buf, handle.uses_completion())),
            completion_token: None,
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

impl<B: IoBufMut> Op for RecvOp<'_, B> {
    type Output = usize;

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
            // SAFETY: the socket is live and IoBufMut provides exclusive
            // writable capacity; recv does not retain the pointer after return.
            let read = unsafe {
                libc::recv(
                    self.handle.handle,
                    buf.as_buf_mut_ptr().cast::<libc::c_void>(),
                    buf.buf_capacity(),
                    if self.peek { libc::MSG_PEEK } else { 0 },
                )
            };
            if read == -1 {
                Err(io::Error::last_os_error())
            } else {
                Ok(read as usize)
            }
        };

        #[cfg(windows)]
        let result = match self.handle.handle {
            RawOsHandle::Socket(socket) => socket_recv(socket as SOCKET, buf, self.peek),
            RawOsHandle::Handle(_) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "poll-based recv currently supports sockets only on Windows",
            )),
        };
        match poll_result_or_wait(result, self.handle, cx, driver, Interest::READABLE) {
            Poll::Ready(Ok(read)) => {
                // SAFETY: the successful synchronous receive initialized the
                // reported prefix within the supplied writable capacity.
                unsafe { buf.set_buf_init(read) };
                Poll::Ready(Ok(read))
            }
            Poll::Ready(Err(e)) => Poll::Ready(Err(e)),
            Poll::Pending => Poll::Pending,
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
            // Get the completion result
            match driver.get_completion_result(completion_token) {
                Some(result) => {
                    self.completion_token = None;
                    result
                }
                None => {
                    // The completion is not ready yet
                    driver.set_completion_waker(completion_token, cx.waker().clone());
                    return Poll::Pending;
                }
            }
        } else {
            // Submit the op
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
        let read = result as usize;
        let buf = self.buf.as_mut().unwrap().as_mut();
        // SAFETY: completion reports the initialized prefix of the submitted
        // writable capacity; pending storage remained owned by CompletionBuffer.
        unsafe { buf.set_buf_init(read) };
        Poll::Ready(Ok(read))
    }

    #[cfg(windows)]
    #[inline]
    fn submit_windows(&mut self, overlapped: *mut OVERLAPPED) -> Result<(), io::Error> {
        let buf = self.buf.as_mut().unwrap().as_mut();
        let RawOsHandle::Socket(socket) = self.handle.handle else {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "WSARecv requires a socket handle",
            ));
        };

        let read_len =
            crate::vibeio::op::io_util::completion_len(buf.buf_capacity()).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "read buffer is too large for Windows socket I/O",
                )
            })?;

        let mut wsabuf = WSABUF {
            len: read_len,
            buf: buf.as_buf_mut_ptr().cast(),
        };

        let mut flags: u32 = if self.peek { MSG_PEEK as u32 } else { 0 };
        // SAFETY: WSARecv captures the descriptor during this call and does not
        // update flags on delayed completion. Payload/OVERLAPPED stay retained.
        // https://learn.microsoft.com/en-us/windows/win32/api/winsock2/nf-winsock2-wsarecv
        let recv_result = unsafe {
            WinSock::WSARecv(
                socket as SOCKET,
                &mut wsabuf,
                1,
                std::ptr::null_mut(),
                &mut flags,
                overlapped,
                None,
            )
        };

        if recv_result == 0 {
            return Ok(());
        }

        // SAFETY: queries the calling thread's Winsock error without pointers.
        let err = unsafe { WinSock::WSAGetLastError() };
        if err == WSA_IO_PENDING {
            Ok(())
        } else {
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
        let transfer_len = completion_len(buf.buf_capacity())?;
        let entry = opcode::Recv::new(
            types::Fd(self.handle.handle),
            buf.as_buf_mut_ptr(),
            transfer_len,
        )
        .flags(if self.peek { libc::MSG_PEEK } else { 0 })
        .build()
        .user_data(user_data);

        Ok(entry)
    }
}

impl<B: IoBufMut> Drop for RecvOp<'_, B> {
    #[inline]
    fn drop(&mut self) {
        if let Some(token) = self.completion_token.take() {
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

    #[test]
    fn pending_buffer_is_retained_by_owning_driver() {
        crate::vibeio::op::io_util::cancellation_tests::check_cancellation(
            |handle, buffer, reclaim| {
                let mut op = RecvOp::new(handle, buffer);
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
