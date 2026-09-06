#![warn(clippy::undocumented_unsafe_blocks)]

use std::io;
use std::task::{Context, Poll};

use mio::Interest;
#[cfg(windows)]
use windows_sys::Win32::{
    Networking::WinSock::{self as WinSock, SOCKET, WSA_IO_PENDING, WSABUF},
    System::IO::OVERLAPPED,
};

use crate::vibeio::driver::AnyDriver;
use crate::vibeio::driver::CompletionIoResult;
use crate::vibeio::fd_inner::InnerRawHandle;
#[cfg(windows)]
use crate::vibeio::fd_inner::RawOsHandle;
use crate::vibeio::io::IoBuf;
use crate::vibeio::op::Op;
#[cfg(target_os = "linux")]
use crate::vibeio::op::io_util::completion_len;
use crate::vibeio::op::io_util::{CompletionBuffer, poll_result_or_wait};

#[cfg(windows)]
#[inline]
fn socket_send<B: IoBuf>(socket: SOCKET, buf: &B) -> io::Result<usize> {
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
    let mut bytes: u32 = 0;

    // SAFETY: the descriptor references the initialized IoBuf prefix for this
    // synchronous call. Count output and metadata are live stack values; null
    // OVERLAPPED means Winsock does not retain them after returning.
    let send_result = unsafe {
        WinSock::WSASend(
            socket,
            &mut wsabuf,
            1,
            &mut bytes,
            0,
            std::ptr::null_mut(),
            None,
        )
    };
    if send_result == SOCKET_ERROR {
        // SAFETY: queries the calling thread's Winsock error without pointers.
        return Err(io::Error::from_raw_os_error(unsafe {
            WinSock::WSAGetLastError()
        }));
    }

    Ok(bytes as usize)
}

pub struct SendOp<'a, B: IoBuf> {
    handle: &'a InnerRawHandle,
    buf: Option<CompletionBuffer<B>>,
    completion_token: Option<usize>,
}

impl<'a, B: IoBuf> SendOp<'a, B> {
    #[inline]
    pub fn new(handle: &'a InnerRawHandle, buf: B) -> Self {
        Self {
            handle,
            buf: Some(CompletionBuffer::new(buf, handle.uses_completion())),
            completion_token: None,
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

impl<B: IoBuf> Op for SendOp<'_, B> {
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
            // SAFETY: the borrowed socket is live and IoBuf owns the initialized
            // prefix for this synchronous send. The kernel retains no pointer.
            let written = unsafe {
                libc::send(
                    self.handle.handle,
                    buf.as_buf_ptr().cast::<libc::c_void>(),
                    buf.buf_len(),
                    0,
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
            RawOsHandle::Socket(socket) => socket_send(socket as SOCKET, buf),
            RawOsHandle::Handle(_) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "poll-based send currently supports sockets only on Windows",
            )),
        };

        match poll_result_or_wait(result, self.handle, cx, driver, Interest::WRITABLE) {
            Poll::Ready(Ok(written)) => Poll::Ready(Ok(written)),
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
                "WSASend can be used only with sockets",
            ));
        };

        let write_len =
            crate::vibeio::op::io_util::completion_len(buf.buf_len()).map_err(|_| {
                io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "write buffer is too large for Windows socket I/O",
                )
            })?;

        let mut wsabuf = WSABUF {
            len: write_len,
            buf: buf.as_buf_ptr().cast_mut().cast(),
        };

        // SAFETY: Winsock captures WSABUF metadata before returning, allowing
        // stack descriptors. The payload remains owned by CompletionBuffer and
        // is retained on cancellation; the driver owns the live OVERLAPPED until
        // completion acknowledgement. Neither payload nor OVERLAPPED is local.
        // https://learn.microsoft.com/en-us/windows/win32/api/winsock2/nf-winsock2-wsasend
        let send_result = unsafe {
            WinSock::WSASend(
                socket as SOCKET,
                &mut wsabuf,
                1,
                std::ptr::null_mut(),
                0,
                overlapped,
                None,
            )
        };

        if send_result == 0 {
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

        let buf = self.buf.as_ref().unwrap().as_ref();
        let transfer_len = completion_len(buf.buf_len())?;
        let entry = opcode::Send::new(
            types::Fd(self.handle.handle),
            buf.as_buf_ptr(),
            transfer_len,
        )
        .build()
        .user_data(user_data);

        Ok(entry)
    }
}

impl<B: IoBuf> Drop for SendOp<'_, B> {
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
                let mut op = SendOp::new(handle, buffer);
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
