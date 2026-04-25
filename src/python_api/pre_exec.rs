use std::process::Command;

#[cfg(unix)]
use std::os::unix::process::CommandExt;

use super::UnixPreExecConfig;

#[cfg(unix)]
pub(super) fn apply(command: &mut Command, config: UnixPreExecConfig) {
    // SAFETY: `pre_exec` installs a closure that runs in the child process after fork and before
    // exec. The closure only invokes async-signal-safe libc operations and returns OS errors.
    unsafe {
        command.pre_exec(move || apply_in_child(&config));
    }
}

#[cfg(not(unix))]
pub(super) fn apply(_command: &mut Command, _config: UnixPreExecConfig) {}

#[cfg(unix)]
fn apply_in_child(config: &UnixPreExecConfig) -> std::io::Result<()> {
    // SAFETY: This runs inside the `pre_exec` child process hook. The callees only use libc calls
    // that are valid in that narrow post-fork, pre-exec window and report failures via errno.
    unsafe {
        restore_child_signals(config.restore_signals)?;
        clear_pass_fds_cloexec(&config.pass_fds)?;
        apply_child_attributes(config)
    }
}

#[cfg(unix)]
/// SAFETY: Must only be called from the child process between fork and exec.
unsafe fn restore_child_signals(restore_signals: bool) -> std::io::Result<()> {
    if !restore_signals {
        return Ok(());
    }

    if libc::signal(libc::SIGPIPE, libc::SIG_DFL) == libc::SIG_ERR {
        return Err(std::io::Error::last_os_error());
    }
    #[cfg(any(target_os = "linux", target_os = "android"))]
    if libc::signal(libc::SIGXFSZ, libc::SIG_DFL) == libc::SIG_ERR {
        return Err(std::io::Error::last_os_error());
    }

    Ok(())
}

#[cfg(unix)]
/// SAFETY: Must only be called from the child process between fork and exec.
unsafe fn clear_pass_fds_cloexec(pass_fds: &[i32]) -> std::io::Result<()> {
    for fd in pass_fds {
        let flags = libc::fcntl(*fd, libc::F_GETFD);
        if flags == -1 {
            return Err(std::io::Error::last_os_error());
        }
        if libc::fcntl(*fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) == -1 {
            return Err(std::io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(unix)]
/// SAFETY: Must only be called from the child process between fork and exec.
unsafe fn apply_child_attributes(config: &UnixPreExecConfig) -> std::io::Result<()> {
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

    if config.start_new_session && libc::setsid() == -1 {
        return Err(std::io::Error::last_os_error());
    }
    if let Some(process_group) = config.process_group {
        if !(config.start_new_session && process_group == 0)
            && libc::setpgid(0, process_group) == -1
        {
            return Err(std::io::Error::last_os_error());
        }
    }
    if let Some(groups) = &config.extra_groups {
        if libc::setgroups(ngroups.expect("extra_groups present"), groups.as_ptr()) == -1 {
            return Err(std::io::Error::last_os_error());
        }
    }
    if let Some(gid) = config.gid {
        if libc::setgid(gid) == -1 {
            return Err(std::io::Error::last_os_error());
        }
    }
    if let Some(uid) = config.uid {
        if libc::setuid(uid) == -1 {
            return Err(std::io::Error::last_os_error());
        }
    }
    if let Some(umask) = config.umask {
        libc::umask(umask as libc::mode_t);
    }

    Ok(())
}
