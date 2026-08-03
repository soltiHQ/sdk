//! # Linux cgroup v2
//!
//! [`CgroupLimits`] controls CPU, memory, processes, and threads for one attempt.
//!
//! ## Flow
//!
//! ```text
//! backend construction
//!      └── resolve and pin cgroup parent
//!                 ▼
//!          execution attempt
//!      ├── create child cgroup
//!      ├── pin directory and control files
//!      ├── set `cgroup.max.depth` to zero
//!      ├── write configured limits
//!      ├── join before execve
//!      ├── terminate through pinned `cgroup.kill`
//!      └── verify empty and remove by owned identity
//! ```
//!
//! An explicit parent must be an existing cgroup v2 directory.
//! Without one, the current process cgroup is used.
//! Workloads must not have write access to that parent.

use std::path::{Path, PathBuf};

use crate::isolation::CgroupLimits;
#[cfg(target_os = "linux")]
use crate::isolation::CpuMax;

#[cfg(all(feature = "host-process", unix))]
use std::fs::File;
#[cfg(all(test, feature = "host-process"))]
use std::fs::OpenOptions;

#[cfg(feature = "host-process")]
use std::process::Command;

#[cfg(feature = "host-process")]
use super::HostProcessError;

/// Result of an attempt to terminate a process domain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
#[must_use = "tree termination can be unavailable and must be handled"]
pub enum DomainTermination {
    /// The prepared cgroup subtree accepted its termination request.
    Requested,
    /// This prepared domain has no cgroup-wide termination primitive.
    Unavailable,
}

/// Resolved and pinned cgroup v2 parent.
#[cfg(feature = "host-process")]
#[derive(Debug)]
pub(crate) struct PreparedCgroupParent {
    #[cfg(target_os = "linux")]
    path: PathBuf,
    #[cfg(target_os = "linux")]
    directory: File,
}

/// Cgroup prepared before the host process is forked.
#[cfg(feature = "host-process")]
pub(crate) struct PreparedCgroup {
    path: PathBuf,
    #[cfg(target_os = "linux")]
    procs: Option<File>,
    #[cfg(target_os = "linux")]
    directory: Option<File>,
    #[cfg(target_os = "linux")]
    events: Option<File>,
    #[cfg(target_os = "linux")]
    parent: Option<File>,
    #[cfg(target_os = "linux")]
    name: Option<std::ffi::CString>,
    #[cfg(unix)]
    kill: Option<File>,
    cleanup_on_drop: bool,
}

#[cfg(feature = "host-process")]
impl PreparedCgroup {
    /// Returns the cgroup directory.
    pub(crate) fn path(&self) -> &Path {
        &self.path
    }

    #[cfg(target_os = "linux")]
    fn into_parts(mut self) -> (File, CgroupDomain) {
        self.cleanup_on_drop = false;
        let procs = self
            .procs
            .take()
            .expect("prepared cgroup must own cgroup.procs");
        let domain = CgroupDomain {
            path: Some(std::mem::take(&mut self.path)),
            directory: self.directory.take(),
            events: self.events.take(),
            parent: self.parent.take(),
            name: self.name.take(),
            kill: self.kill.take(),
            termination_requested: false,
        };
        (procs, domain)
    }

    #[cfg(not(target_os = "linux"))]
    fn into_domain(mut self) -> CgroupDomain {
        self.cleanup_on_drop = false;
        CgroupDomain {
            path: Some(std::mem::take(&mut self.path)),
            #[cfg(target_os = "linux")]
            directory: self.directory.take(),
            #[cfg(target_os = "linux")]
            events: self.events.take(),
            #[cfg(target_os = "linux")]
            parent: self.parent.take(),
            #[cfg(target_os = "linux")]
            name: self.name.take(),
            #[cfg(unix)]
            kill: self.kill.take(),
            termination_requested: false,
        }
    }
}

/// Resources that identify one attached cgroup.
#[derive(Debug)]
pub(crate) struct CgroupDomain {
    path: Option<PathBuf>,
    #[cfg(all(feature = "host-process", target_os = "linux"))]
    directory: Option<File>,
    #[cfg(all(feature = "host-process", target_os = "linux"))]
    events: Option<File>,
    #[cfg(all(feature = "host-process", target_os = "linux"))]
    parent: Option<File>,
    #[cfg(all(feature = "host-process", target_os = "linux"))]
    name: Option<std::ffi::CString>,
    #[cfg(all(feature = "host-process", unix))]
    kill: Option<File>,
    termination_requested: bool,
}

impl CgroupDomain {
    pub(crate) fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    pub(crate) fn terminate_tree(&mut self) -> std::io::Result<DomainTermination> {
        if self.termination_requested {
            return Ok(DomainTermination::Requested);
        }

        #[cfg(all(feature = "host-process", unix))]
        let Some(kill) = self.kill.as_ref() else {
            return Ok(DomainTermination::Unavailable);
        };
        #[cfg(not(all(feature = "host-process", unix)))]
        return Ok(DomainTermination::Unavailable);

        #[cfg(all(feature = "host-process", unix))]
        {
            write_kill(kill)?;
            self.termination_requested = true;
            Ok(DomainTermination::Requested)
        }
    }

