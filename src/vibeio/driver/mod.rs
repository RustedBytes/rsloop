#[cfg(windows)]
mod iocp;
#[cfg(target_vendor = "apple")]
mod kqueue;
#[cfg(unix)]
mod mio;
mod mock;
#[cfg(target_os = "linux")]
mod uring;

#[cfg(any(windows, test))]
use std::collections::HashSet;
use std::task::Waker;
#[cfg(target_os = "linux")]
use std::task::{Context, Poll};
use std::{io, time::Duration};

use ::mio::{Interest, Token};

#[cfg(windows)]
use crate::vibeio::driver::iocp::{IocpDriver, IocpInterruptor};
#[cfg(target_vendor = "apple")]
use crate::vibeio::driver::kqueue::{KqueueDriver, KqueueInterruptor};
#[cfg(unix)]
use crate::vibeio::driver::mio::{MioDriver, MioInterruptor};
use crate::vibeio::driver::mock::MockInterruptor;
#[cfg(target_os = "linux")]
use crate::vibeio::driver::uring::{UringDriver, UringInterruptor};
use crate::vibeio::op::Op;
use crate::vibeio::{driver::mock::MockDriver, fd_inner::InnerRawHandle};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum RegistrationMode {
    Poll,
    Completion,
}

#[derive(Debug)]
pub enum CompletionIoResult {
    #[allow(dead_code)]
    Ok(i32),
    #[allow(dead_code)]
    Retry(usize), // usize -> token
    SubmitErr(std::io::Error),
}

#[cfg(any(target_os = "linux", windows, test))]
struct RetainedCompletionData(Vec<Box<dyn std::any::Any>>);

/// Preserve stable payload allocations without building a recursive drop chain.
#[cfg(any(target_os = "linux", windows, test))]
fn retain_completion_data(
    retained: &mut Option<Box<dyn std::any::Any>>,
    data: Box<dyn std::any::Any>,
) {
    if let Some(group) = retained
        .as_mut()
        .and_then(|owner| owner.downcast_mut::<RetainedCompletionData>())
    {
        group.0.push(data);
        return;
    }
    *retained = Some(match retained.take() {
        None => data, // Ordinary first cancellation needs no additional allocation.
        Some(first) => Box::new(RetainedCompletionData(vec![first, data])),
    });
}

#[cfg(test)]
mod retained_completion_tests {
    use super::*;
    use std::cell::Cell;
    use std::rc::Rc;

    #[test]
    fn repeated_retention_preserves_allocations_and_drops_a_flat_list() {
        struct Tracked(Rc<Cell<usize>>);
        impl Drop for Tracked {
            fn drop(&mut self) {
                self.0.set(self.0.get() + 1);
            }
        }
        let drops = Rc::new(Cell::new(0));
        let first = Box::new(Tracked(drops.clone()));
        let first_address = std::ptr::from_ref(first.as_ref());
        let mut retained = None;
        retain_completion_data(&mut retained, first);
        assert!(retained.as_ref().unwrap().is::<Tracked>());
        for _ in 1..100_000 {
            retain_completion_data(&mut retained, Box::new(Tracked(drops.clone())));
        }
        let group = retained
            .as_ref()
            .unwrap()
            .downcast_ref::<RetainedCompletionData>()
            .unwrap();
        assert_eq!(group.0.len(), 100_000);
        assert!(std::ptr::eq(
            group.0[0].downcast_ref::<Tracked>().unwrap(),
            first_address
        ));
        assert_eq!(drops.get(), 0);
        drop(retained);
        assert_eq!(drops.get(), 100_000);
    }
}

