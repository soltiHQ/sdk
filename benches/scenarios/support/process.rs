//! Controlled real-child workload and public subprocess fixtures.
//!
//! Every binary using this module dispatches child mode before Criterion.
//! No shell is needed for Command, readiness, output, or process-tree fixtures.

#![allow(dead_code)]

use std::{
    io::Write as _,
    path::{Path, PathBuf},
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use solti_benches::fixtures::WAIT_BOUND;
use solti_exec::subprocess::SubprocessRunner;
use solti_model::{
    Flag, OutputEvent, StreamKind, SubprocessMode, SubprocessSpec, Task, TaskEnv, TaskManifest,
    TaskSpec, TaskWorkload,
};
use solti_runner::{OutputPublisher, OutputSink};

pub const CHILD_SWITCH: &str = "--solti-bench-child";
pub const DONE: &[u8] = b"solti-bench-done";

/// Runs only when the benchmark itself starts this executable as a workload.
pub fn maybe_child() -> bool {
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    if args.first().map(String::as_str) != Some(CHILD_SWITCH) {
        return false;
    }
    child(&args[1..]);
    true
}

fn child(args: &[String]) {
    match args.first().map(String::as_str).expect("child mode") {
        "quiet" => println!("solti-bench-done"),
        "wait" => {
            std::fs::write(&args[1], std::process::id().to_string()).expect("ready marker");
            wait_file(Path::new(&args[2]));
        }
        "tree" => {
            let descendant_marker = format!("{}.descendant", args[1]);
            let mut descendant = std::process::Command::new(std::env::current_exe().unwrap())
                .args([CHILD_SWITCH, "wait", &descendant_marker, &args[2]])
                .spawn()
                .expect("spawn controlled descendant");
            wait_file(Path::new(&descendant_marker));
            std::fs::write(
                &args[1],
                format!("{} {}", std::process::id(), descendant.id()),
            )
            .expect("tree ready marker");
            wait_file(Path::new(&args[2]));
            descendant.wait().expect("reap controlled descendant");
        }
        "output" => {
            let lines: usize = args[3].parse().expect("line count");
            let width: usize = args[4].parse().expect("line width");
            std::fs::write(&args[1], std::process::id().to_string()).expect("ready marker");
            wait_file(Path::new(&args[2]));
            let line = vec![b'x'; width];
            let mut stdout = std::io::stdout().lock();
            let mut stderr = std::io::stderr().lock();
            for index in 0..lines {
                let out: &mut dyn std::io::Write = if index % 2 == 0 {
                    &mut stdout
                } else {
                    &mut stderr
                };
                out.write_all(&line).expect("write output body");
                out.write_all(b"\n").expect("write output delimiter");
            }
            stdout.flush().expect("flush stdout");
            stderr.flush().expect("flush stderr");
        }
        #[cfg(target_os = "linux")]
        "policy" => {
            if args[2] == "seccomp" {
                let status = std::fs::read_to_string("/proc/self/status").expect("child status");
                assert!(status.lines().any(|line| line == "NoNewPrivs:\t1"));
                assert!(status.lines().any(|line| line == "Seccomp:\t2"));
            }
            let pid = std::process::id().to_string();
            let cgroup = std::fs::read_dir(&args[1])
                .expect("delegated parent directory")
                .filter_map(Result::ok)
                .map(|entry| entry.path())
                .find(|path| {
                    std::fs::read_to_string(path.join("cgroup.procs"))
                        .is_ok_and(|members| members.lines().any(|member| member == pid))
                })
                .expect("child must belong to an attempt cgroup under the delegated parent");
            assert_eq!(
                std::fs::read_to_string(cgroup.join("pids.max"))
                    .unwrap()
                    .trim(),
                "8"
            );
            println!("solti-bench-cgroup={}", cgroup.display());
            println!("solti-bench-policy-ok");
        }
        other => panic!("unknown controlled child mode {other}"),
    }
}

fn wait_file(path: &Path) {
    let deadline = Instant::now() + WAIT_BOUND;
    while !path.is_file() {
        assert!(
            Instant::now() < deadline,
            "child gate exceeded {WAIT_BOUND:?}"
        );
        std::thread::sleep(Duration::from_millis(1));
    }
}

pub fn command_workload(args: Vec<String>) -> TaskWorkload {
    TaskWorkload::Subprocess(SubprocessSpec::new(
        SubprocessMode::Command {
            command: std::env::current_exe()
                .expect("benchmark executable")
                .to_str()
                .expect("UTF-8 benchmark executable")
                .to_owned(),
            args: std::iter::once(CHILD_SWITCH.to_owned())
                .chain(args)
                .collect(),
        },
        TaskEnv::new(),
        None,
        Flag::enabled(),
    ))
}

pub fn task(name: &str, workload: TaskWorkload) -> Task {
    Task::new(name, spec(name, workload)).expect("valid subprocess task")
}

pub fn manifest(name: &str, workload: TaskWorkload) -> TaskManifest {
    TaskManifest::new(name, spec(name, workload)).expect("valid subprocess manifest")
}

fn spec(name: &str, workload: TaskWorkload) -> TaskSpec {
    TaskSpec::builder(name, workload, 20_000_u64)
        .build()
        .expect("valid subprocess task spec")
}

pub struct Gate {
    _directory: tempfile::TempDir,
    pub ready: PathBuf,
    pub release: PathBuf,
}

impl Gate {
    pub fn new() -> Self {
        let directory = tempfile::tempdir().expect("workload gate directory");
        Self {
            ready: directory.path().join("ready"),
            release: directory.path().join("release"),
            _directory: directory,
        }
    }

    pub fn args(&self, mode: &str) -> Vec<String> {
        vec![
            mode.to_owned(),
            self.ready.to_str().unwrap().to_owned(),
            self.release.to_str().unwrap().to_owned(),
        ]
    }

    pub async fn wait_ready(&self) -> Vec<u32> {
        tokio::time::timeout(WAIT_BOUND, async {
            loop {
                if let Ok(value) = std::fs::read_to_string(&self.ready)
                    && !value.trim().is_empty()
                {
                    return value
                        .split_whitespace()
                        .map(|pid| pid.parse().expect("child pid"))
                        .collect();
                }
                tokio::time::sleep(Duration::from_millis(1)).await;
            }
        })
        .await
        .expect("controlled child did not become ready")
    }

    pub fn open(&self) {
        std::fs::write(&self.release, b"run").expect("open workload gate");
    }
}

#[derive(Default, Clone, Debug)]
pub struct OutputStats {
    pub chunks: usize,
    pub bytes: usize,
    pub stdout: usize,
    pub stderr: usize,
    pub truncated: usize,
    pub done: usize,
    pub policy_ok: usize,
    pub cgroup: Option<PathBuf>,
}

#[derive(Default, Clone)]
pub struct RecordingOutput(pub Arc<Mutex<OutputStats>>);

impl RecordingOutput {
    pub fn snapshot(&self) -> OutputStats {
        self.0.lock().expect("output stats lock").clone()
    }
}

impl OutputPublisher for RecordingOutput {
    fn sink_for(
        &self,
        _task: &solti_model::TaskId,
        generation: u64,
        attempt: u32,
    ) -> Option<OutputSink> {
        let stats = Arc::clone(&self.0);
        Some(OutputSink::new(generation, attempt, move |event| {
            if let OutputEvent::Chunk(chunk) = event {
                let mut stats = stats.lock().expect("output stats lock");
                stats.chunks += 1;
                stats.bytes += chunk.line.len();
                stats.truncated += usize::from(chunk.truncated);
                stats.done += usize::from(chunk.line.as_ref() == DONE);
                stats.policy_ok += usize::from(chunk.line.as_ref() == b"solti-bench-policy-ok");
                if let Some(path) = chunk.line.strip_prefix(b"solti-bench-cgroup=") {
                    stats.cgroup = Some(PathBuf::from(
                        std::str::from_utf8(path).expect("cgroup path UTF-8"),
                    ));
                }
                match chunk.stream {
                    StreamKind::Stdout => stats.stdout += 1,
                    StreamKind::Stderr => stats.stderr += 1,
                }
            }
        }))
    }
}

pub async fn wait_clean(runner: &SubprocessRunner) {
    tokio::time::timeout(WAIT_BOUND, async {
        loop {
            let status = runner.finalizer_status();
            assert!(
                status.healthy(),
                "subprocess finalizer lost progress: {status:?}"
            );
            assert_eq!(status.quarantined(), 0, "subprocess ownership quarantined");
            if status.owned() == 0 {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("subprocess ownership was not physically released");
}

pub async fn shutdown(runner: &SubprocessRunner) {
    runner
        .shutdown(WAIT_BOUND)
        .await
        .expect("subprocess runner shutdown");
    let status = runner.finalizer_status();
    assert!(!status.accepting());
    assert!(status.healthy());
    assert_eq!(status.owned(), 0);
    assert_eq!(status.quarantined(), 0);
}

#[cfg(unix)]
pub async fn wait_not_running(pids: &[u32]) {
    tokio::time::timeout(WAIT_BOUND, async {
        loop {
            if pids.iter().all(|&pid| !process_running(pid)) {
                return;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("controlled subprocess or descendant still running");
}

#[cfg(unix)]
fn process_running(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    if let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat"))
        && stat
            .rsplit_once(") ")
            .is_some_and(|(_, tail)| tail.starts_with('Z'))
    {
        // The runner owns/reaps the leader. A descendant's new parent owns its
        // wait status; a zombie no longer executes or holds output descriptors.
        return false;
    }
    // SAFETY: signal zero checks the exact PID reported by our child; it sends no signal.
    let result = unsafe { libc::kill(pid as libc::pid_t, 0) };
    result == 0 || std::io::Error::last_os_error().raw_os_error() != Some(libc::ESRCH)
}
