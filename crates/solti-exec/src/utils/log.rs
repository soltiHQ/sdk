//! # Pre-exec logging
//!
//! Unix `pre_exec` hooks can use only async-signal-safe operations.
//! These helpers write diagnostics directly to stderr.
//!
//! ## Flow
//!
//! ```text
//! pre_exec hook
//!      ├── message ──► libc::write(stderr)
//!      └── errno ────► stack conversion ──► libc::write(stderr)
//! ```
//!
//! Writes are best-effort.
//! Unix paths do not allocate.
//! Other platforms use standard stderr writes.

/// Writes raw bytes to stderr on Unix.
#[cfg(unix)]
pub(crate) fn pre_exec_log(msg: &[u8]) {
    // SAFETY:
    // `libc::write` to STDERR_FILENO is async-signal-safe.
    // `msg.as_ptr()` is valid for `msg.len()` bytes (from a valid `&[u8]` slice).
    unsafe {
        let _ = libc::write(
            libc::STDERR_FILENO,
            msg.as_ptr() as *const libc::c_void,
            msg.len(),
        );
    }
}

/// Writes raw bytes to stderr on other platforms.
#[cfg(not(unix))]
pub(crate) fn pre_exec_log(msg: &[u8]) {
    use std::io::Write;

    let _ = std::io::stderr().write_all(msg);
}

/// Writes `errno=<N>` and a newline to stderr on Unix.
///
/// Integer conversion uses a stack buffer.
#[cfg(unix)]
pub(crate) fn pre_exec_log_errno(errno: i32) {
    let mut buf = [0u8; 32];
    let mut idx = buf.len();
    let negative = errno < 0;
    let mut n = errno.unsigned_abs();

    if n == 0 {
        idx -= 1;
        buf[idx] = b'0';
    } else {
        while n > 0 {
            let digit = (n % 10) as u8;
            n /= 10;
            idx -= 1;
            buf[idx] = b'0' + digit;
        }
    }
    if negative {
        idx -= 1;
        buf[idx] = b'-';
    }

    const PREFIX: &[u8] = b"errno=";

    // SAFETY:
    // All pointers are derived from valid stack-local byte slices/arrays.
    // `libc::write` to STDERR_FILENO is async-signal-safe per POSIX.
    unsafe {
        let _ = libc::write(
            libc::STDERR_FILENO,
            PREFIX.as_ptr() as *const libc::c_void,
            PREFIX.len(),
        );
        let _ = libc::write(
            libc::STDERR_FILENO,
            buf[idx..].as_ptr() as *const libc::c_void,
            buf.len() - idx,
        );
        let nl = b"\n";
        let _ = libc::write(
            libc::STDERR_FILENO,
            nl.as_ptr() as *const libc::c_void,
            nl.len(),
        );
    }
}

/// Writes `errno=<N>` and a newline to stderr on other platforms.
#[cfg(not(unix))]
pub(crate) fn pre_exec_log_errno(errno: i32) {
    use std::io::Write;

    let mut stderr = std::io::stderr();
    let _ = write!(stderr, "errno={errno}\n");
}
