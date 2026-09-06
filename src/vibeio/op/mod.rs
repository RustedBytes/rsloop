mod accept;
#[cfg(unix)]
mod accept_unix;
mod connect;
#[cfg(all(target_os = "linux", feature = "fs"))]
mod fsync;
#[cfg(all(target_os = "linux", feature = "fs"))]
mod hard_link;
mod io_util;
#[cfg(all(target_os = "linux", feature = "fs"))]
mod mkdir;
#[cfg(all(target_os = "linux", feature = "fs"))]
mod open;
mod read;
#[cfg(feature = "fs")]
mod readat;
mod readiness;
mod readv;
mod recv;
mod recvfrom;
#[cfg(all(target_os = "linux", feature = "fs"))]
mod rename;
mod send;
mod sendto;
mod socket_addr;
#[cfg(all(target_os = "linux", feature = "splice"))]
mod splice;
#[cfg(all(
    target_os = "linux",
    any(target_env = "gnu", musl_v1_2_3),
    feature = "fs"
))]
mod statx; // musl libc 1.1.x doesn't support statx
#[cfg(all(target_os = "linux", feature = "fs"))]
mod symlink;
#[cfg(all(target_os = "linux", feature = "fs"))]
mod unlink;
#[cfg(all(target_os = "linux", feature = "process"))]
mod waitpid;
mod write;
#[cfg(feature = "fs")]
mod writeat;
mod writev;

use std::io;
use std::task::Context;
use std::task::Poll;

pub use accept::AcceptOp;
#[cfg(unix)]
pub use accept_unix::AcceptUnixOp;
pub use connect::ConnectOp;
#[cfg(all(target_os = "linux", feature = "fs"))]
pub use fsync::FsyncOp;
#[cfg(all(target_os = "linux", feature = "fs"))]
pub use hard_link::HardLinkOp;
#[cfg(all(target_os = "linux", feature = "fs"))]
pub use mkdir::MkDirOp;
#[cfg(all(target_os = "linux", feature = "fs"))]
pub use open::OpenOp;
pub use read::ReadOp;
#[cfg(feature = "fs")]
pub use readat::ReadAtOp;
pub use readiness::ReadinessOp;
pub use readv::ReadvOp;
pub use recv::RecvOp;
pub use recvfrom::RecvfromOp;
#[cfg(all(target_os = "linux", feature = "fs"))]
pub use rename::RenameOp;
pub use send::SendOp;
pub use sendto::SendtoOp;
pub(crate) use socket_addr::socket_addr_to_raw;
#[cfg(all(target_os = "linux", feature = "splice"))]
pub use splice::SpliceOp;
#[cfg(all(
    target_os = "linux",
    any(target_env = "gnu", musl_v1_2_3),
    feature = "fs"
))]
pub use statx::StatxOp;
#[cfg(all(target_os = "linux", feature = "fs"))]
pub use symlink::SymlinkOp;
#[cfg(all(target_os = "linux", feature = "fs"))]
pub use unlink::UnlinkOp;
#[cfg(all(target_os = "linux", feature = "process"))]
pub use waitpid::WaitPidOp;
pub use write::WriteOp;
#[cfg(feature = "fs")]
pub use writeat::WriteAtOp;
pub use writev::WritevOp;

use crate::vibeio::driver::AnyDriver;

pub trait Op {
    /// I/O operation return type
    type Output;

    /// Whether a successful Linux completion transfers ownership of a new fd.
    /// The driver must close an unclaimed result after cancellation.
    #[cfg(target_os = "linux")]
    fn completion_returns_fd(&self) -> bool {
        false
    }

    /// Polls the operation for readiness (poll-based I/O).
    #[inline]
    fn poll_poll(
        &mut self,
        _cx: &mut Context<'_>,
        _driver: &AnyDriver,
    ) -> Poll<io::Result<Self::Output>> {
        Poll::Ready(Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "operation does not support poll-based submission",
        )))
    }

    /// Polls the operation for readiness (completion-based I/O).
    #[inline]
    fn poll_completion(
        &mut self,
        _cx: &mut Context<'_>,
        _driver: &AnyDriver,
    ) -> Poll<io::Result<Self::Output>> {
        Poll::Ready(Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "operation does not support completion-based submission",
        )))
    }

    /// Polls the operation for readiness (automatically determined I/O).
    #[cfg(any(feature = "fs", feature = "process"))]
    #[allow(dead_code)]
    #[inline]
    fn poll(&mut self, cx: &mut Context<'_>, driver: &AnyDriver) -> Poll<io::Result<Self::Output>> {
        if driver.supports_completion() {
            self.poll_completion(cx, driver)
        } else {
            self.poll_poll(cx, driver)
        }
    }

    /// Builds an io_uring submission entry for this operation. Returns the
    /// constructed SQE.
    #[cfg(target_os = "linux")]
    #[inline]
    fn build_completion_entry(
        &mut self,
        _user_data: u64,
    ) -> Result<io_uring::squeue::Entry, io::Error> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "operation does not support completion-based submission",
        ))
    }

    /// Submits a Windows overlapped I/O operation for this operation.
    #[cfg(windows)]
    #[inline]
    fn submit_windows(
        &mut self,
        _overlapped: *mut windows_sys::Win32::System::IO::OVERLAPPED,
    ) -> Result<(), io::Error> {
        Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "operation does not support completion-based submission",
        ))
    }
}

