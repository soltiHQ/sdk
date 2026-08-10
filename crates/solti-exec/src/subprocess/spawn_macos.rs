//! Native macOS subprocess creation with an atomic descriptor allowlist.

use std::{
    collections::BTreeMap,
    ffi::{CStr, CString, OsStr, OsString, c_char, c_short, c_void},
    io,
    mem::MaybeUninit,
    os::{
        fd::{AsRawFd as _, FromRawFd as _, OwnedFd, RawFd},
        unix::ffi::OsStrExt as _,
    },
    path::Path,
    pin::Pin,
    sync::OnceLock,
    task::{Context, Poll, ready},
};

use tokio::io::{AsyncRead, ReadBuf, unix::AsyncFd};

use super::{boundary::PinnedCwd, child::ProcessChild};

const POSIX_SPAWN_SETSID: c_short = 0x0400;

unsafe extern "C" {
    fn posix_spawn_file_actions_addinherit_np(
        actions: *mut libc::posix_spawn_file_actions_t,
        fd: libc::c_int,
    ) -> libc::c_int;
}

type AddFchdir =
    unsafe extern "C" fn(*mut libc::posix_spawn_file_actions_t, libc::c_int) -> libc::c_int;

pub(super) struct SpawnSpec<'a> {
    pub(super) command: &'a OsStr,
    pub(super) args: &'a [OsString],
    pub(super) env: &'a BTreeMap<OsString, OsString>,
    pub(super) cwd: Option<&'a PinnedCwd>,
    pub(super) passed_fds: &'a [RawFd],
    pub(super) reset_signals: &'a [libc::c_int],
}

/// Returns whether the pinned-cwd action exists on the running macOS version.
pub(super) fn supports(spec: &SpawnSpec<'_>) -> bool {
    spec.cwd.is_none() || addfchdir_function().is_some()
}

/// Spawns a session leader with only standard and explicitly inherited descriptors.
///
/// `None` requests the compatible `fork` fallback for an exec behavior which
/// native spawn cannot reproduce without changing task semantics.
pub(super) fn spawn(spec: &SpawnSpec<'_>) -> io::Result<Option<ProcessChild>> {
    let Some(programs) = executable_candidates(spec)? else {
        return Ok(None);
    };
    let arguments = c_arguments(spec.command, spec.args)?;
    let environment = c_environment(spec.env)?;
    let mut argv = pointer_array(&arguments);
    let mut envp = pointer_array(&environment);

    let stdout = OutputPipe::new()?;
    let stderr = OutputPipe::new()?;
    let passed_fds = validate_passed_fds(spec.passed_fds)?;
    let mut actions = FileActions::new()?;
    actions.add_open(0, c"/dev/null", libc::O_RDONLY, 0)?;
    actions.add_dup2(stdout.writer.as_raw_fd(), 1)?;
    actions.add_dup2(stderr.writer.as_raw_fd(), 2)?;

    if let Some(cwd) = spec.cwd {
        let addfchdir = addfchdir_function().ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::Unsupported,
                "pinned cwd is not supported by posix_spawn on this macOS version",
            )
        })?;
        actions.add_inherit(cwd.as_raw_fd())?;
        // SAFETY: `actions` is initialized and the pinned directory fd remains live through spawn.
        cvt_spawn(unsafe { addfchdir(actions.as_mut_ptr(), cwd.as_raw_fd()) })?;
        actions.add_close(cwd.as_raw_fd())?;
    }

    for fd in passed_fds {
        actions.add_inherit(fd)?;
    }

    let mut attrs = SpawnAttributes::new()?;
    attrs.configure_signals(spec.reset_signals)?;

    let Some(pid) = spawn_first_available(&programs, &actions, &attrs, &mut argv, &mut envp)?
    else {
        return Ok(None);
    };

    drop(stdout.writer);
    drop(stderr.writer);
    Ok(Some(ProcessChild::from_macos(
        pid,
        stdout.reader,
        stderr.reader,
    )))
}

