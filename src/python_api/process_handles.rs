use std::fs::File;

use pyo3::exceptions::PyRuntimeError;
use pyo3::prelude::*;

use crate::fd_ops;

pub(super) fn file_from_fd(fd: fd_ops::RawFd) -> PyResult<File> {
    #[cfg(windows)]
    {
        let handle = fd_ops::duplicate_handle_from_fd(fd)
            .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
        return Ok(file_from_owned_handle(handle));
    }

    #[cfg(unix)]
    {
        let dup = fd_ops::dup_raw_fd(fd as fd_ops::RawFd)
            .map_err(|err| PyRuntimeError::new_err(err.to_string()))?;
        Ok(file_from_owned_fd(dup as i32))
    }
}

pub(super) fn new_pipe() -> PyResult<(File, File)> {
    #[cfg(windows)]
    {
        let (read_end, write_end) = create_pipe_handles()?;
        return Ok((
            file_from_owned_handle(read_end),
            file_from_owned_handle(write_end),
        ));
    }

    #[cfg(unix)]
    {
        let [read_end, write_end] = create_pipe_fds()?;
        Ok((file_from_owned_fd(read_end), file_from_owned_fd(write_end)))
    }
}

#[cfg(windows)]
fn file_from_owned_handle(handle: windows_sys::Win32::Foundation::HANDLE) -> File {
    use std::os::windows::io::FromRawHandle;

    // SAFETY: Callers pass a newly owned Windows handle. `File::from_raw_handle` takes ownership and
    // closes it exactly once.
    unsafe { File::from_raw_handle(handle as _) }
}

#[cfg(unix)]
fn file_from_owned_fd(fd: i32) -> File {
    use std::os::fd::FromRawFd;

    // SAFETY: Callers pass a newly owned file descriptor. `File::from_raw_fd` takes ownership and
    // closes it exactly once.
    unsafe { File::from_raw_fd(fd) }
}

#[cfg(windows)]
fn create_pipe_handles() -> PyResult<(
    windows_sys::Win32::Foundation::HANDLE,
    windows_sys::Win32::Foundation::HANDLE,
)> {
    use windows_sys::Win32::Foundation::HANDLE;
    use windows_sys::Win32::System::Pipes::CreatePipe;

    let mut read_end: HANDLE = std::ptr::null_mut();
    let mut write_end: HANDLE = std::ptr::null_mut();
    // SAFETY: The out-pointers are valid for writes and the security attributes pointer is null as
    // permitted by `CreatePipe`.
    let ok = unsafe { CreatePipe(&mut read_end, &mut write_end, std::ptr::null(), 0) };
    if ok == 0 {
        return Err(PyRuntimeError::new_err(
            std::io::Error::last_os_error().to_string(),
        ));
    }
    Ok((read_end, write_end))
}

#[cfg(unix)]
fn create_pipe_fds() -> PyResult<[i32; 2]> {
    let mut fds = [0_i32; 2];
    // SAFETY: `fds` has room for the two descriptors that `pipe` writes on success.
    let rc = unsafe { libc::pipe(fds.as_mut_ptr()) };
    if rc == -1 {
        return Err(PyRuntimeError::new_err(
            std::io::Error::last_os_error().to_string(),
        ));
    }
    Ok(fds)
}
