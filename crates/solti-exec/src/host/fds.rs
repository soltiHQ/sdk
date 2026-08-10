//! # Child file descriptors
//!
//! This module contains the host-process primitive for descriptor inheritance.
//! Backends decide which descriptors a child may inherit.
//! Linux marks the complete non-standard descriptor range close-on-exec.
//! Unix `fork` fallback paths combine a parent snapshot with a child-side range sweep.

use std::{io, os::fd::RawFd, os::unix::process::CommandExt as _, process::Command};

/// Applies the platform child-descriptor close-on-exec policy.
///
/// `passed_fds` contains the descriptors that must survive `execve`.
/// Standard input, output, and error are always handled by [`Command`].
///
/// The hook does not close descriptors between `fork` and `execve`.
/// This preserves the standard library's private exec-error pipe.
pub(crate) fn attach_fd_cloexec(command: &mut Command, passed_fds: &[RawFd]) -> io::Result<()> {
    let mut passed_fds = passed_fds.to_vec();
    passed_fds.sort_unstable();
    passed_fds.dedup();

    for &fd in &passed_fds {
        if fd < 3 {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("passed file descriptor must be at least 3, got {fd}"),
            ));
        }
        // SAFETY:
        // `F_GETFD` takes no third argument and dereferences no memory.
        // An invalid descriptor is reported through the return value.
        let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
        if flags < 0 {
            let error = io::Error::last_os_error();
            return Err(io::Error::new(
                error.kind(),
                format!("passed file descriptor {fd} is not available: {error}"),
            ));
        }
    }

    #[cfg(not(target_os = "linux"))]
    let inherited_fds = discover_open_fds()?;
    #[cfg(target_os = "linux")]
    let inherited_fds = Vec::new();
    #[cfg(not(target_os = "linux"))]
    let descriptor_table_size = descriptor_table_size()?;
    #[cfg(target_os = "linux")]
    let descriptor_table_size = 0;

    // SAFETY:
    // both descriptor lists are allocated before `fork`.
    // The descriptor-table bound is resolved in the parent.
    // The hook uses only `fcntl` and inline OS errors.
    unsafe {
        command.pre_exec(move || {
            mark_all_close_on_exec(&inherited_fds, descriptor_table_size)?;
            for &fd in &passed_fds {
                clear_close_on_exec(fd)?;
            }
            Ok(())
        });
    }
    Ok(())
}

