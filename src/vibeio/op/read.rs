#![deny(unsafe_op_in_unsafe_fn)]
#![warn(clippy::undocumented_unsafe_blocks)]

use std::io;
use std::task::{Context, Poll};

use mio::Interest;
#[cfg(windows)]
use windows_sys::Win32::{
    Foundation::{ERROR_HANDLE_EOF, ERROR_IO_PENDING, HANDLE},
    Networking::WinSock::{self, SOCKET, WSA_IO_PENDING, WSABUF},
    Storage::FileSystem::ReadFile,
    System::IO::OVERLAPPED,
};

use crate::vibeio::driver::AnyDriver;
use crate::vibeio::driver::CompletionIoResult;
use crate::vibeio::fd_inner::InnerRawHandle;
#[cfg(windows)]
use crate::vibeio::fd_inner::RawOsHandle;
use crate::vibeio::op::Op;
use crate::vibeio::op::io_util::{CompletionBuffer, completion_len, poll_result_or_wait};

#[cfg(windows)]
#[inline]
fn socket_read(socket: SOCKET, buf: &mut [std::mem::MaybeUninit<u8>]) -> io::Result<usize> {
    use windows_sys::Win32::Networking::WinSock::{self as WinSock, SOCKET_ERROR, WSABUF};

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
    let mut flags: u32 = 0;

    // SAFETY: wsabuf describes exclusive writable MaybeUninit bytes. All output
    // locals remain live during this synchronous, null-OVERLAPPED call.
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
        // SAFETY: reads this thread's Winsock error without pointer arguments.
        return Err(io::Error::from_raw_os_error(unsafe {
            WinSock::WSAGetLastError()
        }));
    }

    Ok(bytes as usize)
}

use crate::vibeio::io::IoBufMut;

pub struct ReadOp<'a, B: IoBufMut> {
    handle: &'a InnerRawHandle,
    buf: Option<CompletionBuffer<B>>,
    completion_token: Option<usize>,
}

impl<'a, B: IoBufMut> ReadOp<'a, B> {
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

impl<B: IoBufMut> Op for ReadOp<'_, B> {
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
            // SAFETY: IoBufMut grants exclusive writable capacity. read writes
            // at most that capacity and retains no pointer after returning.
            let read = unsafe {
                libc::read(
                    self.handle.handle,
                    buf.as_buf_mut_ptr().cast::<libc::c_void>(),
                    buf.buf_capacity(),
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
            RawOsHandle::Socket(socket) => {
                // SAFETY: IoBufMut guarantees exclusive writable capacity, but
                // not initialized bytes. The synchronous call retains no pointer.
                let slice = unsafe {
                    std::slice::from_raw_parts_mut(buf.as_buf_mut_ptr().cast(), buf.buf_capacity())
                };
                socket_read(socket as SOCKET, slice)
            }
            RawOsHandle::Handle(_) => Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "poll-based read currently supports sockets only on Windows",
            )),
        };

        match poll_result_or_wait(result, self.handle, cx, driver, Interest::READABLE) {
            Poll::Ready(Ok(read)) => {
                // SAFETY: successful synchronous read initialized the reported
                // prefix within the supplied buffer capacity.
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
            #[cfg(windows)]
            if -result == ERROR_HANDLE_EOF as i32 {
                let buf = self.buf.as_mut().unwrap().as_mut();
                // SAFETY: the empty prefix is initialized for every buffer.
                unsafe { buf.set_buf_init(0) };
                return Poll::Ready(Ok(0));
            }
            return Poll::Ready(Err(io::Error::from_raw_os_error(-result)));
        }
        let read = result as usize;
        let buf = self.buf.as_mut().unwrap().as_mut();
        // SAFETY: the successful completion acknowledges this initialized prefix
        // within the stable buffer retained through the read operation.
        unsafe { buf.set_buf_init(read) };
        Poll::Ready(Ok(read))
    }

    #[cfg(windows)]
    #[inline]
    fn submit_windows(&mut self, overlapped: *mut OVERLAPPED) -> Result<(), io::Error> {
        let buf = self.buf.as_mut().unwrap().as_mut();
        match self.handle.handle {
            RawOsHandle::Socket(socket) => {
                let read_len = completion_len(buf.buf_capacity())?;

                let mut wsabuf = WSABUF {
                    len: read_len,
                    buf: buf.as_buf_mut_ptr().cast(),
                };
                let mut flags = 0;
                // SAFETY: WSARecv captures WSABUF before returning; delayed
                // completion does not update flags. The payload and driver-owned
                // OVERLAPPED remain retained through acknowledgement.
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

                // SAFETY: reads this thread's last Winsock error without pointers.
                let err = unsafe { WinSock::WSAGetLastError() };
                if err == WSA_IO_PENDING {
                    Ok(())
                } else {
                    Err(io::Error::from_raw_os_error(err))
                }
            }
            RawOsHandle::Handle(handle) => {
                let read_len = completion_len(buf.buf_capacity())?;

                // SAFETY: IoBufMut provides writable capacity retained through
                // completion/cancellation; the driver owns live OVERLAPPED storage.
                let read_result = unsafe {
                    ReadFile(
                        handle as HANDLE,
                        buf.as_buf_mut_ptr().cast(),
                        read_len,
                        std::ptr::null_mut(),
                        overlapped,
                    )
                };

                if read_result != 0 {
                    return Ok(());
                }

                let err = io::Error::last_os_error();
                if err.raw_os_error() == Some(ERROR_IO_PENDING as i32) {
                    Ok(())
                } else {
                    Err(err)
                }
            }
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
        let read_len = completion_len(buf.buf_capacity())?;
        let entry = opcode::Read::new(
            types::Fd(self.handle.handle),
            buf.as_buf_mut_ptr(),
            read_len,
        )
        .build()
        .user_data(user_data);

        Ok(entry)
    }
}

impl<B: IoBufMut> Drop for ReadOp<'_, B> {
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

    #[cfg(unix)]
    #[test]
    fn short_read_eof_and_error_preserve_initialized_prefix_contract() {
        use std::io::Write;
        use std::os::fd::AsRawFd;
        use std::rc::Rc;
        let driver = Rc::new(AnyDriver::new_mock());
        let (reader, mut writer) = std::io::pipe().unwrap();
        writer.write_all(b"abc").unwrap();
        drop(writer);
        let mut handle = InnerRawHandle::for_mock_completion(driver.clone());
        handle.handle = reader.as_raw_fd();
        let mut cx = Context::from_waker(std::task::Waker::noop());
        let mut op = ReadOp::new(&handle, Vec::<u8>::with_capacity(32));
        assert!(matches!(op.poll_poll(&mut cx, &driver), Poll::Ready(Ok(3))));
        let buffer = op.take_bufs();
        assert_eq!(buffer, b"abc");
        let mut op = ReadOp::new(&handle, buffer);
        assert!(matches!(op.poll_poll(&mut cx, &driver), Poll::Ready(Ok(0))));
        assert!(op.take_bufs().is_empty());

        let invalid = InnerRawHandle::for_mock_completion(driver.clone());
        let mut op = ReadOp::new(&invalid, b"unchanged".to_vec());
        assert!(
            matches!(op.poll_poll(&mut cx, &driver), Poll::Ready(Err(error)) if error.raw_os_error() == Some(libc::EBADF))
        );
        assert_eq!(op.take_bufs(), b"unchanged");
    }

    #[test]
    fn pending_buffer_is_retained_by_owning_driver() {
        crate::vibeio::op::io_util::cancellation_tests::check_cancellation(
            |handle, buffer, reclaim| {
                let mut op = ReadOp::new(handle, buffer);
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
