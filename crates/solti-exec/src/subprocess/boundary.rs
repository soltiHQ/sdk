//! # Subprocess boundary
//!
//! This module pins resources selected while a task is built.
//! The child uses the pinned handles instead of resolving paths again.

use std::{io, path::Path};

use tokio::process::Command;

/// Working directory pinned while a task is built.
#[derive(Debug, Clone)]
pub(crate) struct PinnedCwd {
    #[cfg(unix)]
    fd: std::sync::Arc<std::os::fd::OwnedFd>,
    #[cfg(not(unix))]
    path: std::path::PathBuf,
}

impl PinnedCwd {
    /// Opens an absolute directory without following mutable path components.
    pub(crate) fn open_absolute(path: &Path) -> io::Result<Self> {
        if !path.is_absolute() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                format!("pinned cwd must be absolute: {}", path.display()),
            ));
        }

        #[cfg(unix)]
        {
            let fd = unix::open_absolute_directory(path)?;
            Ok(Self {
                fd: std::sync::Arc::new(fd),
            })
        }

        #[cfg(not(unix))]
        {
            Ok(Self {
                path: path.to_path_buf(),
            })
        }
    }

    /// Opens a relative directory beneath an already pinned directory.
    pub(crate) fn open_beneath(&self, relative: &Path) -> io::Result<Self> {
        #[cfg(unix)]
        {
            use std::os::fd::AsRawFd as _;

            let fd = unix::open_directory_beneath(self.fd.as_raw_fd(), relative)?;
            Ok(Self {
                fd: std::sync::Arc::new(fd),
            })
        }

        #[cfg(not(unix))]
        {
            Ok(Self {
                path: self.path.join(relative),
            })
        }
    }

    /// Makes the pinned directory the child's initial working directory.
    pub(crate) fn attach_to_command(&self, command: &mut Command) {
        #[cfg(unix)]
        {
            use std::os::{fd::AsRawFd as _, unix::process::CommandExt as _};

            let pinned = std::sync::Arc::clone(&self.fd);
            // SAFETY: `pinned` keeps the directory descriptor valid for the hook.
            // `fchdir` is async-signal-safe, and the error path only reads `errno`.
            unsafe {
                command.as_std_mut().pre_exec(move || {
                    let fd = pinned.as_raw_fd();
                    if libc::fchdir(fd) != 0 {
                        return Err(io::Error::last_os_error());
                    }
                    Ok(())
                });
            }
        }

        #[cfg(not(unix))]
        command.current_dir(&self.path);
    }

    #[cfg(any(all(test, unix), target_os = "macos"))]
    pub(crate) fn as_raw_fd(&self) -> std::os::fd::RawFd {
        use std::os::fd::AsRawFd as _;
        self.fd.as_raw_fd()
    }
}

#[cfg(unix)]
mod unix {
    use std::{
        ffi::CString,
        io,
        os::{
            fd::{FromRawFd as _, OwnedFd, RawFd},
            unix::ffi::OsStrExt as _,
        },
        path::{Component, Path},
    };

    pub(super) fn open_absolute_directory(path: &Path) -> io::Result<OwnedFd> {
        let mut current = open_at(libc::AT_FDCWD, Path::new("/"))?;
        for component in path.components() {
            match component {
                Component::RootDir | Component::CurDir => {}
                Component::Normal(name) => {
                    current = open_at(current.as_raw_fd(), Path::new(name))?;
                }
                Component::ParentDir | Component::Prefix(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("unsafe absolute cwd component in {}", path.display()),
                    ));
                }
            }
        }
        Ok(current)
    }

    pub(super) fn open_directory_beneath(root_fd: RawFd, relative: &Path) -> io::Result<OwnedFd> {
        let mut current = duplicate(root_fd)?;
        for component in relative.components() {
            match component {
                Component::CurDir => {}
                Component::Normal(name) => {
                    current = open_at(current.as_raw_fd(), Path::new(name))?;
                }
                Component::RootDir | Component::ParentDir | Component::Prefix(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidInput,
                        format!("cwd path escapes its pinned root: {}", relative.display()),
                    ));
                }
            }
        }
        Ok(current)
    }

    fn open_at(parent: RawFd, name: &Path) -> io::Result<OwnedFd> {
        let name = CString::new(name.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "cwd path contains NUL"))?;
        // SAFETY: `parent` is either `AT_FDCWD` or a live directory descriptor.
        // `name` is NUL-terminated and remains alive for the call.
        let raw = unsafe {
            libc::openat(
                parent,
                name.as_ptr(),
                libc::O_RDONLY | libc::O_DIRECTORY | libc::O_CLOEXEC | libc::O_NOFOLLOW,
            )
        };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful `openat` returned a new descriptor not owned elsewhere.
        let fd = unsafe { OwnedFd::from_raw_fd(raw) };
        if raw >= 3 {
            return Ok(fd);
        }

        let normalized = duplicate(raw)?;
        drop(fd);
        Ok(normalized)
    }

    fn duplicate(fd: RawFd) -> io::Result<OwnedFd> {
        // SAFETY: `fd` remains live, and `F_DUPFD_CLOEXEC` expects an integer minimum.
        let raw = unsafe { libc::fcntl(fd, libc::F_DUPFD_CLOEXEC, 3) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful `F_DUPFD_CLOEXEC` returned a new descriptor not owned elsewhere.
        Ok(unsafe { OwnedFd::from_raw_fd(raw) })
    }

    use std::os::fd::AsRawFd as _;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn pinned_directory_survives_path_replacement() {
        let parent = tempfile::TempDir::new().unwrap();
        let original = parent.path().join("work");
        let moved = parent.path().join("moved");
        std::fs::create_dir(&original).unwrap();

        let real = original.canonicalize().unwrap();
        let pinned = PinnedCwd::open_absolute(&real).unwrap();
        assert!(pinned.as_raw_fd() >= 3);
        std::fs::rename(&original, &moved).unwrap();
        std::fs::create_dir(&original).unwrap();

        let mut command = Command::new("sh");
        command.arg("-c").arg("printf pinned > marker");
        pinned.attach_to_command(&mut command);
        assert!(command.as_std_mut().status().unwrap().success());

        assert_eq!(
            std::fs::read_to_string(moved.join("marker")).unwrap(),
            "pinned"
        );
        assert!(!original.join("marker").exists());
    }

    #[cfg(unix)]
    #[test]
    fn beneath_rejects_parent_components() {
        let parent = tempfile::TempDir::new().unwrap();
        let real = parent.path().canonicalize().unwrap();
        let pinned = PinnedCwd::open_absolute(&real).unwrap();

        let error = pinned.open_beneath(Path::new("../escape")).unwrap_err();
        assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
    }
}