/// Follow provider handles without looping forever on a cyclic fallback chain.
#[cfg(any(windows, test))]
fn resolve_base_socket_with(
    mut socket: usize,
    mut base: impl FnMut(usize) -> io::Result<usize>,
    mut layered: impl FnMut(usize) -> io::Result<usize>,
) -> io::Result<usize> {
    // Layered service providers are expected to form a very short chain. Keep
    // a generous limit so a corrupt provider cannot force unbounded work even
    // when it returns a fresh bogus handle at every step.
    const MAX_PROVIDER_CHAIN_DEPTH: usize = 64;
    let mut visited = HashSet::from([socket]);
    for _ in 0..MAX_PROVIDER_CHAIN_DEPTH {
        if let Ok(resolved) = base(socket) {
            if resolved != usize::MAX {
                return Ok(resolved);
            }
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "provider returned an invalid base socket",
            ));
        }
        let next = layered(socket)?;
        if next == usize::MAX || !visited.insert(next) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid or cyclic socket provider chain",
            ));
        }
        socket = next;
    }

    Err(io::Error::new(
        io::ErrorKind::InvalidData,
        "socket provider chain exceeds the supported depth",
    ))
}

#[cfg(test)]
mod base_socket_tests {
    use super::*;

    #[test]
    fn resolver_handles_success_errors_and_provider_cycles() {
        assert_eq!(
            resolve_base_socket_with(1, |_| Ok(7), |_| panic!("unexpected fallback")).unwrap(),
            7
        );
        let unsupported = |_| Err(io::Error::from(io::ErrorKind::Unsupported));
        assert_eq!(
            resolve_base_socket_with(
                1,
                |socket| {
                    if socket == 3 {
                        Ok(7)
                    } else {
                        unsupported(socket)
                    }
                },
                |socket| Ok(socket + 1)
            )
            .unwrap(),
            7
        );
        for length in [1, 2, 3, 32] {
            let error =
                resolve_base_socket_with(0, unsupported, |socket| Ok((socket + 1) % length))
                    .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        }
        let mut fallback_calls = 0;
        let error = resolve_base_socket_with(0, unsupported, |socket| {
            fallback_calls += 1;
            Ok(socket + 1)
        })
        .unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidData);
        assert_eq!(fallback_calls, 64);
        assert_eq!(
            resolve_base_socket_with(
                1,
                |_| Ok(usize::MAX),
                |_| panic!("invalid success must fail")
            )
            .unwrap_err()
            .kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(
            resolve_base_socket_with(1, unsupported, |_| Ok(usize::MAX))
                .unwrap_err()
                .kind(),
            io::ErrorKind::InvalidData
        );
        assert_eq!(
            resolve_base_socket_with(1, unsupported, |_| Err(io::Error::from_raw_os_error(123)))
                .unwrap_err()
                .raw_os_error(),
            Some(123)
        );
    }
}

/// Encode only errors representable by the driver's negative i32 result.
#[cfg(any(windows, test))]
fn encode_completion_error(code: u32) -> Option<i32> {
    i32::try_from(code)
        .ok()
        .filter(|code| *code > 0)
        .map(|code| -code)
}

#[cfg(test)]
mod completion_encoding_tests {
    use super::*;

    #[test]
    fn native_errors_cannot_become_success_counts_or_overflow() {
        for code in [1, 6, 38, 317, 534, 995, i32::MAX as u32] {
            let encoded = encode_completion_error(code).unwrap();
            assert!(encoded < 0);
            assert_eq!(completion_error(encoded).raw_os_error(), Some(code as i32));
        }
        for code in [0, i32::MAX as u32 + 1, u32::MAX] {
            assert_eq!(encode_completion_error(code), None);
        }
    }
}

/// Reserve Windows INFINITE for an explicitly unbounded wait.
#[cfg(any(windows, test))]
fn iocp_timeout_ms(timeout: Option<Duration>) -> u32 {
    match timeout {
        Some(timeout) => timeout.as_millis().min((u32::MAX - 1) as u128) as u32,
        None => u32::MAX,
    }
}

#[cfg(test)]
mod iocp_timeout_tests {
    use super::*;

    #[test]
    fn finite_timeouts_never_select_infinite_wait() {
        assert_eq!(iocp_timeout_ms(None), u32::MAX);
        assert_eq!(iocp_timeout_ms(Some(Duration::ZERO)), 0);
        assert_eq!(iocp_timeout_ms(Some(Duration::from_millis(1))), 1);
        for millis in [u32::MAX as u64 - 1, u32::MAX as u64, u64::MAX] {
            assert_eq!(
                iocp_timeout_ms(Some(Duration::from_millis(millis))),
                u32::MAX - 1
            );
        }
        assert_eq!(iocp_timeout_ms(Some(Duration::MAX)), u32::MAX - 1);
    }
}

