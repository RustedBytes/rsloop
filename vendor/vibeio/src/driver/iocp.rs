use std::cell::RefCell;
use std::ffi::c_void;
use std::io::{self, ErrorKind};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle, RawHandle};
use std::ptr;
use std::sync::Arc;
use std::task::Waker;
use std::time::Duration;

use mio::{Interest, Token};
use slab::Slab;
use windows_sys::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows_sys::Wdk::Storage::FileSystem::{
    FileReplaceCompletionInformation, NtCancelIoFileEx, NtCreateFile, NtSetInformationFile,
    FILE_COMPLETION_INFORMATION, FILE_OPEN,
};
use windows_sys::Wdk::System::IO::NtDeviceIoControlFile;
use windows_sys::Win32::Foundation::{
    RtlNtStatusToDosError, ERROR_ABANDONED_WAIT_0, HANDLE, INVALID_HANDLE_VALUE, NTSTATUS,
    OBJ_CASE_INSENSITIVE, UNICODE_STRING, WAIT_TIMEOUT,
};
use windows_sys::Win32::Networking::WinSock::{
    self as WinSock, INVALID_SOCKET, SIO_BASE_HANDLE, SIO_BSP_HANDLE_POLL, SOCKET, SOCKET_ERROR,
};
use windows_sys::Win32::Storage::FileSystem::{
    SetFileCompletionNotificationModes, FILE_SHARE_READ, FILE_SHARE_WRITE,
};
use windows_sys::Win32::System::IO::{
    CancelIoEx, CreateIoCompletionPort, GetQueuedCompletionStatusEx, PostQueuedCompletionStatus,
    IO_STATUS_BLOCK, OVERLAPPED, OVERLAPPED_ENTRY,
};

use crate::driver::{CompletionIoResult, Interruptor};
use crate::{
    driver::{Driver, RegistrationMode},
    fd_inner::{InnerRawHandle, RawOsHandle},
    op::Op,
};

const INTERRUPT_KEY: usize = usize::MAX;
const AFD_POLL_COMPLETION_KEY: usize = usize::MAX - 1;
const IOCP_BATCH_SIZE: usize = 128;

const STATUS_PENDING: NTSTATUS = 0x0000_0103;
const IOCTL_AFD_POLL: u32 = 0x0001_2024;
const FILE_SKIP_SET_EVENT_ON_HANDLE: u8 = 0x02;
const SYNCHRONIZE_ACCESS: u32 = 0x0010_0000;

const AFD_POLL_RECEIVE: u32 = 0x0001;
const AFD_POLL_RECEIVE_EXPEDITED: u32 = 0x0002;
const AFD_POLL_SEND: u32 = 0x0004;
const AFD_POLL_DISCONNECT: u32 = 0x0008;
const AFD_POLL_ABORT: u32 = 0x0010;
const AFD_POLL_LOCAL_CLOSE: u32 = 0x0020;
const AFD_POLL_CONNECT: u32 = 0x0040;
const AFD_POLL_ACCEPT: u32 = 0x0080;
const AFD_POLL_CONNECT_FAIL: u32 = 0x0100;

