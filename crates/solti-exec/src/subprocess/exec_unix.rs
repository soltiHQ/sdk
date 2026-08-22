//! Fail-closed Unix executable dispatch.
//!
//! Rust's Unix `Command` fork path ends in `execvp`, which interprets an
//! executable text file through `/bin/sh` after `ENOEXEC`. Subprocess command
//! mode promises direct executable dispatch instead. This module prepares the
//! complete PATH search, argv, and environment in the parent, then installs a
//! final `pre_exec` hook which uses `execve` plus only the platform's required
//! async-signal-safe PATH continuation check.

use std::{
    collections::BTreeMap,
    ffi::{CString, OsStr, OsString, c_char},
    io,
    os::unix::{ffi::OsStrExt as _, process::CommandExt as _},
    path::Path,
};

use tokio::process::Command;

/// Parent-prepared direct-exec state for one spawn attempt.
pub(super) struct ExecvePlan {
    candidates: Vec<CString>,
    arguments: CStringArray,
    environment: CStringArray,
    searches_path: bool,
}

impl ExecvePlan {
    /// Materializes every allocation and fallible string conversion before fork.
    pub(super) fn prepare(
        command: &OsStr,
        arguments: &[OsString],
        environment: &BTreeMap<OsString, OsString>,
    ) -> io::Result<Self> {
        let searches_path = !command.as_bytes().contains(&b'/');
        Ok(Self {
            candidates: executable_candidates(command, environment)?,
            arguments: CStringArray::arguments(command, arguments)?,
            environment: CStringArray::environment(environment)?,
            searches_path,
        })
    }

    /// Installs the final child hook after cwd, descriptor, and host controls.
    pub(super) fn attach(self, command: &mut Command) {
        // SAFETY:
        // `self` owns all strings and pointer arrays used by the hook. They are
        // completely allocated in the parent. `execute` performs only execve,
        // errno reads, scalar comparisons, iteration over fixed storage, and
        // Darwin's async-signal-safe stat check for ambiguous search errors.
        unsafe {
            command.as_std_mut().pre_exec(move || self.execute());
        }
    }

    /// Searches PATH with direct `execve` calls and never invokes a shell.
    fn execute(&self) -> io::Result<()> {
        let mut permission_denied = false;
        #[cfg(target_os = "linux")]
        let mut last_search_error = None;
        for candidate in &self.candidates {
            // SAFETY:
            // all strings and null-terminated pointer arrays are retained by
            // `self`; execve reads them only for the duration of this call.
            unsafe {
                libc::execve(
                    candidate.as_ptr(),
                    self.arguments.as_ptr(),
                    self.environment.as_ptr(),
                );
            }
            let error = io::Error::last_os_error();
            if !self.searches_path {
                return Err(error);
            }
            if !continue_path_search(candidate, &error, &mut permission_denied) {
                // ENOEXEC deliberately returns here. Calling execvp would
                // reinterpret the file through /bin/sh.
                return Err(error);
            }
            #[cfg(target_os = "linux")]
            {
                // glibc preserves the last continued search errno unless an
                // EACCES candidate takes precedence at exhaustion.
                last_search_error = error.raw_os_error();
            }
        }

        if permission_denied {
            return Err(io::Error::from_raw_os_error(libc::EACCES));
        }
        #[cfg(target_os = "linux")]
        if let Some(code) = last_search_error {
            return Err(io::Error::from_raw_os_error(code));
        }
        Err(io::Error::from_raw_os_error(libc::ENOENT))
    }
}

/// C strings plus their stable null-terminated pointer array.
struct CStringArray {
    _storage: Vec<CString>,
    pointers: Vec<StableCStringPointer>,
}

impl CStringArray {
    fn arguments(command: &OsStr, arguments: &[OsString]) -> io::Result<Self> {
        let storage = std::iter::once(command)
            .chain(arguments.iter().map(OsString::as_os_str))
            .map(|value| cstring(value, "process argument contains NUL"))
            .collect::<io::Result<Vec<_>>>()?;
        Ok(Self::new(storage))
    }

    fn environment(environment: &BTreeMap<OsString, OsString>) -> io::Result<Self> {
        let storage = environment
            .iter()
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
            .collect::<io::Result<Vec<_>>>()?;
        Ok(Self::new(storage))
    }

    fn new(storage: Vec<CString>) -> Self {
        let mut pointers = storage
            .iter()
            .map(|value| StableCStringPointer(value.as_ptr()))
            .collect::<Vec<_>>();
        pointers.push(StableCStringPointer(std::ptr::null()));
        Self {
            _storage: storage,
            pointers,
        }
    }