/// Decode the driver's negative error representation without signed overflow.
pub(crate) fn completion_error(result: i32) -> io::Error {
    match result.checked_neg().filter(|code| *code > 0) {
        Some(code) => io::Error::from_raw_os_error(code),
        None => io::Error::new(
            io::ErrorKind::InvalidData,
            "invalid negative completion result",
        ),
    }
}

#[cfg(any(target_vendor = "apple", target_os = "linux", test))]
#[inline]
fn send_wake_notification(mut send: impl FnMut() -> io::Result<usize>) -> io::Result<()> {
    loop {
        match send() {
            Ok(_) => return Ok(()),
            // A full nonblocking socket/eventfd already has a queued wake.
            Err(err) if err.kind() == io::ErrorKind::WouldBlock => return Ok(()),
            // Retry without recursive stack growth under repeated signals.
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => return Err(err),
        }
    }
}

#[inline]
fn unsupported_completion_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "driver does not support completion-based I/O submission",
    )
}

#[inline]
fn unsupported_poll_error() -> io::Error {
    io::Error::new(
        io::ErrorKind::Unsupported,
        "driver does not support poll-based I/O submission",
    )
}

pub trait Interruptor {
    /// Interrupts a waiting I/O operation.
    fn interrupt(&self);
}

pub enum AnyInterruptor {
    Mock(MockInterruptor),
    #[cfg(windows)]
    Iocp(IocpInterruptor),
    #[cfg(unix)]
    Mio(MioInterruptor),
    #[cfg(target_vendor = "apple")]
    Kqueue(KqueueInterruptor),
    #[cfg(target_os = "linux")]
    IoUring(UringInterruptor),
}

impl AnyInterruptor {
    pub(crate) fn interrupt(&self) {
        match self {
            AnyInterruptor::Mock(interruptor) => interruptor.interrupt(),
            #[cfg(windows)]
            AnyInterruptor::Iocp(interruptor) => interruptor.interrupt(),
            #[cfg(unix)]
            AnyInterruptor::Mio(interruptor) => interruptor.interrupt(),
            #[cfg(target_vendor = "apple")]
            AnyInterruptor::Kqueue(interruptor) => interruptor.interrupt(),
            #[cfg(target_os = "linux")]
            AnyInterruptor::IoUring(interruptor) => interruptor.interrupt(),
        }
    }
}

pub trait Driver {
    type Interruptor: Interruptor;

    /// Flushes the driver's I/O.
    #[inline]
    fn flush(&self) {}

    /// Returns whether the executor should call `flush` after polling a task batch.
    #[inline]
    fn should_flush(&self) -> bool {
        true
    }

    /// Waits for the I/O.
    fn wait(&self, timeout: Option<Duration>);

    /// Registers an I/O source and returns its token.
    fn register_handle(
        &self,
        handle: &InnerRawHandle,
        interest: Interest,
    ) -> Result<Token, std::io::Error>;

    /// Registers an I/O source with the requested mode.
    fn register_handle_with_mode(
        &self,
        handle: &InnerRawHandle,
        interest: Interest,
        _mode: RegistrationMode,
    ) -> Result<Token, io::Error> {
        self.register_handle(handle, interest)
    }

    /// Updates the interest set for a registered I/O source.
    fn reregister_handle(
        &self,
        handle: &InnerRawHandle,
        interest: Interest,
    ) -> Result<(), std::io::Error>;

    /// Removes an I/O source from the poller.
    fn deregister_handle(&self, handle: &InnerRawHandle) -> Result<(), std::io::Error>;

    /// Returns whether the driver supports completion-based I/O operations.
    #[inline]
    fn supports_completion(&self) -> bool {
        false
    }

    /// Submits a completion-based I/O operation.
    #[inline]
    fn submit_completion<O>(&self, _op: &mut O, _waker: Waker) -> CompletionIoResult
    where
        O: Op,
    {
        CompletionIoResult::SubmitErr(unsupported_completion_error())
    }