#[repr(C)]
#[derive(Clone, Copy, Default)]
struct AfdPollHandleInfo {
    handle: HANDLE,
    events: u32,
    status: NTSTATUS,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct AfdPollInfo {
    timeout: i64,
    number_of_handles: u32,
    exclusive: u32,
    handles: [AfdPollHandleInfo; 1],
}

impl AfdPollInfo {
    #[inline]
    fn new(socket: SOCKET, events: u32) -> Self {
        Self {
            timeout: i64::MAX,
            number_of_handles: 1,
            exclusive: 0,
            handles: [AfdPollHandleInfo {
                handle: socket as HANDLE,
                events,
                status: 0,
            }],
        }
    }
}

pub struct IocpInterruptor {
    port: std::sync::Weak<OwnedHandle>,
}

impl Interruptor for IocpInterruptor {
    #[inline]
    fn interrupt(&self) {
        if let Some(port) = self.port.upgrade() {
            let _ = unsafe {
                PostQueuedCompletionStatus(
                    port.as_raw_handle() as HANDLE,
                    0,
                    INTERRUPT_KEY,
                    ptr::null_mut(),
                )
            };
        }
    }
}

struct Completion {
    waiter: Option<Waker>,
    completed: Option<i32>,
    overlapped: Option<Box<OverlappedCtx>>,
    ignored_data: Option<Box<dyn std::any::Any>>,
}

#[repr(C)]
struct OverlappedCtx {
    overlapped: OVERLAPPED,
    token: usize,
}

#[repr(C)]
struct AfdIoStatusCtx {
    io_status: IO_STATUS_BLOCK,
    token: usize,
}

struct PollOp {
    registration_token: usize,
    io_status: Box<AfdIoStatusCtx>,
    _input: Box<AfdPollInfo>,
    _output: Box<AfdPollInfo>,
}

struct PollRegistration {
    socket: SOCKET,
    waiter: Option<Waker>,
    interest: Interest,
    poll_token: Option<usize>,
}

enum HandleRegistration {
    Completion,
    Poll(PollRegistration),
}

struct DriverState {
    registrations: Slab<HandleRegistration>,
    completions: Slab<Completion>,
    poll_ops: Slab<PollOp>,
}

pub struct IocpDriver {
    port: Arc<OwnedHandle>,
    afd: RefCell<Option<OwnedHandle>>,
    state: RefCell<DriverState>,
}

impl IocpDriver {
    #[inline]
    pub(crate) fn new() -> Result<Self, io::Error> {
        let port = unsafe {
            CreateIoCompletionPort(INVALID_HANDLE_VALUE as HANDLE, ptr::null_mut(), 0, 0)
        };
        if port.is_null() {
            return Err(io::Error::last_os_error());
        }

        Ok(Self {
            port: Arc::new(unsafe { OwnedHandle::from_raw_handle(port as RawHandle) }),
            afd: RefCell::new(None),
            state: RefCell::new(DriverState {
                registrations: Slab::with_capacity(1024),
                completions: Slab::with_capacity(1024),
                poll_ops: Slab::with_capacity(1024),
            }),
        })
    }

    #[inline]
    fn update_waiter(waiter_slot: &mut Option<Waker>, waker: Waker) {
        if !waiter_slot
            .as_ref()
            .is_some_and(|waiter| waiter.will_wake(&waker))
        {
            *waiter_slot = Some(waker);
        }
    }

    #[inline]
    fn iocp_handle(&self) -> HANDLE {
        self.port.as_raw_handle() as HANDLE
    }

    #[inline]
    fn status_is_success(status: NTSTATUS) -> bool {
        status >= 0
    }

    #[inline]
    fn ntstatus_to_io_error(status: NTSTATUS) -> io::Error {
        let mapped = unsafe { RtlNtStatusToDosError(status) } as i32;
        if mapped != 0 {
            io::Error::from_raw_os_error(mapped)
        } else {
            io::Error::from_raw_os_error(status)
        }
    }

    #[inline]
    fn raw_os_handle_to_windows_handle(handle: RawOsHandle) -> HANDLE {
        match handle {
            RawOsHandle::Socket(socket) => socket as HANDLE,
            RawOsHandle::Handle(handle) => handle as HANDLE,
        }
    }

    #[inline]
    fn raw_os_handle_to_socket(handle: RawOsHandle) -> Result<SOCKET, io::Error> {
        match handle {
            RawOsHandle::Socket(socket) => Ok(socket as SOCKET),
            RawOsHandle::Handle(_) => Err(io::Error::new(
                ErrorKind::Unsupported,
                "poll mode currently supports sockets only on Windows",
            )),
        }
    }

    #[inline]
    fn duration_to_timeout_ms(timeout: Option<Duration>) -> u32 {
        match timeout {
            Some(timeout) => timeout.as_millis().min(u32::MAX as u128) as u32,
            None => u32::MAX,
        }
    }

    #[inline]
    fn completion_result_from_entry(entry: &OVERLAPPED_ENTRY) -> i32 {
        if entry.Internal == 0 {
            return entry.dwNumberOfBytesTransferred as i32;
        }

        let ntstatus = entry.Internal as i32;
        let win32_error = unsafe { RtlNtStatusToDosError(ntstatus) } as i32;
        let mapped_error = if win32_error == 0 {
            ntstatus
        } else {
            win32_error
        };
        -mapped_error
    }

