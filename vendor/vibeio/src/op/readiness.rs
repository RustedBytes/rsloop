use std::io;
use std::task::{Context, Poll};

use crate::driver::AnyDriver;
use crate::fd_inner::InnerRawHandle;
use crate::op::Op;

pub struct ReadinessOp<'a> {
    handle: &'a InnerRawHandle,
    is_writable: bool,
}

impl<'a> ReadinessOp<'a> {
    #[inline]
    pub fn new_readable(handle: &'a InnerRawHandle) -> Self {
        Self {
            handle,
            is_writable: false,
        }
    }

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
