//! Opt-in integration coverage for Linux host-process enforcement.
//!
//! The public test runs the production `solti-exec` library against a real
//! writable cgroup v2 delegation. It is ignored by default and requires both
//! `SOLTI_TEST_LINUX_HOST=1` and `SOLTI_TEST_CGROUP_PARENT`.

#![cfg(target_os = "linux")]

use std::{
    fs,
    io::{self, Write as _},
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::Duration,
};

use solti_exec::{
    host::{CgroupLimits, HostProcessPolicy, SeccompPolicy, SecurityConfig},
    subprocess::{SubprocessBackendConfig, register_subprocess_runner_with_backend},
};
use solti_model::{
    Flag, OutputEvent, SubprocessMode, SubprocessSpec, Task, TaskEnv, TaskId, TaskSpec,
    TaskWorkload,
};
use solti_runner::{OutputPublisher, OutputPublisherHandle, OutputSink, RunnerRouter};
use taskvisor::TaskContext;

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

const CHILD_MODE: &str = "solti-linux-host-runtime-v1";
const PIDS_MAX: u64 = 8;
const ATTEMPT_TIMEOUT: Duration = Duration::from_secs(30);
const SHUTDOWN_TIMEOUT: Duration = Duration::from_secs(15);

#[derive(Default)]
struct RecordingOutput {
    events: Arc<Mutex<Vec<OutputEvent>>>,
}

impl OutputPublisher for RecordingOutput {
    fn sink_for(&self, _task_name: &TaskId, generation: u64, attempt: u32) -> Option<OutputSink> {
        let events = Arc::clone(&self.events);
        Some(OutputSink::new(generation, attempt, move |event| {
            events
                .lock()
                .expect("Linux host integration output lock")
                .push(event);
        }))
    }
}

/// Runs inside the subprocess created by `public_linux_host_policy_is_enforced`.
///
/// Keeping this helper in the external test binary exercises the production
/// library build. The helper is ignored and rejects direct execution unless
/// the parent supplies the private child mode used by the public test.
#[test]
#[ignore = "invoked only as the child of the public Linux host runtime test"]
fn linux_host_runtime_child_helper() -> TestResult {
    require_exact_child_mode()?;

    let parent = required_path("SOLTI_TEST_CGROUP_PARENT")?;
    let cgroup = find_own_attempt_cgroup(&parent)?;
    assert_security_status()?;
    assert_denied_process_vm_readv()?;
    assert_cgroup_controls(&cgroup)?;
    assert_pids_limit_is_enforced(&cgroup)?;

    let escaped_pid = spawn_escaped_descendant()?;
    let mut stdout = io::stdout().lock();
    writeln!(stdout, "SOLTI_LINUX_HOST_SECURITY_OK=1")?;
    writeln!(stdout, "SOLTI_LINUX_HOST_PIDS_OK=1")?;
    writeln!(stdout, "SOLTI_LINUX_HOST_CGROUP={}", cgroup.display())?;
    writeln!(stdout, "SOLTI_LINUX_HOST_ESCAPED_PID={escaped_pid}")?;
    stdout.flush()?;
    escaped_pid.disarm();
    Ok(())
}