    #[inline]
    fn interest_to_afd_events(interest: Interest) -> u32 {
        let mut events = AFD_POLL_DISCONNECT | AFD_POLL_ABORT | AFD_POLL_LOCAL_CLOSE;

        if interest.is_readable() {
            events |= AFD_POLL_RECEIVE | AFD_POLL_RECEIVE_EXPEDITED | AFD_POLL_ACCEPT;
        }
        if interest.is_writable() {
            events |= AFD_POLL_SEND | AFD_POLL_CONNECT | AFD_POLL_CONNECT_FAIL;
        }

        if events == (AFD_POLL_DISCONNECT | AFD_POLL_ABORT | AFD_POLL_LOCAL_CLOSE) {
            events |= AFD_POLL_RECEIVE | AFD_POLL_SEND | AFD_POLL_ACCEPT | AFD_POLL_CONNECT;
        }

        events
    }

    #[inline]
    fn get_base_socket(socket: SOCKET, ioctl: u32) -> Result<SOCKET, io::Error> {
        let mut base_socket: SOCKET = INVALID_SOCKET;
        let mut bytes: u32 = 0;
        let result = unsafe {
            WinSock::WSAIoctl(
                socket,
                ioctl,
                ptr::null_mut(),
                0,
                (&mut base_socket as *mut SOCKET).cast(),
                std::mem::size_of::<SOCKET>() as u32,
                &mut bytes,
                ptr::null_mut(),
                None,
            )
        };

        if result == SOCKET_ERROR {
            let err = unsafe { WinSock::WSAGetLastError() };
            return Err(io::Error::from_raw_os_error(err));
        }

        Ok(base_socket)
    }

    #[inline]
    fn resolve_base_socket(mut socket: SOCKET) -> Result<SOCKET, io::Error> {
        loop {
            if let Ok(base_socket) = Self::get_base_socket(socket, SIO_BASE_HANDLE) {
                return Ok(base_socket);
            }

            match Self::get_base_socket(socket, SIO_BSP_HANDLE_POLL) {
                Ok(base_socket) if base_socket != INVALID_SOCKET && base_socket != socket => {
                    socket = base_socket;
                }
                Ok(_) => {
                    return Err(io::Error::new(
                        ErrorKind::Other,
                        "failed to resolve base socket for AFD polling",
                    ))
                }
                Err(err) => return Err(err),
            }
        }
    }

    #[inline]
    fn open_afd_handle() -> Result<OwnedHandle, io::Error> {
        let device_name = format!("\\Device\\Afd\\vibeio-{}", std::process::id());
        let mut device_name_utf16: Vec<u16> = device_name.encode_utf16().collect();
        let name_len = (device_name_utf16.len() * std::mem::size_of::<u16>()) as u16;

        let object_name = UNICODE_STRING {
            Length: name_len,
            MaximumLength: name_len,
            Buffer: device_name_utf16.as_mut_ptr(),
        };
        let object_attributes = OBJECT_ATTRIBUTES {
            Length: std::mem::size_of::<OBJECT_ATTRIBUTES>() as u32,
            RootDirectory: ptr::null_mut(),
            ObjectName: &object_name,
            Attributes: OBJ_CASE_INSENSITIVE,
            SecurityDescriptor: ptr::null(),
            SecurityQualityOfService: ptr::null(),
        };

        let mut afd_handle: HANDLE = ptr::null_mut();
        let mut create_status = IO_STATUS_BLOCK::default();
        let status = unsafe {
            NtCreateFile(
                &mut afd_handle,
                SYNCHRONIZE_ACCESS,
                &object_attributes,
                &mut create_status,
                ptr::null(),
                0,
                FILE_SHARE_READ | FILE_SHARE_WRITE,
                FILE_OPEN,
                0,
                ptr::null(),
                0,
            )
        };

        if !Self::status_is_success(status) {
            return Err(Self::ntstatus_to_io_error(status));
        }
        if afd_handle.is_null() {
            return Err(io::Error::new(
                ErrorKind::Other,
                "NtCreateFile returned a null AFD handle",
            ));
        }

        Ok(unsafe { OwnedHandle::from_raw_handle(afd_handle as RawHandle) })
    }