#[cfg(all(test, target_os = "linux"))]
mod vectored_uring_tests {
    use std::future::poll_fn;
    use std::os::fd::AsRawFd;

    use mio::Interest;

    use crate::vibeio::op::{ReadvOp, WritevOp};
    use crate::vibeio::{driver::AnyDriver, fd_inner::InnerRawHandle};

    #[test]
    fn io_uring_vectored_read_write_pipe() {
        // Create a runtime with an io_uring driver and run the test inside it so
        // current_driver() is available for InnerRawHandle::new.
        let driver = AnyDriver::new_uring().expect("failed to create uring driver");
        let runtime = crate::vibeio::executor::Runtime::new(driver);

        runtime.block_on(async {
            // Own endpoints before creating registrations: reverse local drop
            // order deregisters first, even if an assertion below unwinds.
            let (reader, writer) = std::io::pipe().unwrap();
            let rfd = reader.as_raw_fd();
            let wfd = writer.as_raw_fd();

            // Register both ends with the runtime. Since the current driver supports
            // completion and the InnerRawHandle default chooses completion mode, these
            // handles will use the completion path (io_uring) where available.
            let rhandle = InnerRawHandle::new(rfd, Interest::READABLE)
                .expect("failed to create reader InnerRawHandle");
            let whandle = InnerRawHandle::new(wfd, Interest::WRITABLE)
                .expect("failed to create writer InnerRawHandle");

            // Prepare vectored write buffers
            let a = b"hello ";
            let b = b"world!";
            let bufs = vec![
                Box::<[u8]>::from(a.as_slice()),
                Box::<[u8]>::from(b.as_slice()),
            ];
            let total_len = a.len() + b.len();

            // Submit vectored write. poll_writev will choose completion-path (io_uring)
            // for this handle because the driver supports completions.
            let whandle_ref = &whandle;
            let mut writev_op = WritevOp::new(whandle_ref, bufs);
            let write_res = poll_fn(move |cx| whandle_ref.poll_op(cx, &mut writev_op)).await;
            let written = write_res.expect("writev failed");
            assert!(
                written > 0,
                "expected writev to write at least one byte (wrote 0)"
            );

            // Prepare vectored read buffers (split sizes arbitrarily)
            let rd_bufs = vec![
                vec![0u8; 3].into_boxed_slice(), // will receive "hel"
                vec![0u8; total_len - 3].into_boxed_slice(),
            ];

            // Read using vectored read. poll_readv will choose completion-path when available.
            let rhandle_ref = &rhandle;
            let mut readv_op = ReadvOp::new(rhandle_ref, rd_bufs);
            let read_res = poll_fn(|cx| rhandle_ref.poll_op(cx, &mut readv_op)).await;
            let read = read_res.expect("readv failed");
            let received_bufs = readv_op.take_bufs();
            let dst1 = &received_bufs[0];
            let dst2 = &received_bufs[1];

            // We expect to read at least as many bytes as were written (pipe semantics
            // on local write -> read without closing may give the bytes).
            // It's possible for writev to be partial; just check we read some data and
            // that the bytes we did read match the written prefix.
            assert!(read > 0, "expected to read at least one byte");

            // Reconstruct the received bytes and compare with written prefix.
            let mut received = Vec::with_capacity(read);
            received.extend_from_slice(&dst1[..dst1.len().min(read)]);
            if read > dst1.len() {
                let rem = read - dst1.len();
                received.extend_from_slice(&dst2[..rem]);
            }

            // The bytes we received should be a prefix of the concatenation of a+b.
            let expected = [a.as_slice(), b.as_slice()].concat();
            assert_eq!(
                received,
                expected[..received.len()],
                "received bytes don't match expected prefix"
            );
        });
    }
}
