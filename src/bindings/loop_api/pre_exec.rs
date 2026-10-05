//! Child-process setup that must run between `fork` and `exec` on Unix.

#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::process::Command;

use super::process_spawn::UnixPreExecConfig;

#[cfg(unix)]
pub(super) fn apply(command: &mut Command, config: UnixPreExecConfig) {
    // SAFETY: `pre_exec` installs a closure that runs in the child process after
    // fork and before exec. The closure only invokes async-signal-safe libc
    // operations and returns OS errors.
    unsafe {
        command.pre_exec(move || apply_in_child(&config));
    }
}

#[cfg(not(unix))]
pub(super) fn apply(_command: &mut Command, _config: UnixPreExecConfig) {}

#[cfg(unix)]
fn should_set_process_group(start_new_session: bool, process_group: i32) -> bool {
    !(start_new_session && process_group == 0)
}

#[cfg(unix)]
fn apply_in_child(config: &UnixPreExecConfig) -> std::io::Result<()> {
    restore_child_signals(config.restore_signals)?;
    clear_pass_fds_cloexec(&config.pass_fds)?;
    apply_child_attributes(config)
}

#[cfg(unix)]
fn restore_child_signals(restore_signals: bool) -> std::io::Result<()> {
    if !restore_signals {
        return Ok(());
    }

    // SAFETY: this runs in the `pre_exec` child and installs a valid signal
    // disposition.
    let result = unsafe { libc::signal(libc::SIGPIPE, libc::SIG_DFL) };
    if result == libc::SIG_ERR {
        return Err(std::io::Error::last_os_error());
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    {
        // SAFETY: this runs in the `pre_exec` child and installs a valid signal
        // disposition.
        let result = unsafe { libc::signal(libc::SIGXFSZ, libc::SIG_DFL) };
        if result == libc::SIG_ERR {
            return Err(std::io::Error::last_os_error());
        }
    }

    Ok(())
}

#[cfg(unix)]
fn clear_pass_fds_cloexec(pass_fds: &[i32]) -> std::io::Result<()> {
    for fd in pass_fds {
        // SAFETY: `fd` is supplied to `pre_exec`; F_GETFD neither dereferences pointers
        // nor allocates.
        let flags = unsafe { libc::fcntl(*fd, libc::F_GETFD) };
        if flags == -1 {
            return Err(std::io::Error::last_os_error());
        }
        // SAFETY: `fd` and the flags returned above are valid inputs for F_SETFD.
        let result = unsafe { libc::fcntl(*fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) };
        if result == -1 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(unix)]
fn apply_child_attributes(config: &UnixPreExecConfig) -> std::io::Result<()> {
    #[cfg(any(target_os = "linux", target_os = "android"))]
    let ngroups = config.extra_groups.as_ref().map(Vec::len);
    #[cfg(not(any(target_os = "linux", target_os = "android")))]
    let ngroups = config
        .extra_groups
        .as_ref()
        .map(|groups| {
            groups.len().try_into().map_err(|_| {
                std::io::Error::new(
                    std::io::ErrorKind::InvalidInput,
                    "extra_groups length exceeds platform limit",
                )
            })
        })
        .transpose()?;

    if config.start_new_session {
        // SAFETY: called only inside the `pre_exec` child; `setsid` takes no pointers.
        let result = unsafe { libc::setsid() };
        if result == -1 {
            return Err(std::io::Error::last_os_error());
        }
    }
    if let Some(process_group) = config.process_group
        && should_set_process_group(config.start_new_session, process_group)
    {
        // SAFETY: called only inside the `pre_exec` child with numeric process IDs.
        let result = unsafe { libc::setpgid(0, process_group) };
        if result == -1 {
            return Err(std::io::Error::last_os_error());
        }
    }
    if let Some(groups) = &config.extra_groups {
        // SAFETY: `groups.as_ptr()` is valid for the supplied `ngroups` length during
        // the call.
        let result =
            unsafe { libc::setgroups(ngroups.expect("extra_groups present"), groups.as_ptr()) };
        if result == -1 {
            return Err(std::io::Error::last_os_error());
        }
    }
    if let Some(gid) = config.gid {
        // SAFETY: called only inside the `pre_exec` child with a numeric group ID.
        let result = unsafe { libc::setgid(gid) };
        if result == -1 {
            return Err(std::io::Error::last_os_error());
        }
    }
    if let Some(uid) = config.uid {
        // SAFETY: called only inside the `pre_exec` child with a numeric user ID.
        let result = unsafe { libc::setuid(uid) };
        if result == -1 {
            return Err(std::io::Error::last_os_error());
        }
    }
    if let Some(umask) = config.umask {
        let umask = libc::mode_t::try_from(umask).expect("validated umask fits mode_t");
        // SAFETY: called only inside the `pre_exec` child with a validated mode value.
        unsafe { libc::umask(umask) };
    }

    Ok(())
}

#[cfg(all(kani, unix))]
mod verification {
    use super::should_set_process_group;

    #[kani::proof]
    fn merge_process_group_setup_skips_only_the_setsid_zero_case() {
        let start_new_session: bool = kani::any();
        let process_group: i32 = kani::any();
        let should_set = should_set_process_group(start_new_session, process_group);

        assert_eq!(should_set, !(start_new_session && process_group == 0));
        if !should_set {
            assert!(start_new_session);
            assert_eq!(process_group, 0);
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use std::{
        os::fd::{AsRawFd, FromRawFd, OwnedFd},
        process::Stdio,
    };

    use super::*;

    fn pipe() -> (OwnedFd, OwnedFd) {
        let mut fds = [-1; 2];
        // SAFETY: `fds` points to space for the two descriptors written by `pipe`.
        assert_eq!(unsafe { libc::pipe(fds.as_mut_ptr()) }, 0);
        // SAFETY: successful `pipe` returned two newly owned descriptors.
        unsafe { (OwnedFd::from_raw_fd(fds[0]), OwnedFd::from_raw_fd(fds[1])) }
    }

    #[test]
    fn pass_fds_clears_close_on_exec_without_changing_other_flags() {
        let (_read_end, write_end) = pipe();
        let fd = write_end.as_raw_fd();
        // SAFETY: `fd` is owned for the duration of the test.
        let original = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert_ne!(original, -1);
        // SAFETY: `fd` is valid and F_SETFD accepts the returned descriptor flags.
        assert_ne!(
            unsafe { libc::fcntl(fd, libc::F_SETFD, original | libc::FD_CLOEXEC) },
            -1
        );

        clear_pass_fds_cloexec(&[fd]).expect("clear close-on-exec");

        // SAFETY: `fd` remains owned and valid.
        let updated = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        assert_eq!(updated & libc::FD_CLOEXEC, 0);
        assert_eq!(updated & !libc::FD_CLOEXEC, original & !libc::FD_CLOEXEC);
    }

    #[test]
    fn invalid_pass_fd_is_reported_before_exec() {
        let config = UnixPreExecConfig {
            restore_signals: false,
            pass_fds: vec![-1],
            ..UnixPreExecConfig::default()
        };

        let err = apply_in_child(&config).expect_err("invalid descriptor should fail");
        assert_eq!(err.raw_os_error(), Some(libc::EBADF));
    }

    #[test]
    fn pre_exec_applies_umask_and_starts_a_new_session_in_the_child_only() {
        let interpreter = std::env::var_os("PYO3_PYTHON").unwrap_or_else(|| "python3".into());
        let mut command = Command::new(interpreter);
        command
            .args([
                "-c",
                "import os; old = os.umask(0); print(f'{old:04o}'); print(os.getpid(), os.getsid(0))",
            ])
            .stdout(Stdio::piped());
        apply(
            &mut command,
            UnixPreExecConfig {
                restore_signals: false,
                start_new_session: true,
                umask: Some(0o27),
                ..UnixPreExecConfig::default()
            },
        );

        let output = command.output().expect("spawn configured child");
        assert!(
            output.status.success(),
            "child failed: status={} stderr={}",
            output.status,
            String::from_utf8_lossy(&output.stderr)
        );
        let stdout = String::from_utf8(output.stdout).expect("utf-8 child output");
        let mut lines = stdout.lines();
        assert_eq!(lines.next().expect("umask output").trim(), "0027");
        let ids = lines
            .next()
            .expect("process ids")
            .split_whitespace()
            .map(|value| value.parse::<u32>().expect("numeric process id"))
            .collect::<Vec<_>>();
        assert_eq!(ids.len(), 2);
        assert_eq!(ids[0], ids[1]);
    }
}