    #[inline]
    fn ensure_afd_handle(&self) -> Result<HANDLE, io::Error> {
        {
            let afd = self.afd.borrow();
            if let Some(afd) = afd.as_ref() {
                return Ok(afd.as_raw_handle() as HANDLE);
            }
        }

        let mut afd_slot = self.afd.borrow_mut();
        if afd_slot.is_none() {
            let afd = Self::open_afd_handle()?;
            let afd_handle = afd.as_raw_handle() as HANDLE;

            let completion_port = unsafe {
                CreateIoCompletionPort(afd_handle, self.iocp_handle(), AFD_POLL_COMPLETION_KEY, 0)
            };
            if completion_port.is_null() {
                return Err(io::Error::last_os_error());
            }

            if unsafe {
                SetFileCompletionNotificationModes(afd_handle, FILE_SKIP_SET_EVENT_ON_HANDLE)
            } == 0
            {
                return Err(io::Error::last_os_error());
            }

            *afd_slot = Some(afd);
        }

        Ok(afd_slot
            .as_ref()
            .expect("AFD handle must be initialized")
            .as_raw_handle() as HANDLE)
    }

    #[inline]
    fn arm_poll_operation(
        &self,
        registration_token: usize,
        socket: SOCKET,
        interest: Interest,
    ) -> Result<(), io::Error> {
        let afd_handle = self.ensure_afd_handle()?;
        let poll_events = Self::interest_to_afd_events(interest);

        let (poll_token, io_status_ptr, input_ptr, output_ptr) = {
            let mut state = self.state.borrow_mut();

            match state.registrations.get(registration_token) {
                Some(HandleRegistration::Poll(registration)) => {
                    if registration.poll_token.is_some() {
                        return Ok(());
                    }
                }
                Some(HandleRegistration::Completion) => {
                    return Err(io::Error::new(
                        ErrorKind::Unsupported,
                        format!(
                            "I/O token {} is registered for completion mode, not poll mode",
                            registration_token
                        ),
                    ));
                }
                None => {
                    return Err(io::Error::new(
                        ErrorKind::NotFound,
                        format!(
                            "I/O token {} is not registered with this driver",
                            registration_token
                        ),
                    ));
                }
            }

            let mut io_status = Box::new(unsafe { std::mem::zeroed::<AfdIoStatusCtx>() });
            let mut input = Box::new(AfdPollInfo::new(socket, poll_events));
            let mut output = Box::new(AfdPollInfo::new(socket, 0));

            let poll_entry = state.poll_ops.vacant_entry();
            let poll_token = poll_entry.key();
            io_status.token = poll_token;

            let io_status_ptr = &mut io_status.io_status as *mut IO_STATUS_BLOCK;
            let input_ptr = (&mut *input as *mut AfdPollInfo).cast();
            let output_ptr = (&mut *output as *mut AfdPollInfo).cast();

            poll_entry.insert(PollOp {
                registration_token,
                io_status,
                _input: input,
                _output: output,
            });
            if let Some(HandleRegistration::Poll(registration)) =
                state.registrations.get_mut(registration_token)
            {
                registration.poll_token = Some(poll_token);
            }

            (poll_token, io_status_ptr, input_ptr, output_ptr)
        };

        let status = unsafe {
            NtDeviceIoControlFile(
                afd_handle,
                ptr::null_mut(),
                None,
                io_status_ptr as *const c_void,
                io_status_ptr,
                IOCTL_AFD_POLL,
                input_ptr,
                std::mem::size_of::<AfdPollInfo>() as u32,
                output_ptr,
                std::mem::size_of::<AfdPollInfo>() as u32,
            )
        };

        if Self::status_is_success(status) || status == STATUS_PENDING {
            return Ok(());
        }

        let mut state = self.state.borrow_mut();
        let _ = state.poll_ops.try_remove(poll_token);
        if let Some(HandleRegistration::Poll(registration)) =
            state.registrations.get_mut(registration_token)
        {
            if registration.poll_token == Some(poll_token) {
                registration.poll_token = None;
            }
        }

        Err(Self::ntstatus_to_io_error(status))
    }

