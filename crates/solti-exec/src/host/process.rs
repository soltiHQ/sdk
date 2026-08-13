//! # Inherited process state
//!
//! [`ProcessConfig`] controls Unix state inherited across `fork` and `execve`.
//!
//! ## Flow
//!
//! ```text
//! ProcessConfig
//!      │ backend construction
//!      ▼
//! validate values and signal numbers
//!      │ pre_exec
//!      ▼
//! reset signals → create session → set umask
//! ```
//!
//! All settings are explicit.
//! An empty configuration leaves backend spawn behavior unchanged.

#[cfg(feature = "host-process")]
use std::process::Command;
#[cfg(all(feature = "host-process", unix))]
use std::sync::Arc;

#[cfg(feature = "host-process")]
use super::HostProcessError;
#[cfg(feature = "host-process")]
use crate::isolation::validate_umask;

/// Unix process state applied before `execve`.
///
/// A non-empty configuration requires Unix.
#[derive(Debug, Clone, Default)]
pub struct ProcessConfig {
    /// Resets inherited signal dispositions and clears the signal mask.
    ///
    /// `false` preserves the platform spawn behavior.
    pub reset_signals: bool,
    /// Creates a new session and process group with `setsid`.
    ///
    /// A backend must not also call `setpgid` for the same child.
    pub new_session: bool,
    /// File creation mask applied to the child.
    ///
    /// `None` preserves the inherited mask.
    /// Values may contain only permission bits `0o000..=0o777`.
    pub umask: Option<u32>,
}

impl ProcessConfig {
    /// Returns `true` when no process-state control is configured.
    pub fn is_empty(&self) -> bool {
        !self.reset_signals && !self.new_session && self.umask.is_none()
    }

    #[cfg(feature = "host-process")]
    pub(crate) fn prepare(&self) -> Result<PreparedProcessConfig, HostProcessError> {
        if let Some(mask) = self.umask {
            validate_umask(mask).map_err(HostProcessError::InvalidConfig)?;
        }

        #[cfg(unix)]
        {
            let reset_signals = if self.reset_signals {
                Arc::from(unix_impl::valid_signal_numbers()?.into_boxed_slice())
            } else {
                Arc::from([])
            };
            Ok(PreparedProcessConfig {
                reset_signals,
                new_session: self.new_session,
                umask: self.umask,
            })
        }

        #[cfg(not(unix))]
        {
            if !self.is_empty() {
                return Err(HostProcessError::InvalidConfig(format!(
                    "process settings are not supported on {}",
                    std::env::consts::OS
                )));
            }
            Ok(PreparedProcessConfig {})
        }
    }
}

/// Process state prepared before a child is forked.
#[cfg(feature = "host-process")]
#[derive(Debug)]
pub(crate) struct PreparedProcessConfig {
    #[cfg(unix)]
    reset_signals: Arc<[libc::c_int]>,
    #[cfg(unix)]
    new_session: bool,
    #[cfg(unix)]
    umask: Option<u32>,
}

#[cfg(feature = "host-process")]
impl PreparedProcessConfig {
    #[cfg(feature = "subprocess")]
    pub(crate) fn starts_new_session(&self) -> bool {
        #[cfg(unix)]
        {
            self.new_session
        }
        #[cfg(not(unix))]
        {
            false
        }
    }

    #[cfg(all(feature = "subprocess", target_os = "macos"))]
    pub(crate) fn reset_signals(&self) -> Arc<[libc::c_int]> {
        Arc::clone(&self.reset_signals)
    }

    #[cfg(all(feature = "subprocess", target_os = "macos"))]
    pub(crate) fn has_umask(&self) -> bool {
        self.umask.is_some()
    }
}

/// Attaches prepared inherited-state controls to a command.
#[cfg(feature = "host-process")]
pub(crate) fn attach_process_config(command: &mut Command, prepared: &PreparedProcessConfig) {
    #[cfg(unix)]
    unix_impl::attach(command, prepared);

    #[cfg(not(unix))]
    let _ = (command, prepared);
}

#[cfg(all(feature = "host-process", unix))]
mod unix_impl {
    use std::{io, os::unix::process::CommandExt as _, process::Command, sync::Arc};

    use super::PreparedProcessConfig;
    use crate::host::log::{pre_exec_log, pre_exec_log_errno};

    pub(super) fn valid_signal_numbers() -> Result<Vec<libc::c_int>, super::HostProcessError> {
        let mut signals = Vec::new();
        let signal_number_bound = libc::c_int::try_from(
            std::mem::size_of::<libc::sigset_t>() * libc::c_char::BITS as usize,
        )
        .unwrap_or(libc::c_int::MAX);
        for signal in 1..signal_number_bound {
            if signal == libc::SIGKILL || signal == libc::SIGSTOP {
                continue;
            }

            let mut current = std::mem::MaybeUninit::<libc::sigaction>::uninit();
            // SAFETY:
            // a null action queries one signal disposition into
            // `current` without changing process state.
            let result = unsafe { libc::sigaction(signal, std::ptr::null(), current.as_mut_ptr()) };
            if result == 0 {
                signals.push(signal);
                continue;
            }

            let error = io::Error::last_os_error();
            if error.raw_os_error() != Some(libc::EINVAL) {
                return Err(super::HostProcessError::Io(error));
            }
        }
        Ok(signals)
    }