#[tokio::test(flavor = "current_thread")]
#[ignore = "requires an explicitly delegated writable Linux cgroup v2 parent"]
async fn public_linux_host_policy_is_enforced() -> TestResult {
    if std::env::var("SOLTI_TEST_LINUX_HOST").as_deref() != Ok("1") {
        return Err("set SOLTI_TEST_LINUX_HOST=1 to run the live Linux host test".into());
    }

    let cgroup_parent = required_path("SOLTI_TEST_CGROUP_PARENT")?.canonicalize()?;
    preflight_cgroup_parent(&cgroup_parent)?;
    let child_executable = std::env::current_exe()?.canonicalize()?;
    let child_command = child_executable
        .to_str()
        .ok_or("Linux host integration test executable path must be valid UTF-8")?
        .to_owned();

    let policy = HostProcessPolicy::new()
        .with_cgroups(CgroupLimits {
            pids: Some(PIDS_MAX),
            ..Default::default()
        })
        .with_cgroup_parent(cgroup_parent.clone())
        .with_security(SecurityConfig {
            seccomp: SeccompPolicy::DenyHostControl,
            ..Default::default()
        });
    let backend = SubprocessBackendConfig::new().with_host_process_policy(policy);

    let output = Arc::new(RecordingOutput::default());
    let output_handle: OutputPublisherHandle = output.clone();
    let mut router = RunnerRouter::new().with_output_publisher(output_handle);
    let runner =
        register_subprocess_runner_with_backend(&mut router, "linux-host-runtime", backend)?;

    let attempt = async {
        let mut env = TaskEnv::new();
        env.push("SOLTI_TEST_LINUX_HOST", "1");
        env.push("SOLTI_TEST_LINUX_HOST_CHILD_MODE", CHILD_MODE);
        env.push(
            "SOLTI_TEST_CGROUP_PARENT",
            cgroup_parent
                .to_str()
                .ok_or("SOLTI_TEST_CGROUP_PARENT must be valid UTF-8")?,
        );
        let workload = TaskWorkload::Subprocess(SubprocessSpec::new(
            SubprocessMode::Command {
                command: child_command.clone(),
                args: vec![
                    "--ignored".into(),
                    "--exact".into(),
                    "linux_host_runtime_child_helper".into(),
                    "--nocapture".into(),
                    "--test-threads=1".into(),
                ],
            },
            env,
            None,
            Flag::enabled(),
        ));
        let spec = TaskSpec::builder("linux-host-runtime", workload, 30_000_u64).build()?;
        let task = Task::new("linux-host-runtime", spec)?;
        let built = router.build(&task).await?;
        tokio::time::timeout(ATTEMPT_TIMEOUT, built.task().spawn(TaskContext::detached()))
            .await
            .map_err(|_| "Linux host attempt exceeded its bounded test timeout")??;
        drop(built);

        let lines = output_lines(&output);
        require_marker(&lines, "SOLTI_LINUX_HOST_SECURITY_OK=1")?;
        require_marker(&lines, "SOLTI_LINUX_HOST_PIDS_OK=1")?;
        let cgroup = marker_path(&lines, "SOLTI_LINUX_HOST_CGROUP=")?;
        if cgroup.parent() != Some(cgroup_parent.as_path()) {
            return Err(format!(
                "reported attempt cgroup {} is not a direct child of {}",
                cgroup.display(),
                cgroup_parent.display()
            )
            .into());
        }
        if cgroup.exists() {
            return Err(format!(
                "attempt cgroup still exists after successful completion: {}",
                cgroup.display()
            )
            .into());
        }

        let escaped_pid = marker_pid(&lines, "SOLTI_LINUX_HOST_ESCAPED_PID=")?;
        wait_for_process_exit(escaped_pid).await?;

        let status = runner.finalizer_status();
        if !status.accepting()
            || !status.healthy()
            || status.owned() != 0
            || status.quarantined() != 0
        {
            return Err(
                format!("unexpected live finalizer status before shutdown: {status:?}").into(),
            );
        }
        Ok::<(), Box<dyn std::error::Error>>(())
    }
    .await;

    drop(router);
    let shutdown = runner.shutdown(SHUTDOWN_TIMEOUT).await;
    let terminal_status = runner.finalizer_status();
    let terminal = if terminal_status.accepting()
        || !terminal_status.healthy()
        || terminal_status.owned() != 0
        || terminal_status.quarantined() != 0
    {
        Err(format!(
            "unexpected terminal finalizer status: {terminal_status:?}"
        ))
    } else {
        Ok(())
    };

    match (attempt, shutdown, terminal) {
        (Ok(()), Ok(()), Ok(())) => Ok(()),
        (attempt, shutdown, terminal) => Err(format!(
            "Linux host integration failed; attempt={attempt:?}; shutdown={shutdown:?}; terminal={terminal:?}"
        )
        .into()),
    }
}