    /// Re-registers interest and submits a waker for poll-based I/O.
    #[inline]
    fn submit_poll(
        &self,
        _handle: &InnerRawHandle,
        _waker: Waker,
        _interest: Interest,
    ) -> Result<(), io::Error> {
        Err(unsupported_poll_error())
    }

    /// Obtains the result for a completion-based I/O operation.
    #[inline]
    fn get_completion_result(&self, _token: usize) -> Option<i32> {
        None
    }

    /// Polls a Linux multishot accept stream.
    #[cfg(target_os = "linux")]
    #[inline]
    fn poll_multishot_accept(
        &self,
        _handle: &InnerRawHandle,
        _cx: &mut Context<'_>,
    ) -> Poll<io::Result<i32>> {
        Poll::Ready(Err(io::Error::new(
            io::ErrorKind::Unsupported,
            "driver does not support multishot accept",
        )))
    }

    /// Sets the waker for a completion-based I/O operation.
    #[inline]
    fn set_completion_waker(&self, _token: usize, _waker: Waker) {}

    /// Cancels a completion-based I/O operation.
    #[inline]
    fn ignore_completion(&self, _token: usize, _data: Box<dyn std::any::Any>) {}

    /// Cancels a Windows completion operation while retaining its owned data
    /// until the completion packet is observed.
    #[cfg(windows)]
    #[inline]
    fn cancel_completion(
        &self,
        token: usize,
        _handle: crate::vibeio::fd_inner::RawOsHandle,
        data: Box<dyn std::any::Any>,
    ) {
        self.ignore_completion(token, data);
    }

    /// Interrupts a waiting I/O operation.
    fn get_interruptor(&self) -> Self::Interruptor;
}

#[allow(
    clippy::large_enum_variant,
    reason = "Keep the selected driver inline without an extra allocation or indirection"
)]
pub enum AnyDriver {
    Mock(MockDriver),
    #[cfg(windows)]
    Iocp(IocpDriver),
    #[cfg(unix)]
    Mio(MioDriver),
    #[cfg(target_vendor = "apple")]
    Kqueue(KqueueDriver),
    #[cfg(target_os = "linux")]
    IoUring(UringDriver),
}

impl AnyDriver {
    #[cfg(unix)]
    #[inline]
    pub(crate) fn new_mio() -> Result<Self, std::io::Error> {
        Ok(AnyDriver::Mio(MioDriver::new()?))
    }

    #[cfg(target_vendor = "apple")]
    #[inline]
    pub(crate) fn new_kqueue() -> Result<Self, io::Error> {
        Ok(AnyDriver::Kqueue(KqueueDriver::new()?))
    }

    #[inline]
    pub(crate) fn new_mock() -> Self {
        AnyDriver::Mock(MockDriver::new())
    }

    #[cfg(windows)]
    #[inline]
    pub(crate) fn new_iocp() -> Result<Self, io::Error> {
        Ok(AnyDriver::Iocp(IocpDriver::new()?))
    }

    #[cfg(target_os = "linux")]
    #[inline]
    pub(crate) fn new_uring_custom(builder: io_uring::Builder) -> Result<Self, io::Error> {
        Ok(AnyDriver::IoUring(UringDriver::new(1024, builder)?))
    }

    #[cfg(target_os = "linux")]
    #[inline]
    pub(crate) fn new_uring() -> Result<Self, io::Error> {
        let mut builder = io_uring::IoUring::builder();
        builder
            .setup_single_issuer()
            .setup_coop_taskrun()
            .setup_taskrun_flag()
            .setup_defer_taskrun()
            .setup_submit_all();

        // Prefer the newer single-threaded setup, but retain support for
        // kernels that provide basic io_uring without every optimization.
        Self::new_uring_custom(builder)
            .or_else(|_| Self::new_uring_custom(io_uring::IoUring::builder()))
    }