fn executable_candidates(spec: &SpawnSpec<'_>) -> io::Result<Option<Vec<CString>>> {
    let command = spec.command.as_bytes();
    if command.contains(&b'/') {
        return Ok(Some(vec![cstring(spec.command, "command contains NUL")?]));
    }

    let path = match spec.env.get(OsStr::new("PATH")) {
        Some(path) => path.clone(),
        None => default_search_path()?,
    };
    let mut candidates = Vec::new();
    for component in path.as_os_str().as_bytes().split(|byte| *byte == b':') {
        let component = OsStr::from_bytes(component);
        if spec.cwd.is_some() && !Path::new(component).is_absolute() {
            return Ok(None);
        }
        let candidate = if component.is_empty() {
            spec.command.to_owned()
        } else {
            Path::new(component).join(spec.command).into_os_string()
        };
        candidates.push(cstring(&candidate, "executable search path contains NUL")?);
    }
    Ok(Some(candidates))
}

fn default_search_path() -> io::Result<OsString> {
    // SAFETY: a null buffer with length zero queries the required byte count.
    let size = unsafe { libc::confstr(libc::_CS_PATH, std::ptr::null_mut(), 0) };
    if size == 0 {
        return Err(io::Error::last_os_error());
    }
    let mut path = vec![0u8; size];
    // SAFETY: `path` has the size returned by the preceding query.
    let written = unsafe {
        libc::confstr(
            libc::_CS_PATH,
            path.as_mut_ptr().cast::<libc::c_char>(),
            path.len(),
        )
    };
    if written == 0 {
        return Err(io::Error::last_os_error());
    }
    path.truncate(written);
    if path.last() == Some(&0) {
        path.pop();
    }
    Ok(OsStr::from_bytes(&path).to_owned())
}

fn spawn_first_available(
    programs: &[CString],
    actions: &FileActions,
    attrs: &SpawnAttributes,
    argv: &mut [*mut c_char],
    envp: &mut [*mut c_char],
) -> io::Result<Option<libc::pid_t>> {
    for program in programs {
        let mut pid = 0;
        // SAFETY:
        // all pointers refer to initialized storage retained until the call returns;
        // argv and envp are null-terminated, and actions/attributes are initialized.
        let result = unsafe {
            libc::posix_spawn(
                &mut pid,
                program.as_ptr(),
                actions.as_ptr(),
                attrs.as_ptr(),
                argv.as_mut_ptr(),
                envp.as_mut_ptr(),
            )
        };
        match result {
            0 => return Ok(Some(pid)),
            libc::ENOENT | libc::ENOTDIR if candidate_is_missing(program) => {}
            // Darwin execvp applies additional stat and shell-fallback rules
            // for ambiguous errors. Preserve them through the fork path.
            _ => return Ok(None),
        }
    }
    Err(io::Error::from_raw_os_error(libc::ENOENT))
}

fn candidate_is_missing(program: &CStr) -> bool {
    // SAFETY: `program` is NUL-terminated and remains live for the call.
    if unsafe { libc::access(program.as_ptr(), libc::F_OK) } == 0 {
        return false;
    }
    matches!(
        io::Error::last_os_error().raw_os_error(),
        Some(libc::ENOENT) | Some(libc::ENOTDIR)
    )
}

fn c_arguments(program: &OsStr, args: &[OsString]) -> io::Result<Vec<CString>> {
    std::iter::once(program)
        .chain(args.iter().map(OsString::as_os_str))
        .map(|value| cstring(value, "process argument contains NUL"))
        .collect()
}

fn c_environment(env: &BTreeMap<OsString, OsString>) -> io::Result<Vec<CString>> {
    env.iter()
        .map(|(key, value)| {
            let key = key.as_os_str().as_bytes();
            if key.contains(&b'=') {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "environment key contains '='",
                ));
            }
            let value = value.as_os_str().as_bytes();
            let mut entry = Vec::with_capacity(key.len() + value.len() + 1);
            entry.extend_from_slice(key);
            entry.push(b'=');
            entry.extend_from_slice(value);
            CString::new(entry).map_err(|_| {
                io::Error::new(io::ErrorKind::InvalidInput, "environment contains NUL")
            })
        })
        .collect()
}

fn cstring(value: &OsStr, message: &'static str) -> io::Result<CString> {
    CString::new(value.as_bytes()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, message))
}

fn pointer_array(values: &[CString]) -> Vec<*mut c_char> {
    values
        .iter()
        .map(|value| value.as_ptr().cast_mut())
        .chain(std::iter::once(std::ptr::null_mut()))
        .collect()
}