fn require_exact_child_mode() -> TestResult {
    if std::env::var("SOLTI_TEST_LINUX_HOST").as_deref() != Ok("1")
        || std::env::var("SOLTI_TEST_LINUX_HOST_CHILD_MODE").as_deref() != Ok(CHILD_MODE)
    {
        return Err(
            "the ignored child helper may run only through the public Linux host runtime test"
                .into(),
        );
    }
    Ok(())
}

fn required_path(name: &str) -> TestResult<PathBuf> {
    let value = std::env::var_os(name).ok_or_else(|| format!("set {name} to an absolute path"))?;
    let path = PathBuf::from(value);
    if !path.is_absolute() {
        return Err(format!("{name} must be absolute: {}", path.display()).into());
    }
    Ok(path)
}

fn preflight_cgroup_parent(parent: &Path) -> TestResult {
    let controllers = fs::read_to_string(parent.join("cgroup.controllers"))?;
    if !controllers.split_whitespace().any(|value| value == "pids") {
        return Err(format!(
            "the pids controller is not delegated at {}",
            parent.display()
        )
        .into());
    }
    let subtree = fs::read_to_string(parent.join("cgroup.subtree_control"))?;
    if !subtree.split_whitespace().any(|value| value == "pids") {
        return Err(format!(
            "enable +pids in {}/cgroup.subtree_control before running this test",
            parent.display()
        )
        .into());
    }
    if !parent.join("cgroup.kill").exists() {
        return Err(format!(
            "{} has no cgroup.kill; escaped-session termination cannot be certified",
            parent.display()
        )
        .into());
    }
    Ok(())
}

fn assert_security_status() -> TestResult {
    let status = fs::read_to_string("/proc/self/status")?;
    let no_new_privs = proc_status_value(&status, "NoNewPrivs")
        .ok_or("/proc/self/status has no NoNewPrivs field")?;
    let seccomp =
        proc_status_value(&status, "Seccomp").ok_or("/proc/self/status has no Seccomp field")?;
    if no_new_privs != "1" || seccomp != "2" {
        return Err(format!(
            "expected NoNewPrivs=1 and Seccomp=2, observed NoNewPrivs={no_new_privs} Seccomp={seccomp}"
        )
        .into());
    }
    Ok(())
}

fn proc_status_value<'a>(status: &'a str, name: &str) -> Option<&'a str> {
    status.lines().find_map(|line| {
        let (field, value) = line.split_once(':')?;
        (field == name).then(|| value.trim())
    })
}

fn assert_denied_process_vm_readv() -> TestResult {
    // SAFETY: the syscall receives the current PID and zero-length null iovec
    // arrays. Without the filter this is a valid no-op; the policy must reject
    // it before argument processing.
    let result = unsafe {
        libc::syscall(
            libc::SYS_process_vm_readv,
            libc::getpid(),
            std::ptr::null::<libc::iovec>(),
            0_usize,
            std::ptr::null::<libc::iovec>(),
            0_usize,
            0_usize,
        )
    };
    let error = io::Error::last_os_error();
    if result != -1 || error.raw_os_error() != Some(libc::EPERM) {
        return Err(format!(
            "process_vm_readv was not rejected with EPERM: result={result}, error={error}"
        )
        .into());
    }
    Ok(())
}

fn find_own_attempt_cgroup(parent: &Path) -> TestResult<PathBuf> {
    let pid = std::process::id().to_string();
    let mut matches = Vec::new();
    for entry in fs::read_dir(parent)? {
        let entry = entry?;
        if !entry.file_type()?.is_dir() {
            continue;
        }
        let path = entry.path();
        let procs = match fs::read_to_string(path.join("cgroup.procs")) {
            Ok(value) => value,
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error.into()),
        };
        if procs.lines().any(|value| value.trim() == pid) {
            matches.push(path);
        }
    }
    match matches.as_slice() {
        [path] => Ok(path.clone()),
        [] => Err(format!(
            "PID {pid} is not a member of a direct child cgroup under {}",
            parent.display()
        )
        .into()),
        _ => Err(format!(
            "PID {pid} appears in multiple direct child cgroups under {}",
            parent.display()
        )
        .into()),
    }
}

