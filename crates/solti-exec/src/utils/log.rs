//! # Log: async-signal-safe logging for `pre_exec` hooks.
//!
//! Provides two helpers that can be called **between `fork()` and `execve()`** where only async-signal-safe functions are allowed.
//!
//! ## API
//!
//! | Function              | What it writes           | Heap? |
//! |-----------------------|--------------------------|-------|
//! | [`pre_exec_log`]      | raw `&[u8]` to stderr    | no    |
//! | [`pre_exec_log_errno`]| `errno=<N>\n` to stderr  | no    |
//!
//! ## How it works
//! ```text
//! pre_exec_log(b"solti-exec: capget failed: ")
//!     │
//!     ├──► Unix:
//!     │     └──► libc::write(STDERR_FILENO, ptr, len)
//!     │          one syscall, no heap, no locks
//!     │
//!     └──► non-Unix:
//!           └──► std::io::stderr().write_all(msg)
//!
//! pre_exec_log_errno(errno)
//!     │
//!     ├──► inline stack-only int→ASCII into a `[u8; 32]` buffer
//!     │     └──► handles 0, negative (via `unsigned_abs`), multi-digit
//!     │
//!     └──► write "errno=" + digits + "\n" via 3 × libc::write
//! ```
//!
//! ## Rules
//! - **Zero heap allocation**: all buffers are stack-local `[u8; N]`
//! - Return values from `libc::write` are intentionally ignored (best-effort)
//! - Non-Unix: falls back to `std::io::stderr` (safe, pre_exec doesn't exist there)

/// Write a raw `&[u8]` message to stderr (async-signal-safe on Unix).
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

/// Write a raw `&[u8]` message to stderr (non-Unix fallback).
#[cfg(not(unix))]
pub(crate) fn pre_exec_log(msg: &[u8]) {
    use std::io::Write;

    let _ = std::io::stderr().write_all(msg);
}

/// Write `errno=<N>\n` to stderr (async-signal-safe on Unix).
///
/// Uses a stack-local `[u8; 32]` buffer for int→ASCII conversion.
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

/// Write `errno=<N>\n` to stderr (non-Unix fallback).
#[cfg(not(unix))]
pub(crate) fn pre_exec_log_errno(errno: i32) {
    use std::io::Write;

    let mut stderr = std::io::stderr();
    let _ = write!(stderr, "errno={errno}\n");
}