    pub(super) fn attach(command: &mut Command, prepared: &PreparedProcessConfig) {
        let signals = Arc::clone(&prepared.reset_signals);
        let new_session = prepared.new_session;
        let umask = prepared.umask;

        // SAFETY:
        // all captured storage is prepared before `fork`.
        // The hook calls process-control functions without allocation.
        unsafe {
            command.pre_exec(move || {
                if !signals.is_empty() {
                    reset_signal_state(&signals)?;
                }
                if new_session && libc::setsid() < 0 {
                    return logged_last_error(b"solti-exec: setsid failed: ");
                }
                if let Some(mask) = umask {
                    libc::umask(mask as libc::mode_t);
                }
                Ok(())
            });
        }
    }

    fn reset_signal_state(signals: &[libc::c_int]) -> io::Result<()> {
        // SAFETY:
        // a zeroed action with SIG_DFL and an empty mask is a valid default disposition on supported Unix targets.
        let mut action = unsafe { std::mem::zeroed::<libc::sigaction>() };
        action.sa_sigaction = libc::SIG_DFL;
        if unsafe { libc::sigemptyset(&mut action.sa_mask) } != 0 {
            return logged_last_error(b"solti-exec: sigemptyset failed: ");
        }

        for &signal in signals {
            // SAFETY:
            // signal numbers were queried in the parent.
            // `action` contains a default disposition and an empty mask.
            if unsafe { libc::sigaction(signal, &action, std::ptr::null_mut()) } != 0 {
                return logged_last_error(b"solti-exec: sigaction reset failed: ");
            }
        }

        // Reset dispositions before unblocking inherited pending signals.
        let mut empty = unsafe { std::mem::zeroed::<libc::sigset_t>() };
        if unsafe { libc::sigemptyset(&mut empty) } != 0 {
            return logged_last_error(b"solti-exec: sigemptyset failed: ");
        }
        // SAFETY:
        // `empty` is a valid empty signal set.
        if unsafe { libc::sigprocmask(libc::SIG_SETMASK, &empty, std::ptr::null_mut()) } != 0 {
            return logged_last_error(b"solti-exec: signal mask reset failed: ");
        }
        Ok(())
    }

    fn logged_last_error(prefix: &[u8]) -> io::Result<()> {
        let error = io::Error::last_os_error();
        pre_exec_log(prefix);
        if let Some(code) = error.raw_os_error() {
            pre_exec_log_errno(code);
        }
        Err(error)
    }
}

#[cfg(all(test, feature = "host-process"))]
mod tests {
    use super::*;

    #[test]
    fn empty_config_preserves_inherited_state() {
        assert!(ProcessConfig::default().is_empty());
    }

    #[test]
    fn invalid_umask_is_rejected() {
        let error = ProcessConfig {
            umask: Some(0o1000),
            ..Default::default()
        }
        .prepare()
        .unwrap_err()
        .to_string();

        assert!(error.contains("0o000..=0o777"), "got: {error}");
    }

    #[cfg(unix)]
    #[test]
    fn configured_umask_is_applied() {
        use std::os::unix::fs::PermissionsExt as _;

        let parent = tempfile::TempDir::new().unwrap();
        let prepared = ProcessConfig {
            umask: Some(0o077),
            ..Default::default()
        }
        .prepare()
        .unwrap();
        let mut command = Command::new("sh");
        command
            .current_dir(parent.path())
            .arg("-c")
            .arg("printf test > created");
        attach_process_config(&mut command, &prepared);

        assert!(command.status().unwrap().success());
        let mode = std::fs::metadata(parent.path().join("created"))
            .unwrap()
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o600);
    }

    #[cfg(unix)]
    #[test]
    fn new_session_uses_child_pid_for_session_and_group() {
        use std::os::unix::process::CommandExt as _;

        let prepared = ProcessConfig {
            new_session: true,
            ..Default::default()
        }
        .prepare()
        .unwrap();
        let mut command = Command::new("sh");
        command.arg("-c").arg("exit 0");
        attach_process_config(&mut command, &prepared);
        // SAFETY: this test hook calls only async-signal-safe process queries.
        unsafe {
            command.pre_exec(|| {
                let pid = libc::getpid();
                if libc::getsid(0) != pid || libc::getpgid(0) != pid {
                    return Err(std::io::Error::from_raw_os_error(libc::EINVAL));
                }
                Ok(())
            });
        }

        assert!(command.status().unwrap().success());
    }

    #[cfg(unix)]
    #[test]
    fn signal_reset_replaces_ignored_disposition_and_blocked_mask() {
        use std::os::unix::{process::CommandExt as _, process::ExitStatusExt as _};

        let prepared = ProcessConfig {
            reset_signals: true,
            ..Default::default()
        }
        .prepare()
        .unwrap();
        let mut command = Command::new("sh");
        command.arg("-c").arg("kill -TERM $$; exit 97");

        unsafe {
            command.pre_exec(|| {
                let mut action = std::mem::zeroed::<libc::sigaction>();
                action.sa_sigaction = libc::SIG_IGN;
                if libc::sigemptyset(&mut action.sa_mask) != 0
                    || libc::sigaction(libc::SIGTERM, &action, std::ptr::null_mut()) != 0
                {
                    return Err(std::io::Error::last_os_error());
                }

                let mut mask = std::mem::zeroed::<libc::sigset_t>();
                if libc::sigemptyset(&mut mask) != 0
                    || libc::sigaddset(&mut mask, libc::SIGTERM) != 0
                    || libc::sigprocmask(libc::SIG_SETMASK, &mask, std::ptr::null_mut()) != 0
                {
                    return Err(std::io::Error::last_os_error());
                }
                Ok(())
            });
        }
        attach_process_config(&mut command, &prepared);

        let status = command.status().unwrap();
        assert_eq!(status.signal(), Some(libc::SIGTERM));
    }
}