fn assert_cgroup_controls(cgroup: &Path) -> TestResult {
    let pids_max = fs::read_to_string(cgroup.join("pids.max"))?;
    if pids_max.trim() != PIDS_MAX.to_string() {
        return Err(format!(
            "expected pids.max={PIDS_MAX}, observed {:?}",
            pids_max.trim()
        )
        .into());
    }
    let max_depth = fs::read_to_string(cgroup.join("cgroup.max.depth"))?;
    if max_depth.trim() != "0" {
        return Err(format!(
            "expected cgroup.max.depth=0, observed {:?}",
            max_depth.trim()
        )
        .into());
    }
    let pid = std::process::id().to_string();
    let procs = fs::read_to_string(cgroup.join("cgroup.procs"))?;
    if !procs.lines().any(|value| value.trim() == pid) {
        return Err(format!("PID {pid} is absent from {}/cgroup.procs", cgroup.display()).into());
    }
    Ok(())
}

fn assert_pids_limit_is_enforced(cgroup: &Path) -> TestResult {
    let before = read_named_counter(&cgroup.join("pids.events"), "max")?;
    let mut children = ChildSet::default();
    let mut denied = None;
    for _ in 0..=PIDS_MAX {
        // SAFETY: the forked child calls only async-signal-safe libc functions
        // before `_exit`; the parent retains every child PID for cleanup.
        let pid = unsafe { libc::fork() };
        if pid == 0 {
            loop {
                // SAFETY: pause has no preconditions and is async-signal-safe.
                unsafe { libc::pause() };
            }
        }
        if pid > 0 {
            children.pids.push(pid);
            continue;
        }

        let error = io::Error::last_os_error();
        denied = Some(error.raw_os_error());
        break;
    }
    if denied != Some(Some(libc::EAGAIN)) {
        return Err(
            format!("fork did not reach the cgroup pids limit with EAGAIN: {denied:?}").into(),
        );
    }
    let after = read_named_counter(&cgroup.join("pids.events"), "max")?;
    if after <= before {
        return Err(format!(
            "pids.events max did not increase after enforced denial: before={before}, after={after}"
        )
        .into());
    }
    children.terminate_and_reap_all();
    Ok(())
}

fn read_named_counter(path: &Path, name: &str) -> TestResult<u64> {
    let contents = fs::read_to_string(path)?;
    let value = contents.lines().find_map(|line| {
        let mut fields = line.split_whitespace();
        (fields.next()? == name).then(|| fields.next()).flatten()
    });
    value
        .ok_or_else(|| format!("{} has no {name} counter", path.display()))?
        .parse::<u64>()
        .map_err(Into::into)
}

#[derive(Default)]
struct ChildSet {
    pids: Vec<libc::pid_t>,
}

impl ChildSet {
    fn terminate_and_reap_all(&mut self) {
        for &pid in &self.pids {
            // SAFETY: each PID was returned by fork in this process.
            let _ = unsafe { libc::kill(pid, libc::SIGKILL) };
        }
        for pid in self.pids.drain(..) {
            reap_child(pid);
        }
    }
}

impl Drop for ChildSet {
    fn drop(&mut self) {
        self.terminate_and_reap_all();
    }
}

struct EscapedChild {
    pid: Option<libc::pid_t>,
}

impl EscapedChild {
    fn pid(&self) -> libc::pid_t {
        self.pid.expect("escaped child is armed")
    }

    fn disarm(mut self) {
        self.pid.take();
    }
}

impl std::fmt::Display for EscapedChild {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.pid().fmt(formatter)
    }
}

impl Drop for EscapedChild {
    fn drop(&mut self) {
        if let Some(pid) = self.pid.take() {
            // SAFETY: this process created the child and still owns waitpid.
            let _ = unsafe { libc::kill(pid, libc::SIGKILL) };
            reap_child(pid);
        }
    }
}

