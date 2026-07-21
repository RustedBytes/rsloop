use std::io;
use std::task::{Context, Poll};

use mio::Interest;

use crate::driver::AnyDriver;
use crate::fd_inner::InnerRawHandle;

pub(crate) enum CompletionBuffer<B> {
    Inline(B),
    Boxed(Box<B>),
}

impl<B> CompletionBuffer<B> {
    #[inline]
    pub(crate) fn new(buf: B, stable: bool) -> Self {
        if stable {
            Self::Boxed(Box::new(buf))
        } else {
            Self::Inline(buf)
        }
    }

    #[inline]
    pub(crate) fn as_ref(&self) -> &B {
        match self {
            Self::Inline(buf) => buf,
            Self::Boxed(buf) => buf.as_ref(),
        }
    }

    #[inline]
    pub(crate) fn as_mut(&mut self) -> &mut B {
        match self {
            Self::Inline(buf) => buf,
            Self::Boxed(buf) => buf.as_mut(),
        }
    }

    #[inline]
    pub(crate) fn into_inner(self) -> B {
        match self {
            Self::Inline(buf) => buf,
            Self::Boxed(buf) => *buf,
        }
    }

    #[inline]
    pub(crate) fn into_stable_box(self) -> Box<B> {
        match self {
            Self::Inline(buf) => Box::new(buf),
            Self::Boxed(buf) => buf,
        }
    }
}

#[inline]
pub(crate) fn poll_result_or_wait(
    result: io::Result<usize>,
    handle: &InnerRawHandle,
    cx: &mut Context<'_>,
    driver: &AnyDriver,
    interest: Interest,
) -> Poll<io::Result<usize>> {
    match result {
        Ok(value) => Poll::Ready(Ok(value)),
        Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
            if let Err(submit_err) = driver.submit_poll(handle, cx.waker().clone(), interest) {
                Poll::Ready(Err(submit_err))
            } else {
                Poll::Pending
            }
        }
        Err(err) => Poll::Ready(Err(err)),
    }
}