/// Marks the complete non-standard descriptor range close-on-exec.
#[cfg(target_os = "linux")]
fn mark_all_close_on_exec(
    _inherited_fds: &[RawFd],
    _descriptor_table_size: RawFd,
) -> io::Result<()> {
    // SAFETY:
    // `close_range` receives only integer arguments and accesses no Rust memory.
    let result = unsafe {
        libc::syscall(
            libc::SYS_close_range,
            3u32,
            u32::MAX,
            libc::CLOSE_RANGE_CLOEXEC,
        )
    };
    if result != 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

/// Marks the current child range and the parent snapshot close-on-exec.
#[cfg(not(target_os = "linux"))]
fn mark_all_close_on_exec(inherited_fds: &[RawFd], descriptor_table_size: RawFd) -> io::Result<()> {
    // macOS does not provide `close_range(CLOSE_RANGE_CLOEXEC)`.
    // The range sweep covers descriptors opened after the parent snapshot.
    // The snapshot still covers an inherited descriptor above a subsequently lowered limit.
    for fd in 3..descriptor_table_size {
        mark_close_on_exec_if_open(fd)?;
    }
    for &fd in inherited_fds {
        if fd >= descriptor_table_size {
            mark_close_on_exec_if_open(fd)?;
        }
    }
    Ok(())
}

/// Resolves the descriptor-table bound before `fork`.
#[cfg(not(target_os = "linux"))]
fn descriptor_table_size() -> io::Result<RawFd> {
    // SAFETY:
    // `getdtablesize` takes no arguments and writes no caller-owned memory.
    let descriptor_table_size = unsafe { libc::getdtablesize() };
    if descriptor_table_size < 0 {
        Err(io::Error::last_os_error())
    } else {
        Ok(descriptor_table_size)
    }
}

/// Marks one descriptor close-on-exec when it is still open in the child.
#[cfg(not(target_os = "linux"))]
fn mark_close_on_exec_if_open(fd: RawFd) -> io::Result<()> {
    if fd < 3 {
        return Ok(());
    }
    // SAFETY:
    // `F_GETFD` takes no third argument and dereferences no memory.
    // A closed descriptor is reported through the return value.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        let error = io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::EBADF) {
            return Ok(());
        }
        return Err(error);
    }
    if flags & libc::FD_CLOEXEC == 0 {
        // SAFETY:
        // `F_SETFD` expects the integer flag value supplied here.
        if unsafe { libc::fcntl(fd, libc::F_SETFD, flags | libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

/// Takes a bounded snapshot of descriptors inherited on Unix without
/// `close_range(CLOSE_RANGE_CLOEXEC)`.
#[cfg(not(target_os = "linux"))]
fn discover_open_fds() -> io::Result<Vec<RawFd>> {
    let mut fds = Vec::new();
    for entry in std::fs::read_dir("/dev/fd")? {
        let entry = entry?;
        let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
            continue;
        };
        let Ok(fd) = name.parse::<RawFd>() else {
            continue;
        };
        fds.push(fd);
    }
    fds.sort_unstable();
    fds.dedup();
    Ok(fds)
}

/// Makes one validated descriptor survive `execve`.
fn clear_close_on_exec(fd: RawFd) -> io::Result<()> {
    // SAFETY:
    // `F_GETFD` takes no third argument and dereferences no memory.
    // An invalid descriptor is reported through the return value.
    let flags = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if flags < 0 {
        return Err(io::Error::last_os_error());
    }
    if flags & libc::FD_CLOEXEC != 0 {
        // SAFETY:
        // `F_SETFD` expects the integer flag value supplied here.
        // An invalid descriptor is reported through the return value.
        if unsafe { libc::fcntl(fd, libc::F_SETFD, flags & !libc::FD_CLOEXEC) } < 0 {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::{os::fd::AsRawFd as _, process::Stdio};

    use super::*;

    #[test]
    fn descriptors_are_closed_unless_passed() {
        let file = tempfile::tempfile().unwrap();
        let fd = file.as_raw_fd();
        let path = format!("/dev/fd/{fd}");

        let mut denied = Command::new("test");
        denied.args(["-e", &path]).stdout(Stdio::null());
        attach_fd_cloexec(&mut denied, &[]).unwrap();
        assert!(!denied.status().unwrap().success());

        let mut passed = Command::new("test");
        passed.args(["-e", &path]).stdout(Stdio::null());
        attach_fd_cloexec(&mut passed, &[fd]).unwrap();
        assert!(passed.status().unwrap().success());
    }

    #[test]
    fn exec_errors_still_reach_the_parent() {
        let mut command = Command::new("/definitely/missing/solti-command");
        attach_fd_cloexec(&mut command, &[]).unwrap();

        let error = command.spawn().unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::NotFound);
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn descriptor_snapshot_contains_open_descriptors_without_duplicates() {
        let file = tempfile::tempfile().unwrap();
        let fd = file.as_raw_fd();

        let snapshot = discover_open_fds().unwrap();
        assert!(snapshot.contains(&fd));
        assert!(snapshot.windows(2).all(|pair| pair[0] < pair[1]));
    }

    #[cfg(not(target_os = "linux"))]
    #[test]
    fn descriptor_opened_after_policy_attachment_is_not_inherited() {
        let mut command = Command::new("test");
        attach_fd_cloexec(&mut command, &[]).unwrap();

        let file = tempfile::tempfile().unwrap();
        let fd = file.as_raw_fd();
        clear_close_on_exec(fd).unwrap();
        let path = format!("/dev/fd/{fd}");

        command.args(["-e", &path]).stdout(Stdio::null());
        assert!(!command.status().unwrap().success());
    }
}