    #[inline]
    pub(crate) fn new_best() -> Result<Self, io::Error> {
        #[cfg(target_os = "linux")]
        {
            // io_uring may be unavailable because of the kernel version,
            // vendor configuration, sysctls, or a container seccomp policy.
            // Automatic selection must remain usable in all of those cases.
            Self::new_uring().or_else(|_| Self::new_mio())
        }

        #[cfg(target_vendor = "apple")]
        {
            Self::new_kqueue()
        }
        #[cfg(all(unix, not(any(target_os = "linux", target_vendor = "apple"))))]
        {
            Self::new_mio()
        }
        #[cfg(windows)]
        {
            Self::new_iocp()
        }
    }

    #[inline]
    pub(crate) fn flush(&self) {
        match self {
            #[cfg(windows)]
            AnyDriver::Iocp(driver) => driver.flush(),
            #[cfg(unix)]
            AnyDriver::Mio(driver) => driver.flush(),
            #[cfg(target_vendor = "apple")]
            AnyDriver::Kqueue(driver) => driver.flush(),
            AnyDriver::Mock(driver) => driver.flush(),
            #[cfg(target_os = "linux")]
            AnyDriver::IoUring(driver) => driver.flush(),
        }
    }

    #[inline]
    pub(crate) fn should_flush(&self) -> bool {
        match self {
            #[cfg(windows)]
            AnyDriver::Iocp(driver) => driver.should_flush(),
            #[cfg(unix)]
            AnyDriver::Mio(driver) => driver.should_flush(),
            #[cfg(target_vendor = "apple")]
            AnyDriver::Kqueue(driver) => driver.should_flush(),
            AnyDriver::Mock(driver) => driver.should_flush(),
            #[cfg(target_os = "linux")]
            AnyDriver::IoUring(driver) => driver.should_flush(),
        }
    }

    #[inline]
    pub(crate) fn wait(&self, timeout: Option<Duration>) {
        match self {
            #[cfg(windows)]
            AnyDriver::Iocp(driver) => driver.wait(timeout),
            #[cfg(unix)]
            AnyDriver::Mio(driver) => driver.wait(timeout),
            #[cfg(target_vendor = "apple")]
            AnyDriver::Kqueue(driver) => driver.wait(timeout),
            AnyDriver::Mock(driver) => driver.wait(timeout),
            #[cfg(target_os = "linux")]
            AnyDriver::IoUring(driver) => driver.wait(timeout),
        }
    }

    #[allow(dead_code)]
    #[inline]
    pub(crate) fn register_handle(
        &self,
        handle: &InnerRawHandle,
        interest: Interest,
    ) -> Result<Token, std::io::Error> {
        match self {
            #[cfg(windows)]
            AnyDriver::Iocp(driver) => driver.register_handle(handle, interest),
            #[cfg(unix)]
            AnyDriver::Mio(driver) => driver.register_handle(handle, interest),
            #[cfg(target_vendor = "apple")]
            AnyDriver::Kqueue(driver) => driver.register_handle(handle, interest),
            AnyDriver::Mock(driver) => driver.register_handle(handle, interest),
            #[cfg(target_os = "linux")]
            AnyDriver::IoUring(driver) => driver.register_handle(handle, interest),
        }
    }

    #[inline]
    pub(crate) fn register_handle_with_mode(
        &self,
        handle: &InnerRawHandle,
        interest: Interest,
        mode: RegistrationMode,
    ) -> Result<Token, io::Error> {
        match self {
            #[cfg(windows)]
            AnyDriver::Iocp(driver) => driver.register_handle_with_mode(handle, interest, mode),
            #[cfg(unix)]
            AnyDriver::Mio(driver) => driver.register_handle_with_mode(handle, interest, mode),
            #[cfg(target_vendor = "apple")]
            AnyDriver::Kqueue(driver) => driver.register_handle_with_mode(handle, interest, mode),
            AnyDriver::Mock(driver) => driver.register_handle_with_mode(handle, interest, mode),
            #[cfg(target_os = "linux")]
            AnyDriver::IoUring(driver) => driver.register_handle_with_mode(handle, interest, mode),
        }
    }