fn validate_passed_fds(passed_fds: &[RawFd]) -> io::Result<Vec<RawFd>> {
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
        // SAFETY: `F_GETFD` only validates the numeric descriptor.
        if unsafe { libc::fcntl(fd, libc::F_GETFD) } < 0 {
            let error = io::Error::last_os_error();
            return Err(io::Error::new(
                error.kind(),
                format!("passed file descriptor {fd} is not available: {error}"),
            ));
        }
    }
    Ok(passed_fds)
}

fn addfchdir_function() -> Option<AddFchdir> {
    static ADD_FCHDIR: OnceLock<Option<AddFchdir>> = OnceLock::new();
    *ADD_FCHDIR.get_or_init(|| {
        let symbol = [
            c"posix_spawn_file_actions_addfchdir",
            c"posix_spawn_file_actions_addfchdir_np",
        ]
        .into_iter()
        .map(|name| {
            // SAFETY: `RTLD_DEFAULT` searches already loaded images and the symbol name is static.
            unsafe { libc::dlsym(libc::RTLD_DEFAULT, name.as_ptr()) }
        })
        .find(|symbol| !symbol.is_null())
        .unwrap_or(std::ptr::null_mut());
        if symbol.is_null() {
            None
        } else {
            // SAFETY: the Darwin symbol has the `AddFchdir` declaration from <spawn.h>.
            Some(unsafe { std::mem::transmute::<*mut c_void, AddFchdir>(symbol) })
        }
    })
}

fn cvt_spawn(result: libc::c_int) -> io::Result<()> {
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(result))
    }
}

struct FileActions(libc::posix_spawn_file_actions_t);

impl FileActions {
    fn new() -> io::Result<Self> {
        let mut actions = MaybeUninit::uninit();
        // SAFETY: the OS initializes `actions` on success.
        cvt_spawn(unsafe { libc::posix_spawn_file_actions_init(actions.as_mut_ptr()) })?;
        // SAFETY: initialization succeeded.
        Ok(Self(unsafe { actions.assume_init() }))
    }

    fn as_ptr(&self) -> *const libc::posix_spawn_file_actions_t {
        &self.0
    }

    fn as_mut_ptr(&mut self) -> *mut libc::posix_spawn_file_actions_t {
        &mut self.0
    }

    fn add_open(
        &mut self,
        fd: RawFd,
        path: &CStr,
        flags: i32,
        mode: libc::mode_t,
    ) -> io::Result<()> {
        // SAFETY: `self` is initialized and `path` remains live for this call.
        cvt_spawn(unsafe {
            libc::posix_spawn_file_actions_addopen(
                self.as_mut_ptr(),
                fd,
                path.as_ptr(),
                flags,
                mode,
            )
        })
    }

    fn add_dup2(&mut self, source: RawFd, target: RawFd) -> io::Result<()> {
        // SAFETY: `self` is initialized and both descriptors are numeric action operands.
        cvt_spawn(unsafe {
            libc::posix_spawn_file_actions_adddup2(self.as_mut_ptr(), source, target)
        })
    }

    fn add_inherit(&mut self, fd: RawFd) -> io::Result<()> {
        // SAFETY: `self` is initialized and the caller keeps the descriptor live through spawn.
        cvt_spawn(unsafe { posix_spawn_file_actions_addinherit_np(self.as_mut_ptr(), fd) })
    }

    fn add_close(&mut self, fd: RawFd) -> io::Result<()> {
        // SAFETY: `self` is initialized and `fd` is a numeric action operand.
        cvt_spawn(unsafe { libc::posix_spawn_file_actions_addclose(self.as_mut_ptr(), fd) })
    }
}

impl Drop for FileActions {
    fn drop(&mut self) {
        // SAFETY: `self` was initialized and is destroyed exactly once.
        let _ = unsafe { libc::posix_spawn_file_actions_destroy(self.as_mut_ptr()) };
    }
}

struct SpawnAttributes(libc::posix_spawnattr_t);

impl SpawnAttributes {
    fn new() -> io::Result<Self> {
        let mut attrs = MaybeUninit::uninit();
        // SAFETY: the OS initializes `attrs` on success.
        cvt_spawn(unsafe { libc::posix_spawnattr_init(attrs.as_mut_ptr()) })?;
        // SAFETY: initialization succeeded.
        Ok(Self(unsafe { attrs.assume_init() }))
    }