fn spawn_escaped_descendant() -> TestResult<EscapedChild> {
    let mut pipe = [0; 2];
    // SAFETY: `pipe` has storage for two descriptors.
    if unsafe { libc::pipe2(pipe.as_mut_ptr(), libc::O_CLOEXEC) } != 0 {
        return Err(io::Error::last_os_error().into());
    }

    // SAFETY: the forked child calls only async-signal-safe libc functions.
    let pid = unsafe { libc::fork() };
    if pid < 0 {
        let error = io::Error::last_os_error();
        // SAFETY: both descriptors were initialized by pipe2.
        unsafe {
            libc::close(pipe[0]);
            libc::close(pipe[1]);
        }
        return Err(error.into());
    }
    if pid == 0 {
        // SAFETY: all calls below are async-signal-safe. The child either
        // pauses in its new session or exits without entering Rust code.
        unsafe {
            libc::close(pipe[0]);
            let established = libc::setsid() >= 0;
            let byte = [u8::from(established)];
            let _ = libc::write(pipe[1], byte.as_ptr().cast(), byte.len());
            libc::close(pipe[1]);
            if !established {
                libc::_exit(126);
            }
            libc::close(libc::STDIN_FILENO);
            libc::close(libc::STDOUT_FILENO);
            libc::close(libc::STDERR_FILENO);
            loop {
                libc::pause();
            }
        }
    }

    // SAFETY: the parent no longer needs the write side.
    unsafe { libc::close(pipe[1]) };
    let child = EscapedChild { pid: Some(pid) };
    let mut established = [0_u8; 1];
    let read = loop {
        // SAFETY: the descriptor is readable and the one-byte buffer is valid.
        let read = unsafe { libc::read(pipe[0], established.as_mut_ptr().cast(), 1) };
        if read >= 0 || io::Error::last_os_error().raw_os_error() != Some(libc::EINTR) {
            break read;
        }
    };
    // SAFETY: the read side is no longer used.
    unsafe { libc::close(pipe[0]) };
    if read != 1 || established[0] != 1 {
        return Err(format!("descendant {pid} failed to establish a new session").into());
    }
    Ok(child)
}

fn reap_child(pid: libc::pid_t) {
    loop {
        // SAFETY: PID identifies a child created by this process.
        let waited = unsafe { libc::waitpid(pid, std::ptr::null_mut(), 0) };
        if waited == pid {
            return;
        }
        if waited < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::EINTR) {
            continue;
        }
        return;
    }
}

fn output_lines(output: &RecordingOutput) -> Vec<String> {
    output
        .events
        .lock()
        .expect("Linux host integration output lock")
        .iter()
        .filter_map(|event| match event {
            OutputEvent::Chunk(chunk) => Some(String::from_utf8_lossy(&chunk.line).into_owned()),
            _ => None,
        })
        .collect()
}

fn require_marker(lines: &[String], marker: &str) -> TestResult {
    // libtest writes `test <name> ... ` before invoking an ignored test. When
    // this binary runs itself as the workload, the first helper marker shares
    // that line with the harness prefix.
    if lines.iter().any(|line| line.ends_with(marker)) {
        Ok(())
    } else {
        Err(format!("subprocess output did not contain {marker:?}: {lines:?}").into())
    }
}

fn marker_value<'a>(lines: &'a [String], prefix: &str) -> TestResult<&'a str> {
    lines
        .iter()
        .find_map(|line| line.strip_prefix(prefix))
        .ok_or_else(|| format!("subprocess output has no {prefix:?} marker: {lines:?}").into())
}

fn marker_path(lines: &[String], prefix: &str) -> TestResult<PathBuf> {
    Ok(PathBuf::from(marker_value(lines, prefix)?))
}

fn marker_pid(lines: &[String], prefix: &str) -> TestResult<libc::pid_t> {
    Ok(marker_value(lines, prefix)?.parse()?)
}

async fn wait_for_process_exit(pid: libc::pid_t) -> TestResult {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            // SAFETY: signal zero performs an existence check without sending a signal.
            let result = unsafe { libc::kill(pid, 0) };
            if result < 0 && io::Error::last_os_error().raw_os_error() == Some(libc::ESRCH) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .map_err(|_| format!("escaped setsid descendant {pid} is still observable after cleanup"))?;
    Ok(())
}