    pub(crate) fn is_populated(&self) -> std::io::Result<Option<bool>> {
        #[cfg(all(feature = "host-process", target_os = "linux"))]
        {
            let Some(events) = self.events.as_ref() else {
                return Ok(None);
            };
            read_populated(events).map(Some)
        }

        #[cfg(not(all(feature = "host-process", target_os = "linux")))]
        Ok(None)
    }

    pub(crate) fn cleanup(&mut self) -> std::io::Result<()> {
        let Some(path) = self.path.as_deref() else {
            return Ok(());
        };
        if self.is_populated()? == Some(true) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::WouldBlock,
                "cgroup still contains live processes",
            ));
        }

        #[cfg(all(feature = "host-process", target_os = "linux"))]
        let cleanup = match (
            self.directory.as_ref(),
            self.parent.as_ref(),
            self.name.as_ref(),
        ) {
            (Some(directory), Some(parent), Some(name)) => {
                cleanup_owned_cgroup(parent, name, directory, path)
            }
            _ => {
                #[cfg(test)]
                {
                    cleanup_cgroup(path)
                }
                #[cfg(not(test))]
                {
                    Err(missing_cgroup_identity(path))
                }
            }
        };
        #[cfg(not(all(feature = "host-process", target_os = "linux")))]
        let cleanup = cleanup_cgroup(path);

        match cleanup {
            Ok(()) => {
                self.path = None;
                #[cfg(all(feature = "host-process", target_os = "linux"))]
                {
                    self.directory = None;
                    self.events = None;
                    self.parent = None;
                    self.name = None;
                }
                #[cfg(all(feature = "host-process", unix))]
                {
                    self.kill = None;
                }
                Ok(())
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                self.path = None;
                #[cfg(all(feature = "host-process", target_os = "linux"))]
                {
                    self.directory = None;
                    self.events = None;
                    self.parent = None;
                    self.name = None;
                }
                #[cfg(all(feature = "host-process", unix))]
                {
                    self.kill = None;
                }
                Ok(())
            }
            Err(error) => Err(error),
        }
    }

    #[cfg(test)]
    pub(crate) fn for_test(path: PathBuf) -> Self {
        Self {
            path: Some(path),
            #[cfg(all(feature = "host-process", target_os = "linux"))]
            directory: None,
            #[cfg(all(feature = "host-process", target_os = "linux"))]
            events: None,
            #[cfg(all(feature = "host-process", target_os = "linux"))]
            parent: None,
            #[cfg(all(feature = "host-process", target_os = "linux"))]
            name: None,
            #[cfg(all(feature = "host-process", unix))]
            kill: None,
            termination_requested: false,
        }
    }
}

#[cfg(all(test, feature = "host-process", unix))]
fn open_kill(path: &Path) -> std::io::Result<Option<File>> {
    match OpenOptions::new()
        .write(true)
        .open(path.join("cgroup.kill"))
    {
        Ok(file) => Ok(Some(file)),
        Err(error) if error.raw_os_error() == Some(libc::ENOENT) => Ok(None),
        Err(error) => Err(error),
    }
}

#[cfg(all(test, feature = "host-process"))]
fn set_max_depth(path: &Path) -> std::io::Result<()> {
    use std::io::Write as _;

    let mut max_depth = OpenOptions::new()
        .write(true)
        .open(path.join("cgroup.max.depth"))?;
    max_depth.write_all(b"0\n")
}

#[cfg(all(feature = "host-process", unix))]
fn write_kill(file: &File) -> std::io::Result<()> {
    use std::os::fd::AsRawFd as _;

    loop {
        // SAFETY: `file` owns a writable descriptor and the one-byte buffer is valid.
        let written = unsafe { libc::write(file.as_raw_fd(), b"1".as_ptr().cast(), 1) };
        if written == 1 {
            return Ok(());
        }
        if written == 0 {
            return Err(std::io::Error::from_raw_os_error(libc::EIO));
        }

        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::EINTR) {
            return Err(error);
        }
    }
}

#[cfg(feature = "host-process")]
impl Drop for PreparedCgroup {
    fn drop(&mut self) {
        if !self.cleanup_on_drop {
            return;
        }
        #[cfg(target_os = "linux")]
        let cleanup = match (
            self.parent.as_ref(),
            self.name.as_ref(),
            self.directory.as_ref(),
        ) {
            (Some(parent), Some(name), Some(directory)) => {
                cleanup_owned_cgroup(parent, name, directory, &self.path)
            }
            _ => {
                #[cfg(test)]
                {
                    cleanup_cgroup(&self.path)
                }
                #[cfg(not(test))]
                {
                    Err(missing_cgroup_identity(&self.path))
                }
            }
        };
        #[cfg(not(target_os = "linux"))]
        let cleanup = cleanup_cgroup(&self.path);

        if let Err(error) = cleanup
            && error.kind() != std::io::ErrorKind::NotFound
        {
            tracing::warn!(
                cgroup = %self.path.display(),
                error = %error,
                "failed to clean up unused prepared cgroup",
            );
        }
    }
}