    fn as_ptr(&self) -> *const *const c_char {
        self.pointers.as_ptr().cast()
    }
}

/// Sendable pointer retained with its immutable CString backing storage.
#[repr(transparent)]
#[derive(Clone, Copy)]
struct StableCStringPointer(*const c_char);

// SAFETY:
// the pointers are never mutated or dereferenced in Rust. Their CString
// storage lives in the same `CStringArray`, and the child only passes the
// array to execve before either value can be dropped.
unsafe impl Send for StableCStringPointer {}
// SAFETY: the immutable pointer array has the same ownership invariant.
unsafe impl Sync for StableCStringPointer {}

fn executable_candidates(
    command: &OsStr,
    environment: &BTreeMap<OsString, OsString>,
) -> io::Result<Vec<CString>> {
    if command.as_bytes().is_empty() {
        return Err(io::Error::from_raw_os_error(libc::ENOENT));
    }
    if command.as_bytes().contains(&b'/') {
        return Ok(vec![cstring(command, "command contains NUL")?]);
    }

    let path = match environment.get(OsStr::new("PATH")) {
        Some(path) => path.clone(),
        None => default_search_path()?,
    };
    let command_bytes = command.as_bytes();
    let mut candidates = Vec::new();
    for component in path.as_os_str().as_bytes().split(|byte| *byte == b':') {
        if skip_overlong_path_component(component, command_bytes) {
            continue;
        }
        let component = OsStr::from_bytes(component);
        let candidate = if component.is_empty() {
            command.to_owned()
        } else {
            Path::new(component).join(command).into_os_string()
        };
        candidates.push(cstring(&candidate, "executable search path contains NUL")?);
    }
    Ok(candidates)
}

/// Applies the platform libc PATH-search continuation policy without a shell.
#[cfg(target_os = "macos")]
fn continue_path_search(
    candidate: &CString,
    error: &io::Error,
    permission_denied: &mut bool,
) -> bool {
    let Some(code) = error.raw_os_error() else {
        return false;
    };
    match code {
        libc::ELOOP | libc::ENAMETOOLONG | libc::ENOENT | libc::ENOTDIR => true,
        libc::E2BIG | libc::ENOEXEC | libc::ENOMEM | libc::ETXTBSY => false,
        _ => {
            let mut metadata = std::mem::MaybeUninit::<libc::stat>::uninit();
            // SAFETY: `candidate` is NUL-terminated and `metadata` points to
            // writable stack storage. `stat` is async-signal-safe on Darwin.
            if unsafe { libc::stat(candidate.as_ptr(), metadata.as_mut_ptr()) } != 0 {
                return true;
            }
            if code == libc::EACCES {
                *permission_denied = true;
                return true;
            }
            false
        }
    }
}

#[cfg(target_os = "linux")]
fn continue_path_search(
    _candidate: &CString,
    error: &io::Error,
    permission_denied: &mut bool,
) -> bool {
    match error.raw_os_error() {
        Some(libc::EACCES) => {
            *permission_denied = true;
            true
        }
        Some(libc::ENOENT | libc::ENOTDIR | libc::ESTALE | libc::ENODEV | libc::ETIMEDOUT) => true,
        _ => false,
    }
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn continue_path_search(
    _candidate: &CString,
    error: &io::Error,
    permission_denied: &mut bool,
) -> bool {
    match error.raw_os_error() {
        Some(libc::EACCES) => {
            *permission_denied = true;
            true
        }
        Some(libc::ENOENT | libc::ENOTDIR) => true,
        _ => false,
    }
}

#[cfg(target_os = "macos")]
fn skip_overlong_path_component(component: &[u8], command: &[u8]) -> bool {
    // Darwin's execvPe builds one candidate in a MAXPATHLEN buffer and skips
    // components which cannot fit. Empty components are represented as `.`.
    let component_len = if component.is_empty() {
        1
    } else {
        component.len()
    };
    component_len
        .checked_add(command.len())
        .and_then(|length| length.checked_add(2))
        .is_none_or(|length| length > libc::PATH_MAX as usize)
}

#[cfg(target_os = "linux")]
fn skip_overlong_path_component(component: &[u8], _command: &[u8]) -> bool {
    // glibc's execvpe skips an overlong PATH element before constructing its
    // stack candidate, while a too-long executable name remains an error.
    component.len() >= libc::PATH_MAX as usize
}

#[cfg(not(any(target_os = "linux", target_os = "macos")))]
fn skip_overlong_path_component(_component: &[u8], _command: &[u8]) -> bool {
    false
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

fn cstring(value: &OsStr, message: &'static str) -> io::Result<CString> {
    CString::new(value.as_bytes()).map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, message))
}