    #[inline]
    pub(crate) fn reregister_handle(
        &self,
        handle: &InnerRawHandle,
        interest: Interest,
    ) -> Result<(), std::io::Error> {
        match self {
            #[cfg(windows)]
            AnyDriver::Iocp(driver) => driver.reregister_handle(handle, interest),
            #[cfg(unix)]
            AnyDriver::Mio(driver) => driver.reregister_handle(handle, interest),
            #[cfg(target_vendor = "apple")]
            AnyDriver::Kqueue(driver) => driver.reregister_handle(handle, interest),
            AnyDriver::Mock(driver) => driver.reregister_handle(handle, interest),
            #[cfg(target_os = "linux")]
            AnyDriver::IoUring(driver) => driver.reregister_handle(handle, interest),
        }
    }

    #[inline]
    pub(crate) fn deregister_handle(&self, handle: &InnerRawHandle) -> Result<(), std::io::Error> {
        match self {
            #[cfg(windows)]
            AnyDriver::Iocp(driver) => driver.deregister_handle(handle),
            #[cfg(unix)]
            AnyDriver::Mio(driver) => driver.deregister_handle(handle),
            #[cfg(target_vendor = "apple")]
            AnyDriver::Kqueue(driver) => driver.deregister_handle(handle),
            AnyDriver::Mock(driver) => driver.deregister_handle(handle),
            #[cfg(target_os = "linux")]
            AnyDriver::IoUring(driver) => driver.deregister_handle(handle),
        }
    }

    #[inline]
    pub(crate) fn supports_completion(&self) -> bool {
        match self {
            #[cfg(windows)]
            AnyDriver::Iocp(driver) => driver.supports_completion(),
            #[cfg(unix)]
            AnyDriver::Mio(driver) => driver.supports_completion(),
            #[cfg(target_vendor = "apple")]
            AnyDriver::Kqueue(driver) => driver.supports_completion(),
            AnyDriver::Mock(driver) => driver.supports_completion(),
            #[cfg(target_os = "linux")]
            AnyDriver::IoUring(driver) => driver.supports_completion(),
        }
    }

    #[inline]
    pub(crate) fn submit_completion<O>(&self, op: &mut O, waker: Waker) -> CompletionIoResult
    where
        O: Op,
    {
        match self {
            #[cfg(windows)]
            AnyDriver::Iocp(driver) => driver.submit_completion(op, waker),
            #[cfg(unix)]
            AnyDriver::Mio(driver) => driver.submit_completion(op, waker),
            #[cfg(target_vendor = "apple")]
            AnyDriver::Kqueue(driver) => driver.submit_completion(op, waker),
            AnyDriver::Mock(driver) => driver.submit_completion(op, waker),
            #[cfg(target_os = "linux")]
            AnyDriver::IoUring(driver) => driver.submit_completion(op, waker),
        }
    }

    #[inline]
    pub(crate) fn submit_poll(
        &self,
        handle: &InnerRawHandle,
        waker: Waker,
        interest: Interest,
    ) -> Result<(), io::Error> {
        match self {
            #[cfg(windows)]
            AnyDriver::Iocp(driver) => driver.submit_poll(handle, waker, interest),
            #[cfg(unix)]
            AnyDriver::Mio(driver) => driver.submit_poll(handle, waker, interest),
            #[cfg(target_vendor = "apple")]
            AnyDriver::Kqueue(driver) => driver.submit_poll(handle, waker, interest),
            AnyDriver::Mock(driver) => driver.submit_poll(handle, waker, interest),
            #[cfg(target_os = "linux")]
            AnyDriver::IoUring(driver) => driver.submit_poll(handle, waker, interest),
        }
    }

    #[inline]
    pub(crate) fn get_completion_result(&self, token: usize) -> Option<i32> {
        match self {
            #[cfg(windows)]
            AnyDriver::Iocp(driver) => driver.get_completion_result(token),
            #[cfg(unix)]
            AnyDriver::Mio(driver) => driver.get_completion_result(token),
            #[cfg(target_vendor = "apple")]
            AnyDriver::Kqueue(driver) => driver.get_completion_result(token),
            AnyDriver::Mock(driver) => driver.get_completion_result(token),
            #[cfg(target_os = "linux")]
            AnyDriver::IoUring(driver) => driver.get_completion_result(token),
        }
    }