/// Resolves an explicit parent or the current process cgroup.
#[cfg(feature = "host-process")]
pub(crate) fn resolve_cgroup_parent(
    explicit: Option<&Path>,
) -> Result<PreparedCgroupParent, HostProcessError> {
    #[cfg(target_os = "linux")]
    {
        linux_impl::resolve_parent(explicit)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = explicit;
        Err(HostProcessError::InvalidConfig(format!(
            "cgroup v2 is not supported on {}",
            std::env::consts::OS
        )))
    }
}

/// Creates an attempt cgroup and applies its limits.
#[cfg(feature = "host-process")]
pub(crate) fn prepare_cgroup(
    parent: &PreparedCgroupParent,
    name: &str,
    limits: &CgroupLimits,
) -> Result<PreparedCgroup, HostProcessError> {
    #[cfg(target_os = "linux")]
    {
        linux_impl::prepare(parent, name, limits)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (parent, name, limits);
        Err(HostProcessError::InvalidConfig(format!(
            "cgroup v2 is not supported on {}",
            std::env::consts::OS
        )))
    }
}

/// Attaches cgroup membership to a command.
#[cfg(feature = "host-process")]
pub(crate) fn attach_cgroup(cmd: &mut Command, prepared: PreparedCgroup) -> CgroupDomain {
    #[cfg(target_os = "linux")]
    {
        linux_impl::attach(cmd, prepared)
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = cmd;
        prepared.into_domain()
    }
}

/// Removes an empty attempt cgroup.
#[cfg(any(not(target_os = "linux"), test))]
pub(crate) fn cleanup_cgroup(path: &Path) -> std::io::Result<()> {
    std::fs::remove_dir(path)
}

#[cfg(all(feature = "host-process", target_os = "linux", not(test)))]
fn missing_cgroup_identity(path: &Path) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("owned cgroup identity is incomplete: {}", path.display()),
    )
}

#[cfg(all(feature = "host-process", target_os = "linux"))]
fn read_populated(events: &File) -> std::io::Result<bool> {
    use std::os::unix::fs::FileExt as _;

    let mut buffer = [0u8; 4096];
    let read = events.read_at(&mut buffer, 0)?;
    if read == buffer.len() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "cgroup.events exceeds the supported size",
        ));
    }
    let contents = std::str::from_utf8(&buffer[..read]).map_err(|error| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("cgroup.events is not valid UTF-8: {error}"),
        )
    })?;
    match contents
        .lines()
        .find_map(|line| line.strip_prefix("populated ").map(str::trim))
    {
        Some("0") => Ok(false),
        Some("1") => Ok(true),
        Some(value) => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("cgroup.events has invalid populated value {value:?}"),
        )),
        None => Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "cgroup.events has no populated field",
        )),
    }
}

#[cfg(all(feature = "host-process", target_os = "linux"))]
fn cleanup_owned_cgroup(
    parent: &File,
    name: &std::ffi::CStr,
    directory: &File,
    path: &Path,
) -> std::io::Result<()> {
    use std::os::fd::AsRawFd as _;
    use std::os::unix::fs::MetadataExt as _;

    let pinned = directory.metadata()?;
    if pinned.nlink() == 0 {
        return Ok(());
    }
    let mut current = std::mem::MaybeUninit::<libc::stat>::uninit();
    // SAFETY: `parent` is open, `name` is NUL-terminated, and `current` is writable.
    let inspected = unsafe {
        libc::fstatat(
            parent.as_raw_fd(),
            name.as_ptr(),
            current.as_mut_ptr(),
            libc::AT_SYMLINK_NOFOLLOW,
        )
    };
    if inspected != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() == Some(libc::ENOENT) {
            if pinned.nlink() == 0 {
                return Ok(());
            }
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!("owned cgroup path disappeared: {}", path.display()),
            ));
        }
        return Err(error);
    }
    // SAFETY: `fstatat` initialized `current` after returning success.
    let current = unsafe { current.assume_init() };
    if current.st_dev != pinned.dev() || current.st_ino != pinned.ino() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "owned cgroup path identifies a different directory: {}",
                path.display()
            ),
        ));
    }

    // SAFETY: `parent` is open and `name` is NUL-terminated.
    let removed = unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) };
    if removed == 0 {
        return Ok(());
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ENOENT) {
        if directory.metadata()?.nlink() == 0 {
            return Ok(());
        }
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("owned cgroup path moved during cleanup: {}", path.display()),
        ));
    }
    Err(error)
}

