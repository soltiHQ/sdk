//! Low-level logging helpers for unsafe contexts (e.g. `pre_exec` hooks).
//!
//! These functions are designed to be safe to call between `fork()` and `execve` on Unix platforms.
//! On non-Unix platforms they fall back to simple `eprintln!`.

/// Write a raw byte message to stderr.
#[cfg(unix)]
pub fn pre_exec_log(msg: &[u8]) {
    unsafe {
        libc::write(
            libc::STDERR_FILENO,
            msg.as_ptr() as *const libc::c_void,
            msg.len(),
        );
    }
}

/// Write a raw byte message to stderr (non-Unix fallback).
#[cfg(not(unix))]
pub fn pre_exec_log(msg: &[u8]) {
    use std::io::Write;

    let _ = std::io::stderr().write_all(msg);
}

/// Log an errno value as `errno=<n>\n` to stderr.
///
/// On Unix this uses only stack buffers + `libc::write`.
#[cfg(unix)]
pub fn pre_exec_log_errno(errno: i32) {
    let mut buf = [0u8; 32];
    let mut idx = buf.len();
    let negative = errno < 0;
    let mut n = if negative {
        (-errno) as u32
    } else {
        errno as u32
    };

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
    unsafe {
        libc::write(
            libc::STDERR_FILENO,
            PREFIX.as_ptr() as *const libc::c_void,
            PREFIX.len(),
        );
        libc::write(
            libc::STDERR_FILENO,
            buf[idx..].as_ptr() as *const libc::c_void,
            buf.len() - idx,
        );
        let nl = b"\n";
        libc::write(
            libc::STDERR_FILENO,
            nl.as_ptr() as *const libc::c_void,
            nl.len(),
        );
    }
}

/// Log an errno value as `errno=<n>\n` (non-Unix fallback).
#[cfg(not(unix))]
pub fn pre_exec_log_errno(errno: i32) {
    use std::io::Write;

    let mut stderr = std::io::stderr();
    let _ = write!(stderr, "errno={errno}\n");
}