    #[inline]
    fn cancel_poll_operation(&self, poll_token: usize) {
        let afd_handle = {
            let afd = self.afd.borrow();
            let Some(afd) = afd.as_ref() else {
                return;
            };
            afd.as_raw_handle() as HANDLE
        };

        let io_status_ptr = {
            let state = self.state.borrow();
            let Some(poll_op) = state.poll_ops.get(poll_token) else {
                return;
            };
            &poll_op.io_status.io_status as *const IO_STATUS_BLOCK
        };

        let mut cancel_status = IO_STATUS_BLOCK::default();
        let _ = unsafe { NtCancelIoFileEx(afd_handle, io_status_ptr, &mut cancel_status) };
    }

    #[inline]
    fn disassociate_iocp_handle(&self, handle: &InnerRawHandle) {
        let windows_handle = Self::raw_os_handle_to_windows_handle(handle.handle);
        let info: FILE_COMPLETION_INFORMATION = FILE_COMPLETION_INFORMATION {
            Port: std::ptr::null_mut(),
            Key: std::ptr::null_mut(),
        };
        let mut status = IO_STATUS_BLOCK::default();
        let _ = unsafe {
            NtSetInformationFile(
                windows_handle,
                &mut status,
                &info as *const FILE_COMPLETION_INFORMATION as *const c_void,
                std::mem::size_of::<FILE_COMPLETION_INFORMATION>() as u32,
                FileReplaceCompletionInformation,
            )
        };
    }

    #[inline]
    fn process_entries(&self, entries: &[OVERLAPPED_ENTRY]) {
        let mut state = self.state.borrow_mut();
        for entry in entries {
            if entry.lpCompletionKey == INTERRUPT_KEY {
                continue;
            }

            if entry.lpCompletionKey == AFD_POLL_COMPLETION_KEY {
                if entry.lpOverlapped.is_null() {
                    continue;
                }

                // SAFETY: every AFD poll submission passes a pointer to AfdIoStatusCtx::io_status,
                // and AfdIoStatusCtx is repr(C) with io_status as its first field.
                let poll_token = unsafe { (*entry.lpOverlapped.cast::<AfdIoStatusCtx>()).token };
                let registration_token = state
                    .poll_ops
                    .get(poll_token)
                    .map(|poll| poll.registration_token);
                let _ = state.poll_ops.try_remove(poll_token);

                if let Some(registration_token) = registration_token {
                    if let Some(HandleRegistration::Poll(registration)) =
                        state.registrations.get_mut(registration_token)
                    {
                        if registration.poll_token == Some(poll_token) {
                            registration.poll_token = None;
                        }
                        if let Some(waiter) = registration.waiter.take() {
                            waiter.wake();
                        }
                    }
                }
                continue;
            }

            if entry.lpOverlapped.is_null() {
                continue;
            }

            // SAFETY: every OVERLAPPED pointer submitted by completion operations points to the
            // first field of OverlappedCtx (repr(C), first field), and lives in Completion::overlapped
            // until consumed here.
            let completion_token = unsafe { (*entry.lpOverlapped.cast::<OverlappedCtx>()).token };
            if let Some(completion) = state.completions.get_mut(completion_token) {
                completion.completed = Some(Self::completion_result_from_entry(entry));
                completion.overlapped = None;
                if let Some(waiter) = completion.waiter.take() {
                    waiter.wake();
                }
                if completion.ignored_data.is_some() {
                    state.completions.remove(completion_token);
                }
            }
        }
    }

    #[inline]
    fn process_batch(&self, timeout_ms: u32) -> Result<usize, io::Error> {
        let mut entries = [OVERLAPPED_ENTRY::default(); IOCP_BATCH_SIZE];
        let mut entries_removed: u32 = 0;

        let success = unsafe {
            GetQueuedCompletionStatusEx(
                self.iocp_handle(),
                entries.as_mut_ptr(),
                entries.len() as u32,
                &mut entries_removed,
                timeout_ms,
                0,
            )
        } != 0;

        if !success {
            let err = io::Error::last_os_error();
            if matches!(
                err.raw_os_error(),
                Some(code) if code == WAIT_TIMEOUT as i32 || code == ERROR_ABANDONED_WAIT_0 as i32
            ) {
                return Ok(0);
            }
            return Err(err);
        }

        if entries_removed == 0 {
            return Ok(0);
        }

        self.process_entries(&entries[..entries_removed as usize]);

        Ok(entries_removed as usize)
    }