#[cfg(all(feature = "host-process", target_os = "linux"))]
mod linux_impl {
    use std::ffi::{CString, OsString};
    use std::fs::{self, File};
    use std::io;
    use std::os::fd::{AsRawFd, FromRawFd as _};
    use std::os::unix::{
        ffi::{OsStrExt as _, OsStringExt as _},
        process::CommandExt as _,
    };
    use std::path::{Path, PathBuf};
    use std::process::Command;

    use super::super::HostProcessError;
    use super::{CgroupLimits, CpuMax, PreparedCgroup, PreparedCgroupParent};
    use crate::host::log::{pre_exec_log, pre_exec_log_errno};

    const CGROUP2_SUPER_MAGIC: u64 = 0x6367_7270;

    pub(super) fn resolve_parent(
        explicit: Option<&Path>,
    ) -> Result<PreparedCgroupParent, HostProcessError> {
        let parent = match explicit {
            Some(path) => {
                if !path.is_absolute() {
                    return Err(HostProcessError::InvalidConfig(format!(
                        "cgroup parent must be absolute: {}",
                        path.display()
                    )));
                }
                path.to_path_buf()
            }
            None => current_process_cgroup()?,
        };

        let parent = parent.canonicalize().map_err(|e| {
            HostProcessError::InvalidConfig(format!(
                "cgroup parent {} cannot be resolved: {e}",
                parent.display()
            ))
        })?;
        let directory = open_directory(&parent).map_err(|error| {
            HostProcessError::InvalidConfig(format!(
                "cgroup parent {} cannot be opened: {error}",
                parent.display()
            ))
        })?;
        validate_cgroup2_directory(&parent, &directory)?;
        Ok(PreparedCgroupParent {
            path: parent,
            directory,
        })
    }

    fn validate_cgroup2_directory(parent: &Path, directory: &File) -> Result<(), HostProcessError> {
        let mut filesystem = std::mem::MaybeUninit::<libc::statfs>::uninit();
        // SAFETY: `directory` is open and `filesystem` points to writable storage.
        if unsafe { libc::fstatfs(directory.as_raw_fd(), filesystem.as_mut_ptr()) } != 0 {
            return Err(HostProcessError::InvalidConfig(format!(
                "cgroup parent {} cannot be inspected: {}",
                parent.display(),
                io::Error::last_os_error()
            )));
        }
        // SAFETY: `statfs` initialized `filesystem` after returning success.
        let filesystem = unsafe { filesystem.assume_init() };
        if filesystem.f_type as u64 != CGROUP2_SUPER_MAGIC {
            return Err(HostProcessError::InvalidConfig(format!(
                "{} is not on a cgroup v2 filesystem",
                parent.display()
            )));
        }
        Ok(())
    }

    fn current_process_cgroup() -> Result<PathBuf, HostProcessError> {
        let membership = fs::read("/proc/self/cgroup").map_err(HostProcessError::Io)?;
        let mountinfo = fs::read("/proc/self/mountinfo").map_err(HostProcessError::Io)?;
        resolve_current_cgroup_from(&membership, &mountinfo)
    }

    fn resolve_current_cgroup_from(
        membership: &[u8],
        mountinfo: &[u8],
    ) -> Result<PathBuf, HostProcessError> {
        let membership = membership
            .split(|byte| *byte == b'\n')
            .find_map(|line| line.strip_prefix(b"0::"))
            .filter(|path| path.first() == Some(&b'/'))
            .map(|path| PathBuf::from(OsString::from_vec(path.to_vec())))
            .ok_or_else(|| {
                HostProcessError::InvalidConfig(
                    "cannot find unified cgroup v2 membership in /proc/self/cgroup".into(),
                )
            })?;

        let mut selected: Option<(bool, usize, PathBuf)> = None;
        for line in mountinfo.split(|byte| *byte == b'\n') {
            let Some(separator) = line.windows(3).position(|window| window == b" - ") else {
                continue;
            };
            let left = &line[..separator];
            let right = &line[separator + 3..];
            if right.split(|byte| *byte == b' ').next() != Some(b"cgroup2".as_slice()) {
                continue;
            }
            let mut fields = left.split(|byte| *byte == b' ');
            let Some(root) = fields.nth(3).and_then(decode_mountinfo_path) else {
                continue;
            };
            let Some(mount_point) = fields.next().and_then(decode_mountinfo_path) else {
                continue;
            };
            let Some(mount_options) = fields.next() else {
                continue;
            };
            let Ok(relative) = membership.strip_prefix(&root) else {
                continue;
            };
            let writable = !mount_options
                .split(|byte| *byte == b',')
                .any(|option| option == b"ro");
            let specificity = root.components().count();
            if selected
                .as_ref()
                .is_none_or(|(current_writable, current_specificity, _)| {
                    (writable, specificity) > (*current_writable, *current_specificity)
                })
            {
                selected = Some((writable, specificity, mount_point.join(relative)));
            }
        }

        selected.map(|(_, _, path)| path).ok_or_else(|| {
            HostProcessError::InvalidConfig(
                "cannot map unified cgroup v2 membership through /proc/self/mountinfo".into(),
            )
        })
    }

