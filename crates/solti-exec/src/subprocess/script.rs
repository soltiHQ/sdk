//! # Script transport
//!
//! Script mode transports decoded bytes through an anonymous descriptor.
//! Linux seals the descriptor before it is inherited by the interpreter.

use std::{
    io::{self, Seek as _, Write as _},
    path::{Path, PathBuf},
};

/// Attempt-scoped script backing storage.
pub(crate) struct AnonymousScript {
    #[cfg(unix)]
    file: std::fs::File,
    #[cfg(not(unix))]
    file: tempfile::NamedTempFile,
    argument_path: PathBuf,
}

impl AnonymousScript {
    /// Creates complete backing storage before the child is forked.
    pub(crate) fn create(body: &str) -> io::Result<Self> {
        #[cfg(target_os = "linux")]
        let mut file = linux::create_sealable_memfd()?;

        #[cfg(all(unix, not(target_os = "linux")))]
        let mut file = normalize_unix_fd(tempfile::tempfile()?)?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;

            file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
            file.write_all(body.as_bytes())?;
            file.flush()?;
            file.seek(io::SeekFrom::Start(0))?;

            #[cfg(target_os = "linux")]
            {
                linux::seal(&file)?;
                file.set_permissions(std::fs::Permissions::from_mode(0o444))?;
            }

            use std::os::fd::AsRawFd as _;
            let fd = file.as_raw_fd();
            #[cfg(target_os = "linux")]
            let argument_path = PathBuf::from(format!("/proc/self/fd/{fd}"));
            #[cfg(not(target_os = "linux"))]
            let argument_path = PathBuf::from(format!("/dev/fd/{fd}"));

            Ok(Self {
                file,
                argument_path,
            })
        }

        #[cfg(not(unix))]
        {
            let mut file = tempfile::NamedTempFile::with_prefix("solti-script-")?;
            file.write_all(body.as_bytes())?;
            file.flush()?;
            let argument_path = file.path().to_path_buf();
            Ok(Self {
                file,
                argument_path,
            })
        }
    }

    /// Returns the path passed as the interpreter's script argument.
    pub(crate) fn argument_path(&self) -> &Path {
        &self.argument_path
    }

    /// Returns the internal descriptor that the interpreter must inherit.
    #[cfg(unix)]
    pub(crate) fn as_raw_fd(&self) -> std::os::fd::RawFd {
        use std::os::fd::AsRawFd as _;
        self.file.as_raw_fd()
    }

    /// Keeps the non-Unix named file field observably owned by this value.
    #[cfg(not(unix))]
    fn _file(&self) -> &tempfile::NamedTempFile {
        &self.file
    }
}

#[cfg(unix)]
fn normalize_unix_fd(file: std::fs::File) -> io::Result<std::fs::File> {
    use std::os::fd::{AsRawFd as _, FromRawFd as _};

    let fd = file.as_raw_fd();
    if fd >= 3 {
        return Ok(file);
    }
    // SAFETY: `fd` is owned by `file`; `fcntl` duplicates it without borrowing
    // memory and requests a descriptor outside the standard range.
    let normalized = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
    if normalized < 0 {
        return Err(io::Error::last_os_error());
    }
    drop(file);
    // SAFETY: `normalized` is a fresh descriptor returned by `fcntl` and is
    // transferred exactly once into `File`.
    Ok(unsafe { std::fs::File::from_raw_fd(normalized) })
}

#[cfg(target_os = "linux")]
mod linux {
    use std::{ffi::CString, fs::File, io, os::fd::FromRawFd as _};

    pub(super) fn create_sealable_memfd() -> io::Result<File> {
        let name = CString::new("solti-script").expect("static memfd name");
        // SAFETY: `name` is a valid NUL-terminated string and the flags are
        // accepted by `memfd_create`.
        let fd = unsafe {
            libc::memfd_create(name.as_ptr(), libc::MFD_CLOEXEC | libc::MFD_ALLOW_SEALING)
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` is a fresh descriptor returned by `memfd_create` and is
        // transferred exactly once into `File`.
        super::normalize_unix_fd(unsafe { File::from_raw_fd(fd) })
    }

    pub(super) fn seal(file: &File) -> io::Result<()> {
        use std::os::fd::AsRawFd as _;

        let seals =
            libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;
        // SAFETY: the descriptor is owned by `file`; `F_ADD_SEALS` does not
        // dereference process memory.
        if unsafe { libc::fcntl(file.as_raw_fd(), libc::F_ADD_SEALS, seals) } < 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_transport_preserves_body() {
        let script = AnonymousScript::create("echo hello").unwrap();
        let written = std::fs::read_to_string(script.argument_path()).unwrap();
        assert_eq!(written, "echo hello");
    }

    #[cfg(unix)]
    #[test]
    fn unix_script_transport_has_expected_mode() {
        use std::os::unix::fs::PermissionsExt as _;

        let script = AnonymousScript::create("echo hello").unwrap();
        let mode = script.file.metadata().unwrap().permissions().mode() & 0o777;
        #[cfg(target_os = "linux")]
        assert_eq!(mode, 0o444);
        #[cfg(not(target_os = "linux"))]
        assert_eq!(mode, 0o600);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_script_transport_is_anonymous_and_sealed() {
        use std::os::fd::AsRawFd as _;

        let script = AnonymousScript::create("echo hello").unwrap();
        let fd = script.file.as_raw_fd();
        assert!(fd >= 3);
        let target = std::fs::read_link(format!("/proc/self/fd/{fd}")).unwrap();
        assert!(
            target.to_string_lossy().contains("memfd:solti-script"),
            "unexpected memfd target: {}",
            target.display()
        );

        let expected =
            libc::F_SEAL_WRITE | libc::F_SEAL_GROW | libc::F_SEAL_SHRINK | libc::F_SEAL_SEAL;
        // SAFETY: `fd` is owned by the live script file; `F_GET_SEALS` does not
        // dereference process memory.
        let actual = unsafe { libc::fcntl(fd, libc::F_GET_SEALS) };
        assert_eq!(actual & expected, expected);

        let byte = b"x";
        // SAFETY: `fd` is valid and `byte` provides a readable buffer of the
        // exact length passed to `pwrite`.
        let written = unsafe { libc::pwrite(fd, byte.as_ptr().cast(), byte.len(), 0) };
        assert_eq!(written, -1);
        assert_eq!(io::Error::last_os_error().raw_os_error(), Some(libc::EPERM));
    }
}
