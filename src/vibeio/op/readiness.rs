use std::{
    io,
    task::{Context, Poll},
};

use crate::vibeio::{driver::AnyDriver, fd_inner::InnerRawHandle, op::Op};

pub struct ReadinessOp<'a> {
    handle: &'a InnerRawHandle,
    is_writable: bool,
}

impl<'a> ReadinessOp<'a> {
    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "ReadinessOp")
    )]
    #[inline]
    pub fn new_readable(handle: &'a InnerRawHandle) -> Self {
        Self {
            handle,
            is_writable: false,
        }
    }

    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "ReadinessOp")
    )]
    #[inline]
    pub fn new_writable(handle: &'a InnerRawHandle) -> Self {
        Self {
            handle,
            is_writable: true,
        }
    }
}

impl Op for ReadinessOp<'_> {
    type Output = ();

    #[cfg_attr(
        feature = "hotpath-profile",
        hotpath::measure(impl_type = "<ReadinessOp as Op>")
    )]
    #[inline]
    fn poll_poll(
        &mut self,
        cx: &mut Context<'_>,
        driver: &AnyDriver,
    ) -> Poll<io::Result<Self::Output>> {
        driver.submit_poll(
            self.handle,
            cx.waker().clone(),
            if self.is_writable {
                mio::Interest::WRITABLE
            } else {
                mio::Interest::READABLE
            },
        )?;
        Poll::Pending
    }
}