    #[cfg(target_os = "linux")]
    #[inline]
    pub(crate) fn poll_multishot_accept(
        &self,
        handle: &InnerRawHandle,
        cx: &mut Context<'_>,
    ) -> Poll<io::Result<i32>> {
        match self {
            AnyDriver::IoUring(driver) => driver.poll_multishot_accept(handle, cx),
            AnyDriver::Mio(driver) => driver.poll_multishot_accept(handle, cx),
            AnyDriver::Mock(driver) => driver.poll_multishot_accept(handle, cx),
        }
    }

    #[inline]
    pub(crate) fn set_completion_waker(&self, token: usize, waker: Waker) {
        match self {
            #[cfg(windows)]
            AnyDriver::Iocp(driver) => driver.set_completion_waker(token, waker),
            #[cfg(unix)]
            AnyDriver::Mio(driver) => driver.set_completion_waker(token, waker),
            #[cfg(target_vendor = "apple")]
            AnyDriver::Kqueue(driver) => driver.set_completion_waker(token, waker),
            AnyDriver::Mock(driver) => driver.set_completion_waker(token, waker),
            #[cfg(target_os = "linux")]
            AnyDriver::IoUring(driver) => driver.set_completion_waker(token, waker),
        }
    }

    #[cfg(not(windows))]
    #[inline]
    pub(crate) fn ignore_completion(&self, token: usize, data: Box<dyn std::any::Any>) {
        match self {
            #[cfg(unix)]
            AnyDriver::Mio(driver) => driver.ignore_completion(token, data),
            #[cfg(target_vendor = "apple")]
            AnyDriver::Kqueue(driver) => driver.ignore_completion(token, data),
            AnyDriver::Mock(driver) => driver.ignore_completion(token, data),
            #[cfg(target_os = "linux")]
            AnyDriver::IoUring(driver) => driver.ignore_completion(token, data),
        }
    }

    #[cfg(windows)]
    #[inline]
    pub(crate) fn cancel_completion(
        &self,
        token: usize,
        handle: crate::vibeio::fd_inner::RawOsHandle,
        data: Box<dyn std::any::Any>,
    ) {
        match self {
            AnyDriver::Iocp(driver) => driver.cancel_completion(token, handle, data),
            AnyDriver::Mock(driver) => driver.ignore_completion(token, data),
        }
    }