    fn decode_mountinfo_path(field: &[u8]) -> Option<PathBuf> {
        let mut decoded = Vec::with_capacity(field.len());
        let mut index = 0;
        while index < field.len() {
            if field[index] != b'\\' {
                decoded.push(field[index]);
                index += 1;
                continue;
            }
            let digits = field.get(index + 1..index + 4)?;
            if !digits.iter().all(|digit| matches!(digit, b'0'..=b'7')) {
                return None;
            }
            let value = u16::from(digits[0] - b'0') * 64
                + u16::from(digits[1] - b'0') * 8
                + u16::from(digits[2] - b'0');
            decoded.push(u8::try_from(value).ok()?);
            index += 4;
        }
        Some(PathBuf::from(OsString::from_vec(decoded)))
    }

    pub(super) fn prepare(
        parent: &PreparedCgroupParent,
        name: &str,
        limits: &CgroupLimits,
    ) -> Result<PreparedCgroup, HostProcessError> {
        let name = CString::new(name).map_err(|_| {
            HostProcessError::InvalidConfig("cgroup name contains an interior NUL byte".into())
        })?;
        let path = parent
            .path
            .join(std::ffi::OsStr::from_bytes(name.to_bytes()));
        create_directory_at(&parent.directory, &name).map_err(HostProcessError::Io)?;
        let directory = match open_directory_at(&parent.directory, &name) {
            Ok(directory) => directory,
            Err(source) => {
                if let Err(cleanup) = remove_directory_at(&parent.directory, &name) {
                    tracing::warn!(
                        cgroup = %path.display(),
                        error = %cleanup,
                        "failed to roll back unopened cgroup",
                    );
                }
                return Err(HostProcessError::Io(source));
            }
        };

        let prepared = (|| -> io::Result<PreparedCgroup> {
            set_max_depth(&directory)?;
            apply_limits(&directory, limits)?;
            let procs = open_file_at(&directory, c"cgroup.procs", libc::O_WRONLY)?;
            let procs = duplicate_child_fd(procs)?;
            let events = open_file_at(&directory, c"cgroup.events", libc::O_RDONLY)?;
            let kill = open_optional_file_at(&directory, c"cgroup.kill", libc::O_WRONLY)?;
            Ok(PreparedCgroup {
                path: path.clone(),
                procs: Some(procs),
                directory: Some(directory.try_clone()?),
                events: Some(events),
                parent: Some(parent.directory.try_clone()?),
                name: Some(name.clone()),
                kill,
                cleanup_on_drop: true,
            })
        })();

        match prepared {
            Ok(prepared) => Ok(prepared),
            Err(source) => {
                if let Err(cleanup) =
                    super::cleanup_owned_cgroup(&parent.directory, &name, &directory, &path)
                {
                    tracing::warn!(
                        cgroup = %path.display(),
                        error = %cleanup,
                        "failed to roll back cgroup setup",
                    );
                }
                Err(HostProcessError::Io(io::Error::new(
                    source.kind(),
                    format!("failed to prepare cgroup {}: {source}", path.display()),
                )))
            }
        }
    }