    fn as_ptr(&self) -> *const libc::posix_spawnattr_t {
        &self.0
    }

    fn as_mut_ptr(&mut self) -> *mut libc::posix_spawnattr_t {
        &mut self.0
    }

    fn configure_signals(&mut self, reset_signals: &[libc::c_int]) -> io::Result<()> {
        // Match Rust `Command`: SIGPIPE is restored to its default disposition.
        let mut defaults = unsafe { std::mem::zeroed::<libc::sigset_t>() };
        // SAFETY: `defaults` is writable signal-set storage.
        cvt_errno(unsafe { libc::sigemptyset(&mut defaults) })?;
        // SAFETY: `defaults` is initialized and SIGPIPE is valid.
        cvt_errno(unsafe { libc::sigaddset(&mut defaults, libc::SIGPIPE) })?;
        for &signal in reset_signals {
            // SAFETY: signal numbers were validated while preparing ProcessConfig.
            cvt_errno(unsafe { libc::sigaddset(&mut defaults, signal) })?;
        }
        // SAFETY: `self` and `defaults` are initialized.
        cvt_spawn(unsafe { libc::posix_spawnattr_setsigdefault(self.as_mut_ptr(), &defaults) })?;

        let mut flags = libc::POSIX_SPAWN_CLOEXEC_DEFAULT as c_short
            | libc::POSIX_SPAWN_SETSIGDEF as c_short
            | POSIX_SPAWN_SETSID;
        if !reset_signals.is_empty() {
            let mut empty = unsafe { std::mem::zeroed::<libc::sigset_t>() };
            // SAFETY: `empty` is writable signal-set storage.
            cvt_errno(unsafe { libc::sigemptyset(&mut empty) })?;
            // SAFETY: `self` and `empty` are initialized.
            cvt_spawn(unsafe { libc::posix_spawnattr_setsigmask(self.as_mut_ptr(), &empty) })?;
            flags |= libc::POSIX_SPAWN_SETSIGMASK as c_short;
        }
        // SAFETY: `self` is initialized and all flags are defined by Darwin <spawn.h>.
        cvt_spawn(unsafe { libc::posix_spawnattr_setflags(self.as_mut_ptr(), flags) })
    }
}

impl Drop for SpawnAttributes {
    fn drop(&mut self) {
        // SAFETY: `self` was initialized and is destroyed exactly once.
        let _ = unsafe { libc::posix_spawnattr_destroy(self.as_mut_ptr()) };
    }
}

fn cvt_errno(result: libc::c_int) -> io::Result<()> {
    if result == 0 {
        Ok(())
    } else {
        Err(io::Error::last_os_error())
    }
}

struct OutputPipe {
    reader: AsyncPipe,
    writer: OwnedFd,
}

impl OutputPipe {
    fn new() -> io::Result<Self> {
        let mut raw = [-1; 2];
        // SAFETY: `raw` provides space for both descriptors.
        if unsafe { libc::pipe(raw.as_mut_ptr()) } < 0 {
            return Err(io::Error::last_os_error());
        }
        // SAFETY: successful `pipe` returned two newly owned descriptors.
        let reader = unsafe { OwnedFd::from_raw_fd(raw[0]) };
        // SAFETY: successful `pipe` returned two newly owned descriptors.
        let writer = unsafe { OwnedFd::from_raw_fd(raw[1]) };
        let reader = normalize_fd(reader)?;
        let writer = normalize_fd(writer)?;
        set_fd_flag(reader.as_raw_fd(), libc::FD_CLOEXEC, true)?;
        set_fd_flag(writer.as_raw_fd(), libc::FD_CLOEXEC, true)?;
        set_status_flag(reader.as_raw_fd(), libc::O_NONBLOCK, true)?;
        Ok(Self {
            reader: AsyncPipe {
                fd: AsyncFd::new(reader)?,
            },
            writer,
        })
    }
}

fn normalize_fd(fd: OwnedFd) -> io::Result<OwnedFd> {
    if fd.as_raw_fd() >= 3 {
        return Ok(fd);
    }
    // SAFETY: the source is live and F_DUPFD_CLOEXEC creates a new descriptor.
    let raw = unsafe { libc::fcntl(fd.as_raw_fd(), libc::F_DUPFD_CLOEXEC, 3) };
    if raw < 0 {
        return Err(io::Error::last_os_error());
    }
    // SAFETY: successful duplication returned a newly owned descriptor.
    Ok(unsafe { OwnedFd::from_raw_fd(raw) })
}