    #[inline]
    pub(crate) fn get_interruptor(&self) -> AnyInterruptor {
        match self {
            #[cfg(windows)]
            AnyDriver::Iocp(driver) => AnyInterruptor::Iocp(driver.get_interruptor()),
            #[cfg(unix)]
            AnyDriver::Mio(driver) => AnyInterruptor::Mio(driver.get_interruptor()),
            #[cfg(target_vendor = "apple")]
            AnyDriver::Kqueue(driver) => AnyInterruptor::Kqueue(driver.get_interruptor()),
            AnyDriver::Mock(driver) => AnyInterruptor::Mock(driver.get_interruptor()),
            #[cfg(target_os = "linux")]
            AnyDriver::IoUring(driver) => AnyInterruptor::IoUring(driver.get_interruptor()),
        }
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn wake_notification_retries_interruptions_without_losing_terminal_result() {
        use std::io::{self, ErrorKind};
        for terminal in [
            None,
            Some(ErrorKind::WouldBlock),
            Some(ErrorKind::BrokenPipe),
        ] {
            let mut attempts = 0usize;
            let result = super::send_wake_notification(|| {
                attempts += 1;
                if attempts <= 100_000 {
                    return Err(ErrorKind::Interrupted.into());
                }
                assert_eq!(attempts, 100_001, "must stop at the terminal result");
                match terminal {
                    None => Ok(1),
                    Some(kind) => Err(io::Error::new(kind, "terminal send result")),
                }
            });
            assert_eq!(attempts, 100_001);
            if terminal == Some(ErrorKind::BrokenPipe) {
                let error = result.unwrap_err();
                assert_eq!(error.kind(), ErrorKind::BrokenPipe);
                assert_eq!(error.to_string(), "terminal send result");
            } else {
                result.unwrap();
            }
        }
    }

    use super::AnyDriver;
    use std::{
        future::poll_fn,
        task::{Poll, Waker},
    };

    #[cfg(unix)]
    #[test]
    fn test_mio_driver_interrupt_basic() {
        let driver = AnyDriver::new_mio().expect("Failed to create MioDriver");
        let interruptor = driver.get_interruptor();

        // Test that interrupt doesn't panic and can be called multiple times
        interruptor.interrupt();
        interruptor.interrupt();
        interruptor.interrupt();
    }

    #[cfg(target_vendor = "apple")]
    #[test]
    fn test_kqueue_driver_interrupt_basic() {
        let driver = AnyDriver::new_kqueue().expect("Failed to create KqueueDriver");
        let interruptor = driver.get_interruptor();
        interruptor.interrupt();
        interruptor.interrupt();
        interruptor.interrupt();
        driver.wait(Some(std::time::Duration::from_millis(10)));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_uring_driver_interrupt_basic() {
        let driver = AnyDriver::new_uring().expect("Failed to create UringDriver");
        let interruptor = driver.get_interruptor();

        // Test that interrupt doesn't panic and can be called multiple times
        interruptor.interrupt();
        interruptor.interrupt();
        interruptor.interrupt();
    }

    #[test]
    fn test_mock_driver_interrupt_basic() {
        let driver = AnyDriver::new_mock();
        let interruptor = driver.get_interruptor();

        // Test that interrupt doesn't panic and can be called multiple times
        interruptor.interrupt();
        interruptor.interrupt();
        interruptor.interrupt();
    }

    #[cfg(windows)]
    #[test]
    fn test_iocp_driver_interrupt_basic() {
        let driver = AnyDriver::new_iocp().expect("Failed to create IocpDriver");
        let interruptor = driver.get_interruptor();

        // Test that interrupt doesn't panic and can be called multiple times
        interruptor.interrupt();
        interruptor.interrupt();
        interruptor.interrupt();
    }

    #[cfg(unix)]
    #[test]
    fn test_interrupt_mio() {
        let runtime = crate::vibeio::executor::Runtime::new(
            AnyDriver::new_mio().expect("Failed to create MioDriver"),
        );

        let (tx, rx) = std::sync::mpsc::channel();

        std::thread::spawn(move || {
            let waker: Waker = rx.recv().unwrap();
            drop(rx); // Drop the receiver before waking the task
            waker.wake();
        });

        runtime.block_on(poll_fn(move |cx| {
            if tx.send(cx.waker().clone()).is_ok() {
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        }));
    }

    #[cfg(target_vendor = "apple")]
    #[test]
    fn test_interrupt_kqueue() {
        let runtime = crate::vibeio::executor::Runtime::new(
            AnyDriver::new_kqueue().expect("Failed to create KqueueDriver"),
        );
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let waker: Waker = rx.recv().unwrap();
            drop(rx);
            waker.wake();
        });
        runtime.block_on(poll_fn(move |cx| {
            if tx.send(cx.waker().clone()).is_ok() {
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        }));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_interruptor_uring() {
        let runtime = crate::vibeio::executor::Runtime::new(
            AnyDriver::new_uring().expect("Failed to create UringDriver"),
        );

        let (tx, rx) = std::sync::mpsc::channel();

        std::thread::spawn(move || {
            let waker: Waker = rx.recv().unwrap();
            drop(rx); // Drop the receiver before waking the task
            waker.wake();
        });

        runtime.block_on(poll_fn(move |cx| {
            if tx.send(cx.waker().clone()).is_ok() {
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        }));
    }

    #[cfg(windows)]
    #[test]
    fn test_interrupt_iocp() {
        let runtime = crate::vibeio::executor::Runtime::new(
            AnyDriver::new_iocp().expect("Failed to create IocpDriver"),
        );

        let (tx, rx) = std::sync::mpsc::channel();

        std::thread::spawn(move || {
            let waker: Waker = rx.recv().unwrap();
            drop(rx); // Drop the receiver before waking the task
            waker.wake();
        });

        runtime.block_on(poll_fn(move |cx| {
            if tx.send(cx.waker().clone()).is_ok() {
                Poll::Pending
            } else {
                Poll::Ready(())
            }
        }));
    }
}