    /// Moves an internal child descriptor outside the standard descriptor range.
    ///
    /// The standard library replaces descriptors `0..=2` while preparing child
    /// stdio. A cgroup descriptor in that range would otherwise refer to a
    /// different file by the time the `pre_exec` hook runs.
    fn duplicate_child_fd(file: File) -> io::Result<File> {
        // SAFETY: `file` owns a valid descriptor. `F_DUPFD_CLOEXEC` creates a
        // second descriptor whose value is at least `3`.
        let raw = unsafe { libc::fcntl(file.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
        if raw < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `raw` is a newly owned descriptor returned by `fcntl`.
        Ok(unsafe { File::from_raw_fd(raw) })
    }

    fn open_directory(path: &Path) -> io::Result<File> {
        let path = CString::new(path.as_os_str().as_bytes())
            .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "path contains a NUL byte"))?;
        // SAFETY: `path` is NUL-terminated.
        let fd = unsafe {
            libc::open(
                path.as_ptr(),
                libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC,
            )
        };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` is a newly owned descriptor returned by `open`.
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    fn create_directory_at(parent: &File, name: &CString) -> io::Result<()> {
        // SAFETY: `parent` is an open directory and `name` is NUL-terminated.
        if unsafe { libc::mkdirat(parent.as_raw_fd(), name.as_ptr(), 0o755) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn open_directory_at(parent: &File, name: &CString) -> io::Result<File> {
        open_at(
            parent,
            name,
            libc::O_PATH | libc::O_DIRECTORY | libc::O_NOFOLLOW,
        )
    }

    fn open_file_at(parent: &File, name: &std::ffi::CStr, flags: libc::c_int) -> io::Result<File> {
        open_at(parent, name, flags | libc::O_NOFOLLOW)
    }

    fn open_optional_file_at(
        parent: &File,
        name: &std::ffi::CStr,
        flags: libc::c_int,
    ) -> io::Result<Option<File>> {
        match open_file_at(parent, name, flags) {
            Ok(file) => Ok(Some(file)),
            Err(error) if error.raw_os_error() == Some(libc::ENOENT) => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn open_at(parent: &File, name: &std::ffi::CStr, flags: libc::c_int) -> io::Result<File> {
        // SAFETY: `parent` is an open directory and `name` is NUL-terminated.
        let fd =
            unsafe { libc::openat(parent.as_raw_fd(), name.as_ptr(), flags | libc::O_CLOEXEC) };
        if fd < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: `fd` is a newly owned descriptor returned by `openat`.
        Ok(unsafe { File::from_raw_fd(fd) })
    }

    fn remove_directory_at(parent: &File, name: &std::ffi::CStr) -> io::Result<()> {
        // SAFETY: `parent` is an open directory and `name` is NUL-terminated.
        if unsafe { libc::unlinkat(parent.as_raw_fd(), name.as_ptr(), libc::AT_REMOVEDIR) } == 0 {
            Ok(())
        } else {
            Err(io::Error::last_os_error())
        }
    }

    fn write_control_file(directory: &File, name: &std::ffi::CStr, value: &[u8]) -> io::Result<()> {
        use std::io::Write as _;

        let mut file = open_file_at(directory, name, libc::O_WRONLY)?;
        file.write_all(value)
    }

    fn set_max_depth(directory: &File) -> io::Result<()> {
        write_control_file(directory, c"cgroup.max.depth", b"0\n")
    }

    fn apply_limits(directory: &File, limits: &CgroupLimits) -> io::Result<()> {
        if let Some(cpu) = limits.cpu {
            write_cpu_max(directory, cpu)?;
        }
        if let Some(memory) = limits.memory {
            write_control_file(directory, c"memory.max", format!("{memory}\n").as_bytes())?;
        }
        if let Some(pids) = limits.pids {
            write_control_file(directory, c"pids.max", format!("{pids}\n").as_bytes())?;
        }
        Ok(())
    }

    fn write_cpu_max(directory: &File, limit: CpuMax) -> io::Result<()> {
        let value = match limit.quota {
            Some(quota) => format!("{quota} {}\n", limit.period),
            None => format!("max {}\n", limit.period),
        };
        write_control_file(directory, c"cpu.max", value.as_bytes())
    }

    pub(super) fn attach(cmd: &mut Command, prepared: PreparedCgroup) -> super::CgroupDomain {
        let (procs, domain) = prepared.into_parts();

        // SAFETY: the hook uses only getpid and write. The open file is owned by
        // the closure in the parent and remains valid in the child until exec.
        unsafe {
            cmd.pre_exec(move || join(procs.as_raw_fd()));
        }
        domain
    }

    fn join(fd: libc::c_int) -> io::Result<()> {
        // SAFETY: getpid has no preconditions.
        let pid = unsafe { libc::getpid() };
        let mut buffer = [0u8; 24];
        let pid = format_pid(pid, &mut buffer);

        // SAFETY: fd refers to cgroup.procs and pid is a valid byte slice.
        let written = loop {
            let written = unsafe { libc::write(fd, pid.as_ptr().cast(), pid.len()) };
            if written >= 0 || io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
                break written;
            }
        };
        if written == pid.len() as isize {
            return Ok(());
        }

        let error = if written < 0 {
            io::Error::last_os_error()
        } else {
            // A cgroup membership write must consume the complete PID. Keep
            // the post-fork error path allocation-free.
            io::Error::from_raw_os_error(libc::EIO)
        };
        pre_exec_log(b"solti-exec: failed to join cgroup: ");
        if let Some(code) = error.raw_os_error() {
            pre_exec_log_errno(code);
        }
        Err(error)
    }

    fn format_pid(pid: i32, buffer: &mut [u8; 24]) -> &[u8] {
        let mut value = pid as u32;
        let mut index = buffer.len() - 1;
        buffer[index] = b'\n';
        if value == 0 {
            index -= 1;
            buffer[index] = b'0';
        } else {
            while value > 0 {
                index -= 1;
                buffer[index] = b'0' + (value % 10) as u8;
                value /= 10;
            }
        }
        &buffer[index..]
    }

    #[cfg(test)]
    mod tests {
        use std::os::fd::AsRawFd as _;

        use super::{
            decode_mountinfo_path, duplicate_child_fd, format_pid, resolve_current_cgroup_from,
            validate_cgroup2_directory,
        };

        #[test]
        fn formats_pid_for_cgroup_procs() {
            let mut buffer = [0u8; 24];
            assert_eq!(format_pid(42, &mut buffer), b"42\n");
        }

        #[test]
        fn child_descriptor_is_above_the_standard_range() {
            let file = tempfile::tempfile().unwrap();
            let duplicated = duplicate_child_fd(file).unwrap();

            assert!(duplicated.as_raw_fd() >= 3);
            let flags = unsafe { libc::fcntl(duplicated.as_raw_fd(), libc::F_GETFD) };
            assert!(flags >= 0);
            assert_ne!(flags & libc::FD_CLOEXEC, 0);
        }

        #[test]
        fn resolves_membership_against_the_matching_cgroup2_mount_root() {
            let membership = b"0::/delegated/agent/attempt\n";
            let mountinfo = b"10 1 0:10 / /legacy rw - tmpfs tmpfs rw\n\
                11 1 0:11 /delegated /run/solti\\040cgroup rw - cgroup2 cgroup rw\n";

            let resolved = resolve_current_cgroup_from(membership, mountinfo).unwrap();

            assert_eq!(
                resolved,
                std::path::Path::new("/run/solti cgroup/agent/attempt")
            );
        }

        #[test]
        fn prefers_a_writable_matching_cgroup2_mount() {
            let membership = b"0::/delegated/agent\n";
            let mountinfo = b"10 1 0:10 /delegated /run/cgroup-ro ro - cgroup2 cgroup ro\n\
                11 1 0:10 /delegated /run/cgroup-rw rw - cgroup2 cgroup rw\n";

            let resolved = resolve_current_cgroup_from(membership, mountinfo).unwrap();

            assert_eq!(resolved, std::path::Path::new("/run/cgroup-rw/agent"));
        }

        #[test]
        fn rejects_out_of_range_mountinfo_escape() {
            assert!(decode_mountinfo_path(br"/run/cgroup\777").is_none());
        }

        #[test]
        fn rejects_a_parent_outside_cgroup2() {
            let directory = tempfile::TempDir::new().unwrap();
            let pinned = super::open_directory(directory.path()).unwrap();

            let error = validate_cgroup2_directory(directory.path(), &pinned)
                .unwrap_err()
                .to_string();

            assert!(error.contains("not on a cgroup v2 filesystem"));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_limits_are_detected() {
        assert!(CgroupLimits::default().is_empty());
    }

    #[test]
    #[cfg(feature = "host-process")]
    fn unused_prepared_cgroup_is_removed_on_drop() {
        let parent = tempfile::TempDir::new().unwrap();
        let path = parent.path().join("attempt");
        std::fs::create_dir(&path).unwrap();
        let prepared = PreparedCgroup {
            path: path.clone(),
            #[cfg(target_os = "linux")]
            procs: None,
            #[cfg(target_os = "linux")]
            directory: None,
            #[cfg(target_os = "linux")]
            events: None,
            #[cfg(target_os = "linux")]
            parent: None,
            #[cfg(target_os = "linux")]
            name: None,
            #[cfg(unix)]
            kill: None,
            cleanup_on_drop: true,
        };

        drop(prepared);
        assert!(!path.exists());
    }

    #[cfg(feature = "host-process")]
    #[test]
    fn max_depth_must_exist_and_is_set_to_zero() {
        let cgroup = tempfile::TempDir::new().unwrap();
        let max_depth = cgroup.path().join("cgroup.max.depth");

        let error = set_max_depth(cgroup.path()).unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::NotFound);
        assert!(!max_depth.exists());

        std::fs::write(&max_depth, b"").unwrap();
        set_max_depth(cgroup.path()).unwrap();
        assert_eq!(std::fs::read(max_depth).unwrap(), b"0\n");
    }

    #[cfg(all(feature = "host-process", unix))]
    #[test]
    fn missing_kill_file_makes_tree_termination_unavailable() {
        let cgroup = tempfile::TempDir::new().unwrap();
        assert!(open_kill(cgroup.path()).unwrap().is_none());

        let mut domain = CgroupDomain {
            path: None,
            #[cfg(target_os = "linux")]
            directory: None,
            #[cfg(target_os = "linux")]
            events: None,
            #[cfg(target_os = "linux")]
            parent: None,
            #[cfg(target_os = "linux")]
            name: None,
            kill: None,
            termination_requested: false,
        };
        assert_eq!(
            domain.terminate_tree().unwrap(),
            DomainTermination::Unavailable
        );
    }

    #[cfg(all(feature = "host-process", unix))]
    #[test]
    fn tree_termination_writes_once() {
        let cgroup = tempfile::TempDir::new().unwrap();
        std::fs::write(cgroup.path().join("cgroup.kill"), b"").unwrap();
        let kill = open_kill(cgroup.path()).unwrap().unwrap();
        let mut domain = CgroupDomain {
            path: None,
            #[cfg(target_os = "linux")]
            directory: None,
            #[cfg(target_os = "linux")]
            events: None,
            #[cfg(target_os = "linux")]
            parent: None,
            #[cfg(target_os = "linux")]
            name: None,
            kill: Some(kill),
            termination_requested: false,
        };

        assert_eq!(
            domain.terminate_tree().unwrap(),
            DomainTermination::Requested
        );
        assert_eq!(
            domain.terminate_tree().unwrap(),
            DomainTermination::Requested
        );
        assert_eq!(
            std::fs::read(cgroup.path().join("cgroup.kill")).unwrap(),
            b"1"
        );
    }

    #[cfg(all(feature = "host-process", unix))]
    #[test]
    fn tree_termination_uses_the_pinned_kill_file() {
        let parent = tempfile::TempDir::new().unwrap();
        let cgroup = parent.path().join("attempt");
        let pinned = parent.path().join("renamed-attempt");
        std::fs::create_dir(&cgroup).unwrap();
        let kill_path = cgroup.join("cgroup.kill");
        std::fs::write(&kill_path, b"").unwrap();
        let kill = open_kill(&cgroup).unwrap().unwrap();

        std::fs::rename(&cgroup, &pinned).unwrap();
        std::fs::create_dir(&cgroup).unwrap();
        let replacement = cgroup.join("cgroup.kill");
        std::fs::write(&replacement, b"replacement").unwrap();

        let mut domain = CgroupDomain {
            path: None,
            #[cfg(target_os = "linux")]
            directory: None,
            #[cfg(target_os = "linux")]
            events: None,
            #[cfg(target_os = "linux")]
            parent: None,
            #[cfg(target_os = "linux")]
            name: None,
            kill: Some(kill),
            termination_requested: false,
        };
        assert_eq!(
            domain.terminate_tree().unwrap(),
            DomainTermination::Requested
        );
        assert_eq!(std::fs::read(pinned.join("cgroup.kill")).unwrap(), b"1");
        assert_eq!(std::fs::read(replacement).unwrap(), b"replacement");
    }

    #[cfg(all(feature = "host-process", target_os = "linux"))]
    #[test]
    fn populated_cgroup_cannot_be_cleaned_up() {
        let parent = tempfile::TempDir::new().unwrap();
        let path = parent.path().join("attempt");
        std::fs::create_dir(&path).unwrap();
        let events_path = path.join("cgroup.events");
        std::fs::write(&events_path, b"populated 1\nfrozen 0\n").unwrap();
        let mut domain = CgroupDomain {
            path: Some(path.clone()),
            directory: Some(File::open(&path).unwrap()),
            events: Some(File::open(&events_path).unwrap()),
            parent: Some(File::open(parent.path()).unwrap()),
            name: Some(std::ffi::CString::new("attempt").unwrap()),
            kill: None,
            termination_requested: false,
        };

        let error = domain.cleanup().unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::WouldBlock);
        assert!(path.exists());

        std::fs::write(&events_path, b"populated 0\nfrozen 0\n").unwrap();
        std::fs::remove_file(events_path).unwrap();
        domain.cleanup().unwrap();
        assert!(!path.exists());
    }

    #[cfg(all(feature = "host-process", target_os = "linux"))]
    #[test]
    fn cleanup_does_not_remove_a_replacement_directory() {
        let parent = tempfile::TempDir::new().unwrap();
        let path = parent.path().join("attempt");
        let moved = parent.path().join("moved-attempt");
        std::fs::create_dir(&path).unwrap();
        let directory = File::open(&path).unwrap();
        std::fs::rename(&path, &moved).unwrap();
        std::fs::create_dir(&path).unwrap();
        let mut domain = CgroupDomain {
            path: Some(path.clone()),
            directory: Some(directory),
            events: None,
            parent: Some(File::open(parent.path()).unwrap()),
            name: Some(std::ffi::CString::new("attempt").unwrap()),
            kill: None,
            termination_requested: false,
        };

        let error = domain.cleanup().unwrap_err();

        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
        assert!(path.exists());
        assert!(moved.exists());
        domain.path = None;
    }

    #[cfg(all(feature = "host-process", target_os = "linux"))]
    #[test]
    fn cleanup_accepts_removed_identity_without_touching_replacement() {
        let parent = tempfile::TempDir::new().unwrap();
        let path = parent.path().join("attempt");
        std::fs::create_dir(&path).unwrap();
        let directory = File::open(&path).unwrap();
        std::fs::remove_dir(&path).unwrap();
        std::fs::create_dir(&path).unwrap();
        let mut domain = CgroupDomain {
            path: Some(path.clone()),
            directory: Some(directory),
            events: None,
            parent: Some(File::open(parent.path()).unwrap()),
            name: Some(std::ffi::CString::new("attempt").unwrap()),
            kill: None,
            termination_requested: false,
        };

        domain.cleanup().unwrap();

        assert!(path.exists());
        assert!(domain.path().is_none());
    }
}