fn set_fd_flag(fd: RawFd, flag: libc::c_int, enabled: bool) -> io::Result<()> {
    // SAFETY: F_GETFD only reads descriptor flags.
    let current = unsafe { libc::fcntl(fd, libc::F_GETFD) };
    if current < 0 {
        return Err(io::Error::last_os_error());
    }
    let updated = if enabled {
        current | flag
    } else {
        current & !flag
    };
    // SAFETY: F_SETFD accepts the descriptor flag word.
    if unsafe { libc::fcntl(fd, libc::F_SETFD, updated) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

fn set_status_flag(fd: RawFd, flag: libc::c_int, enabled: bool) -> io::Result<()> {
    // SAFETY: F_GETFL only reads status flags.
    let current = unsafe { libc::fcntl(fd, libc::F_GETFL) };
    if current < 0 {
        return Err(io::Error::last_os_error());
    }
    let updated = if enabled {
        current | flag
    } else {
        current & !flag
    };
    // SAFETY: F_SETFL accepts the status flag word.
    if unsafe { libc::fcntl(fd, libc::F_SETFL, updated) } < 0 {
        return Err(io::Error::last_os_error());
    }
    Ok(())
}

struct AsyncPipe {
    fd: AsyncFd<OwnedFd>,
}

impl AsyncRead for AsyncPipe {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        loop {
            let mut guard = ready!(self.fd.poll_read_ready(cx))?;
            match guard.try_io(|inner| {
                let unfilled = buffer.initialize_unfilled();
                // SAFETY: the descriptor is live and `unfilled` is writable for its full length.
                let read = unsafe {
                    libc::read(
                        inner.get_ref().as_raw_fd(),
                        unfilled.as_mut_ptr().cast(),
                        unfilled.len(),
                    )
                };
                if read < 0 {
                    return Err(io::Error::last_os_error());
                }
                buffer.advance(read as usize);
                Ok(())
            }) {
                Ok(result) => return Poll::Ready(result),
                Err(_would_block) => continue,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use std::{
        fs,
        os::fd::{AsRawFd as _, FromRawFd as _, OwnedFd},
        os::unix::fs::PermissionsExt as _,
        process::ExitStatus,
    };

    use tokio::io::AsyncReadExt as _;

    use super::*;

    fn environment() -> BTreeMap<OsString, OsString> {
        BTreeMap::from([(OsString::from("PATH"), OsString::from("/usr/bin:/bin"))])
    }

    fn duplicate_high_fd() -> OwnedFd {
        let source = std::fs::File::open("/dev/null").unwrap();
        // SAFETY: `source` is live and F_DUPFD creates a newly owned descriptor.
        let raw = unsafe { libc::fcntl(source.as_raw_fd(), libc::F_DUPFD, 200) };
        assert!(raw >= 200, "failed to allocate high descriptor");
        // SAFETY: successful F_DUPFD returned a new descriptor.
        unsafe { OwnedFd::from_raw_fd(raw) }
    }

    fn write_executable(path: &Path, body: &[u8]) {
        fs::write(path, body).unwrap();
        let mut permissions = fs::metadata(path).unwrap().permissions();
        permissions.set_mode(0o700);
        fs::set_permissions(path, permissions).unwrap();
    }

    async fn run_fd_probe(fd: RawFd, passed_fds: &[RawFd]) -> ExitStatus {
        let args = vec![
            OsString::from("-c"),
            OsString::from(format!("test -e /dev/fd/{fd}")),
        ];
        let env = environment();
        let spec = SpawnSpec {
            command: OsStr::new("/bin/sh"),
            args: &args,
            env: &env,
            cwd: None,
            passed_fds,
            reset_signals: &[],
        };
        let mut child = spawn(&spec).unwrap().unwrap();
        child.wait().await.unwrap()
    }

    #[tokio::test]
    async fn cloexec_default_closes_an_unlisted_descriptor() {
        let fd = duplicate_high_fd();
        let status = run_fd_probe(fd.as_raw_fd(), &[]).await;
        assert!(!status.success());
    }

    #[tokio::test]
    async fn inherit_action_preserves_a_listed_descriptor() {
        let fd = duplicate_high_fd();
        let status = run_fd_probe(fd.as_raw_fd(), &[fd.as_raw_fd()]).await;
        assert!(status.success());
    }

    #[tokio::test]
    async fn spawn_captures_output_and_creates_a_session() {
        let args = vec![OsString::from("native-output")];
        let env = environment();
        let spec = SpawnSpec {
            command: OsStr::new("/bin/echo"),
            args: &args,
            env: &env,
            cwd: None,
            passed_fds: &[],
            reset_signals: &[],
        };
        let mut child = spawn(&spec).unwrap().unwrap();
        let pid = child.id().unwrap() as libc::pid_t;
        // SAFETY: getsid only reads process metadata for the owned child pid.
        assert_eq!(unsafe { libc::getsid(pid) }, pid);

        let mut stdout = child.take_stdout().unwrap();
        let mut output = String::new();
        stdout.read_to_string(&mut output).await.unwrap();
        assert!(child.wait().await.unwrap().success());
        assert_eq!(output, "native-output\n");
    }

    #[tokio::test]
    async fn spawn_enters_the_pinned_working_directory() {
        let directory = tempfile::TempDir::new().unwrap();
        let canonical = directory.path().canonicalize().unwrap();
        write_executable(&canonical.join("cwd-probe"), b"#!/bin/sh\npwd\n");
        let pinned = PinnedCwd::open_absolute(&canonical).unwrap();
        let args = Vec::new();
        let env = environment();
        let spec = SpawnSpec {
            command: OsStr::new("./cwd-probe"),
            args: &args,
            env: &env,
            cwd: Some(&pinned),
            passed_fds: &[],
            reset_signals: &[],
        };
        assert!(supports(&spec));

        let mut child = spawn(&spec).unwrap().unwrap();
        let mut stdout = child.take_stdout().unwrap();
        let mut output = String::new();
        stdout.read_to_string(&mut output).await.unwrap();
        assert!(child.wait().await.unwrap().success());
        assert_eq!(output.trim_end(), canonical.to_str().unwrap());
    }

    #[tokio::test]
    async fn bare_command_uses_the_child_environment_path() {
        let directory = tempfile::TempDir::new().unwrap();
        let canonical = directory.path().canonicalize().unwrap();
        write_executable(
            &canonical.join("child-path-probe"),
            b"#!/bin/sh\nprintf child-path\n",
        );
        let args = Vec::new();
        let env = BTreeMap::from([(OsString::from("PATH"), canonical.as_os_str().to_owned())]);
        let spec = SpawnSpec {
            command: OsStr::new("child-path-probe"),
            args: &args,
            env: &env,
            cwd: None,
            passed_fds: &[],
            reset_signals: &[],
        };

        let mut child = spawn(&spec).unwrap().unwrap();
        let mut stdout = child.take_stdout().unwrap();
        let mut output = String::new();
        stdout.read_to_string(&mut output).await.unwrap();
        assert!(child.wait().await.unwrap().success());
        assert_eq!(output, "child-path");
    }

    #[tokio::test]
    async fn relative_search_path_with_pinned_cwd_requests_fork_fallback() {
        let directory = tempfile::TempDir::new().unwrap();
        let canonical = directory.path().canonicalize().unwrap();
        let pinned = PinnedCwd::open_absolute(&canonical).unwrap();
        let args = Vec::new();
        let env = BTreeMap::from([(OsString::from("PATH"), OsString::from("bin"))]);
        let spec = SpawnSpec {
            command: OsStr::new("probe"),
            args: &args,
            env: &env,
            cwd: Some(&pinned),
            passed_fds: &[],
            reset_signals: &[],
        };
        assert!(spawn(&spec).unwrap().is_none());
    }

    #[tokio::test]
    async fn executable_text_without_shebang_requests_fork_fallback() {
        let directory = tempfile::TempDir::new().unwrap();
        let program = directory.path().join("plain-text");
        write_executable(&program, b"exit 0\n");
        let args = Vec::new();
        let env = environment();
        let spec = SpawnSpec {
            command: program.as_os_str(),
            args: &args,
            env: &env,
            cwd: None,
            passed_fds: &[],
            reset_signals: &[],
        };
        assert!(spawn(&spec).unwrap().is_none());
    }
}
