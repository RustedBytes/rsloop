use std::io;
use std::task::{Context, Poll};

use mio::Interest;

use crate::vibeio::driver::AnyDriver;
use crate::vibeio::fd_inner::InnerRawHandle;

pub(crate) enum CompletionBuffer<B> {
    Inline(B),
    Boxed(Box<B>),
}

#[cfg(test)]
mod storage_tests {
    use super::*;
    use crate::vibeio::io::{IoBuf, IoBufMut};

    #[test]
    fn inline_array_keeps_address_through_completion_storage_moves() {
        let mut storage = CompletionBuffer::new([7u8; 32], true);
        let pointer = storage.as_mut().as_buf_mut_ptr();
        let moved = Box::new(storage);
        assert_eq!(moved.as_ref().as_ref().as_buf_ptr(), pointer);
        let retained = (*moved).into_stable_box();
        assert_eq!(retained.as_ref().as_buf_ptr(), pointer);
        assert_eq!(retained.as_ref(), &[7; 32]);
        // Model the type-erased driver payload and eventual acknowledgement.
        let payload: Box<dyn std::any::Any> = Box::new(Some(retained));
        let acknowledged = payload
            .downcast::<Option<Box<[u8; 32]>>>()
            .unwrap()
            .unwrap();
        assert_eq!(acknowledged.as_ref().as_buf_ptr(), pointer);
        assert_eq!(*acknowledged, [7; 32]);
    }

    #[test]
    fn poll_storage_returns_inline_array_without_stability_requirement() {
        let storage = CompletionBuffer::new([5u8; 8], false);
        assert!(matches!(storage, CompletionBuffer::Inline(_)));
        assert_eq!(storage.into_inner(), [5; 8]);
    }
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

#[cfg(test)]
pub(crate) mod cancellation_tests {
    use super::*;
    use crate::vibeio::io::{IoBuf, IoBufMut, IoVec, IoVectoredBuf, IoVectoredBufMut};
    use std::{rc::Rc, sync::Arc};

    pub(crate) struct TrackedBuffer {
        bytes: Box<[u8]>,
        _lifetime: Arc<()>,
    }

    impl TrackedBuffer {
        pub(crate) fn new(lifetime: Arc<()>) -> Self {
            Self {
                bytes: vec![0; 8].into_boxed_slice(),
                _lifetime: lifetime,
            }
        }
    }

    // SAFETY: the box owns eight initialized bytes at a stable address.
    unsafe impl IoBuf for TrackedBuffer {
        fn as_buf_ptr(&self) -> *const u8 {
            self.bytes.as_ptr()
        }
        fn buf_len(&self) -> usize {
            self.bytes.len()
        }
        fn buf_capacity(&self) -> usize {
            self.bytes.len()
        }
    }
    // SAFETY: all bytes are initialized and exclusively owned by this buffer.
    unsafe impl IoBufMut for TrackedBuffer {
        fn as_buf_mut_ptr(&mut self) -> *mut u8 {
            self.bytes.as_mut_ptr()
        }
        unsafe fn set_buf_init(&mut self, _: usize) {}
    }
    // SAFETY: the sole vector points into the buffer's owned, stable allocation.
    unsafe impl IoVectoredBuf for TrackedBuffer {
        fn as_iovecs(&self) -> Box<[IoVec]> {
            vec![IoVec {
                ptr: self.bytes.as_ptr().cast_mut(),
                len: self.bytes.len(),
            }]
            .into_boxed_slice()
        }
    }
    // SAFETY: the sole writable region is exclusively borrowed and cannot overlap.
    unsafe impl IoVectoredBufMut for TrackedBuffer {
        fn as_iovecs_mut(&mut self) -> Box<[IoVec]> {
            vec![IoVec {
                ptr: self.bytes.as_mut_ptr(),
                len: self.bytes.len(),
            }]
            .into_boxed_slice()
        }
    }

    pub(crate) fn check_cancellation(
        cancel: impl Fn(&InnerRawHandle, TrackedBuffer, bool) + Copy + 'static,
    ) {
        for entered in [false, true] {
            for reclaim in [false, true] {
                let owner = Rc::new(AnyDriver::new_mock());
                let handle = InnerRawHandle::for_mock_completion(owner.clone());
                let lifetime = Arc::new(());
                let weak = Arc::downgrade(&lifetime);
                let buffer = TrackedBuffer::new(lifetime);
                if entered {
                    // A different runtime must not receive the owner's token.
                    let runtime = crate::vibeio::executor::Runtime::new(AnyDriver::new_mock());
                    runtime.block_on(async move { cancel(&handle, buffer, reclaim) });
                } else {
                    assert!(crate::vibeio::current_driver().is_none());
                    cancel(&handle, buffer, reclaim);
                }
                let AnyDriver::Mock(driver) = owner.as_ref() else {
                    unreachable!()
                };
                let held = driver.ignored.take();
                assert_eq!(held.len(), 1, "cancellation missed the owning driver");
                assert_eq!(held[0].0, 41);
                assert!(
                    weak.upgrade().is_some(),
                    "buffer released before completion"
                );
                drop(held); // model completion acknowledgement
                assert!(
                    weak.upgrade().is_none(),
                    "buffer retained after acknowledgement"
                );
            }
        }
    }
}