    #[inline]
    fn process_ready_completions(&self) -> Result<(), io::Error> {
        while self.process_batch(0)? > 0 {}
        Ok(())
    }
}

impl Driver for IocpDriver {
    type Interruptor = IocpInterruptor;

    #[inline]
    fn flush(&self) {
        match self.process_ready_completions() {
            Ok(_) => {}
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => panic!("iocp flush failed while processing completions: {err}"),
        }
    }

    #[inline]
    fn wait(&self, timeout: Option<Duration>) {
        let timeout_ms = Self::duration_to_timeout_ms(timeout);
        match self.process_batch(timeout_ms) {
            Ok(processed) if processed > 0 => {
                if let Err(err) = self.process_ready_completions() {
                    panic!("iocp drain failed while waiting for I/O: {err}");
                }
            }
            Ok(_) => {}
            Err(err) if err.kind() == io::ErrorKind::Interrupted => {}
            Err(err) => panic!("iocp wait failed while waiting for I/O: {err}"),
        }
    }

    #[inline]
    fn register_handle(
        &self,
        handle: &InnerRawHandle,
        interest: Interest,
    ) -> Result<Token, io::Error> {
        self.register_handle_with_mode(handle, interest, RegistrationMode::Completion)
    }

    #[inline]
    fn register_handle_with_mode(
        &self,
        handle: &InnerRawHandle,
        interest: Interest,
        mode: RegistrationMode,
    ) -> Result<Token, io::Error> {
        match mode {
            RegistrationMode::Completion => {
                let token = {
                    let mut state = self.state.borrow_mut();
                    let entry = state.registrations.vacant_entry();
                    let token = Token(entry.key());
                    entry.insert(HandleRegistration::Completion);
                    token
                };

                let source_handle = Self::raw_os_handle_to_windows_handle(handle.handle);
                let completion_port = unsafe {
                    CreateIoCompletionPort(source_handle, self.iocp_handle(), token.0, 0)
                };
                if completion_port.is_null() {
                    let mut state = self.state.borrow_mut();
                    let _ = state.registrations.try_remove(token.0);
                    return Err(io::Error::last_os_error());
                }

                Ok(token)
            }
            RegistrationMode::Poll => {
                let socket = Self::raw_os_handle_to_socket(handle.handle)?;
                let base_socket = Self::resolve_base_socket(socket)?;
                self.ensure_afd_handle()?;

                let mut state = self.state.borrow_mut();
                let entry = state.registrations.vacant_entry();
                let token = Token(entry.key());
                entry.insert(HandleRegistration::Poll(PollRegistration {
                    socket: base_socket,
                    waiter: None,
                    interest,
                    poll_token: None,
                }));
                Ok(token)
            }
        }
    }

    #[inline]
    fn reregister_handle(
        &self,
        handle: &InnerRawHandle,
        interest: Interest,
    ) -> Result<(), io::Error> {
        let mut state = self.state.borrow_mut();
        match state.registrations.get_mut(handle.token.0) {
            Some(HandleRegistration::Completion) => Ok(()),
            Some(HandleRegistration::Poll(registration)) => {
                registration.interest = interest;
                Ok(())
            }
            None => Err(io::Error::new(
                ErrorKind::NotFound,
                format!(
                    "I/O token {} is not registered with this driver",
                    handle.token.0
                ),
            )),
        }
    }

    #[inline]
    fn deregister_handle(&self, handle: &InnerRawHandle) -> Result<(), io::Error> {
        let poll_token = {
            let mut state = self.state.borrow_mut();
            match state.registrations.try_remove(handle.token.0) {
                Some(HandleRegistration::Completion) => None,
                Some(HandleRegistration::Poll(registration)) => registration.poll_token,
                None => {
                    return Err(io::Error::new(
                        ErrorKind::NotFound,
                        format!(
                            "I/O token {} is not registered with this driver",
                            handle.token.0
                        ),
                    ))
                }
            }
        };

        if let Some(poll_token) = poll_token {
            // Poll-based I/O
            self.cancel_poll_operation(poll_token);
        } else {
            // Completion-based I/O
            self.disassociate_iocp_handle(handle);
        }

        Ok(())
    }

