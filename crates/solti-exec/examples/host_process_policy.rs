//! # Host process policy
//!
//! `HostProcessPolicy` is the low-level boundary for process-based backends.
//! It prepares controls before process creation and returns attempt-owned cleanup state.
//!
//! This example shows:
//!
//! - inherited process-state controls;
//! - POSIX limit preparation and clamping;
//! - attaching prepared controls to one command;
//! - waiting before attempt cleanup;
//! - the empty cgroup boundary when no cgroup limits are requested.
//!
//! The example uses Unix process controls.
//! Linux cgroups, namespaces, credentials, capabilities, and seccomp require separate platform policy.
//!
//! Run with `cargo run -p solti-exec --example host_process_policy --features host-process`.

type ExampleResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

const FLOW: &str = r#"
solti-exec: low-level host process boundary

  HostProcessPolicy
      ├── ProcessConfig ──► reset signals + new session + umask
      ├── RlimitConfig ───► clamp against inherited hard limits
      └── no cgroups
               ▼
  PreparedHostProcessPolicy
               │ prepare_attempt(None)
               ▼
  PreparedHostProcessAttempt ──► apply_to_command()
                                          ├──► child process
                                          └──► AttemptProcessDomain
                                          wait/reap │
                                                    ▼
                                                 cleanup

  Preparation validates policy before process creation.
  The backend must keep the domain until its process scope has stopped.
"#;

fn main() -> ExampleResult {
    println!("{FLOW}");
    println!(
        "[purpose] Apply reusable low-level controls without depending on the Subprocess runner."
    );
    run_platform()
}

#[cfg(unix)]
fn run_platform() -> ExampleResult {
    use std::process::{Command, Stdio};

    use solti_exec::host::{HostProcessPolicy, ProcessConfig, RlimitConfig};

    let policy = HostProcessPolicy::new()
        .with_process_config(ProcessConfig {
            reset_signals: true,
            new_session: true,
            umask: Some(0o077),
        })
        .with_rlimits(RlimitConfig {
            max_open_files: Some(64),
            max_file_size_bytes: None,
            disable_core_dumps: true,
        });
    println!("[setup] Requested a new session, umask 077, NOFILE=64, and disabled core dumps.");

    let prepared = policy.prepare()?;
    let rlimits = prepared
        .rlimits()
        .ok_or("prepared policy must contain rlimits")?;
    println!(
        "[prepare] Resolved NOFILE={:?}, FSIZE={:?}, CORE={:?}.",
        rlimits.max_open_files(),
        rlimits.max_file_size_bytes(),
        rlimits.core_dump_size_bytes(),
    );
    let resolved_nofile = rlimits
        .max_open_files()
        .ok_or("prepared policy must contain NOFILE")?;
    assert!(resolved_nofile <= 64);
    assert_eq!(rlimits.core_dump_size_bytes(), Some(0));

    let attempt = prepared.prepare_attempt(None)?;
    let mut command = Command::new("/bin/sh");
    command
        .args(["-c", "printf 'umask=%s\\n' \"$(umask)\"; printf 'nofile=%s\\n' \"$(ulimit -n)\"; printf 'core=%s\\n' \"$(ulimit -c)\""])
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit());
    let mut domain = attempt.apply_to_command(&mut command);
    assert!(domain.cgroup_path().is_none());
    println!("[attempt] Attached controls; no cgroup was created.");

    let output = command.spawn()?.wait_with_output()?;
    if !output.status.success() {
        return Err(format!("child exited with {}", output.status).into());
    }
    let observed = String::from_utf8(output.stdout)?;
    println!("[child] Observed controls:\n{}", observed.trim_end());
    let observed_nofile = observed
        .lines()
        .find_map(|line| line.strip_prefix("nofile="))
        .ok_or("child did not report NOFILE")?
        .parse::<u64>()?;
    assert_eq!(observed_nofile, resolved_nofile);
    assert!(observed.contains("core=0"));
    assert!(observed.contains("umask=0077") || observed.contains("umask=077"));

    domain.cleanup()?;
    println!("[cleanup] The child was reaped before the attempt domain was cleaned.");
    println!(
        "\nResult: the custom backend validated once, attached prepared controls to one child, retained ownership through wait, and cleaned the attempt domain."
    );
    Ok(())
}

#[cfg(not(unix))]
fn run_platform() -> ExampleResult {
    println!("[platform] This runnable policy uses Unix process controls.");
    println!("\nResult: no process was started on this platform.");
    Ok(())
}