    #[inline]
    fn supports_completion(&self) -> bool {
        true
    }

    #[inline]
    fn submit_completion<O>(&self, op: &mut O, waker: Waker) -> CompletionIoResult
    where
        O: Op,
    {
        let (completion_token, overlapped_ptr) = {
            let mut state = self.state.borrow_mut();
            let vacant_completion = state.completions.vacant_entry();
            let completion_token = vacant_completion.key();

            let mut overlapped = Box::new(unsafe { std::mem::zeroed::<OverlappedCtx>() });
            overlapped.token = completion_token;
            let overlapped_ptr: *mut OVERLAPPED = &mut overlapped.overlapped;

            vacant_completion.insert(Completion {
                waiter: Some(waker),
                completed: None,
                overlapped: Some(overlapped),
                ignored_data: None,
            });

            (completion_token, overlapped_ptr)
        };

        if let Err(err) = op.submit_windows(overlapped_ptr) {
            let mut state = self.state.borrow_mut();
            let _ = state.completions.try_remove(completion_token);
            return CompletionIoResult::SubmitErr(err);
        }

        CompletionIoResult::Retry(completion_token)
    }

    #[inline]
    fn submit_poll(
        &self,
        handle: &InnerRawHandle,
        waker: Waker,
        interest: Interest,
    ) -> Result<(), io::Error> {
        let (socket, should_arm) = {
            let mut state = self.state.borrow_mut();
            let registration = state.registrations.get_mut(handle.token.0).ok_or_else(|| {
                io::Error::new(
                    ErrorKind::NotFound,
                    format!(
                        "I/O token {} is not registered with this driver",
                        handle.token.0
                    ),
                )
            })?;

            let HandleRegistration::Poll(registration) = registration else {
                return Err(io::Error::new(
                    ErrorKind::Unsupported,
                    format!(
                        "I/O token {} is registered for completion mode, not poll mode",
                        handle.token.0
                    ),
                ));
            };

            registration.interest = interest;
            Self::update_waiter(&mut registration.waiter, waker);
            (registration.socket, registration.poll_token.is_none())
        };

        if should_arm {
            self.arm_poll_operation(handle.token.0, socket, interest)?;
        }

        Ok(())
    }

    #[inline]
    fn get_completion_result(&self, token: usize) -> Option<i32> {
        let mut state = self.state.borrow_mut();
        let completed = state
            .completions
            .get(token)
            .and_then(|completion| completion.completed);
        if completed.is_some() {
            state.completions.remove(token);
        }
        completed
    }

    #[inline]
    fn set_completion_waker(&self, token: usize, waker: Waker) {
        let mut state = self.state.borrow_mut();
        if let Some(completion) = state.completions.get_mut(token) {
            Self::update_waiter(&mut completion.waiter, waker);
        }
    }

    #[inline]
    fn get_interruptor(&self) -> Self::Interruptor {
        IocpInterruptor {
            port: Arc::downgrade(&self.port),
        }
    }

    #[inline]
    fn ignore_completion(&self, token: usize, data: Box<dyn std::any::Any>) {
        let mut state = self.state.borrow_mut();
        if let Some(c) = state.completions.get_mut(token) {
            c.ignored_data = Some(data);
        }
    }

    #[inline]
    fn cancel_completion(&self, token: usize, handle: RawOsHandle, data: Box<dyn std::any::Any>) {
        let overlapped = {
            let mut state = self.state.borrow_mut();
            let Some(completion) = state.completions.get_mut(token) else {
                return;
            };
            completion.ignored_data = Some(data);
            completion
                .overlapped
                .as_deref_mut()
                .map(|ctx| &mut ctx.overlapped as *mut OVERLAPPED)
        };

        if let Some(overlapped) = overlapped {
            // The completion entry and ignored_data remain registered until
            // IOCP reports the aborted (or concurrently completed) operation.
            let _ =
                unsafe { CancelIoEx(Self::raw_os_handle_to_windows_handle(handle), overlapped) };
        }
    }
}
