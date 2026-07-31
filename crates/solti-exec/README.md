# solti-exec

`solti-exec` provides concrete execution backends for the Solti SDK.
The `subprocess` feature implements a runner for `solti.io/v1`, kind `Subprocess`.
It turns a `Task` resource into a reusable Taskvisor task that starts one operating-system process per attempt.

Use this crate when an agent executes commands or scripts on its host.
The crate does not schedule tasks, store resources, or expose a network API.

## Quick start

Register the default subprocess runner, then build a Taskvisor task through `RunnerRouter`:

```rust,no_run
use solti_exec::subprocess::register_subprocess_runner;
use solti_model::{
    Flag, SubprocessMode, SubprocessSpec, Task, TaskEnv, TaskSpec, TaskWorkload,
};
use solti_runner::RunnerRouter;

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let mut router = RunnerRouter::new();
    register_subprocess_runner(&mut router, "default")?;

    let workload = TaskWorkload::Subprocess(SubprocessSpec::new(
        SubprocessMode::Command {
            command: "echo".into(),
            args: vec!["hello".into()],
        },
        TaskEnv::new(),
        None,
        Flag::enabled(),
    ));
    let spec = TaskSpec::builder("jobs", workload, 5_000_u64).build()?;
    let task = Task::new("hello", spec)?;

    let task_ref = router.build(&task)?;
    assert!(task_ref.name().starts_with("default-jobs-"));
    Ok(())
}
```

`RunnerRouter::build` constructs the task but does not start it.
`solti-core` submits the returned task to Taskvisor during reconciliation.

## What it does

- registers a runner for the built-in `Subprocess` workload GVK;
- executes commands directly;
- decodes scripts and materializes one temporary file per attempt;
- applies environment and working-directory policies;
- streams stdout and stderr to tracing and the runner output sink;
- stops subprocesses on cancellation, timeout, or dropped task futures;
- applies POSIX rlimits;
- applies Linux cgroup, namespace, identity, capability, and seccomp controls;
- reports backend preparation and spawn failures through runner metrics.

## Inputs and outputs

| API or value                              | Input                                      | Output                                        |
|-------------------------------------------|--------------------------------------------|-----------------------------------------------|
| `register_subprocess_runner`              | Router and runner name                     | Registered default subprocess runner          |
| `register_subprocess_runner_with_backend` | Router, runner name, and backend config    | Registered configured subprocess runner       |
| `RunnerRouter::build`                     | `Task` with a `Subprocess` workload        | Reusable `taskvisor::TaskRef`                 |
| `SubprocessBackendConfig`                 | Limits, security, environment, and logging | Runner-wide attempt settings                  |
| One task attempt                          | Resolved command or script                 | Task result and optional stdout/stderr chunks |

`SubprocessRunner` accepts only the exact built-in `Subprocess` GVK.
Routing and runner selection remain owned by `solti-runner`.

## Execution flow

```text
Task { workload: Subprocess }
              │ exact GVK and optional runnerSelector
              ▼
         RunnerRouter
              ▼
      SubprocessRunner
              │ build
              ▼
      taskvisor::TaskRef
              │ each attempt
              ▼
 environment + cwd + backend preparation
              ▼
      operating-system process
         ├── stdout ──► tracing + OutputSink
         ├── stderr ──► tracing + OutputSink
         └── exit / cancel / dropped future
                         ▼
                cleanup process group,
                script file, and cgroup
```

Attempt-scoped resources are created inside the Taskvisor task.
The same `TaskRef` can therefore run more than once under a restart policy.

## Commands and scripts

`Command` executes the supplied program and arguments directly:

```rust
use solti_model::SubprocessMode;

let mode = SubprocessMode::Command {
    command: "/usr/bin/env".into(),
    args: vec!["printf".into(), "ready\n".into()],
};
```

`Script` requires an explicit interpreter.
The body uses standard base64:

```rust
use base64::Engine;
use base64::engine::general_purpose::STANDARD as BASE64;
use solti_model::SubprocessMode;

let mode = SubprocessMode::Script {
    interpreter: "/bin/sh".into(),
    body: BASE64.encode(b"printf 'ready: %s\\n' \"$1\""),
    args: vec!["agent".into()],
};
```

The runner decodes and validates the script while building the task.
It writes a fresh temporary file for every attempt.
On Unix, the file mode is `0600`.
The interpreter receives the file path before the configured arguments.
The file is removed when the attempt ends.

The model hard limit is `MAX_SCRIPT_BODY_BYTES` after decoding.
`SubprocessBackendConfig::with_max_script_body_bytes` can lower that limit.
It cannot raise it.

## Environment

`SubprocessBackendConfig` controls the parent environment visible to the child:

| Policy                 | Parent environment                     |
|------------------------|----------------------------------------|
| `EnvPolicy::Clear`     | Cleared; this is the default           |
| `EnvPolicy::Inherit`   | Inherited                              |
| `EnvPolicy::Allowlist` | Only named parent variables are copied |

The workload environment is merged with `BuildContext` runner values.
Runner values win when both inputs contain the same key.
The merged values are applied after the selected parent policy.

`Clear` injects `/usr/local/bin:/usr/bin:/bin` when the merged environment does not define `PATH`.
`Allowlist` injects the same path only when neither the merged environment nor the allowlist names `PATH`.

## Working directory

`CwdPolicy::Unrestricted` accepts any workload `cwd`.
It is the default.

`CwdPolicy::Roots` requires an explicit `cwd` under one configured root:

```rust,no_run
use std::path::PathBuf;
use solti_exec::subprocess::{CwdPolicy, SubprocessBackendConfig};

let backend = SubprocessBackendConfig::new().with_cwd_policy(
    CwdPolicy::Roots(vec![PathBuf::from("/srv/jobs")]),
);
```

Roots are canonicalized when the runner is created.
The workload directory is canonicalized when the task is built.
This resolves symlinks and `..` before comparison.

The policy checks only the starting directory.
It does not confine file access after the process starts.

## Backend configuration

Register a configured runner when attempts need resource or security controls:

```rust,no_run
use solti_exec::subprocess::{
    EnvPolicy, LogConfig, SubprocessBackendConfig,
    register_subprocess_runner_with_backend,
};
use solti_exec::{
    CgroupLimits, CpuMax, LinuxCapability, RlimitConfig, SecurityConfig,
};
use solti_runner::RunnerRouter;

fn configured() -> Result<(), solti_exec::ExecError> {
    let backend = SubprocessBackendConfig::new()
        .with_env_policy(EnvPolicy::Clear)
        .with_rlimits(RlimitConfig {
            max_open_files: Some(1024),
            max_file_size_bytes: Some(64 * 1024 * 1024),
            disable_core_dumps: true,
        })
        .with_cgroups(CgroupLimits {
            cpu: Some(CpuMax {
                quota: Some(50_000),
                period: 100_000,
            }),
            memory: Some(256 * 1024 * 1024),
            pids: Some(64),
        })
        .with_security(SecurityConfig {
            drop_all_caps: true,
            keep_caps: vec![LinuxCapability::NetBindService],
            no_new_privs: true,
            ..Default::default()
        })
        .with_logger(LogConfig {
            max_line_length: 4096,
            max_line_bytes: 64 * 1024,
            stdout_info: true,
            stderr_warn: true,
        });

    let mut router = RunnerRouter::new();
    register_subprocess_runner_with_backend(&mut router, "restricted", backend)
}
```

Configuration is validated when the runner is created.
Configured controls are fail-closed.
An unsupported or invalid control rejects the runner or fails the attempt.

## Resource and security controls

| Control                         | Platform                 | Behavior                                               |
|---------------------------------|--------------------------|--------------------------------------------------------|
| `RlimitConfig`                  | Unix                     | Sets soft `NOFILE`, `FSIZE`, and `CORE` limits         |
| `CgroupLimits`                  | Linux cgroup v2          | Limits CPU, memory, and process count per attempt      |
| `Namespaces`                    | Linux                    | Creates mount, network, IPC, UTS, or cgroup namespaces |
| UID and GID                     | Linux                    | Clears supplementary groups, then changes identity     |
| Linux capabilities              | Linux                    | Drops the bounding set and keeps explicit capabilities |
| `no_new_privs`                  | Linux                    | Prevents privilege gain through `execve`               |
| `SeccompPolicy::BlockDangerous` | Linux, feature `seccomp` | Rejects a host-control syscall denylist with `EPERM`   |

Cgroups are created below the process's current delegated cgroup.
`with_cgroup_parent` selects another absolute cgroup v2 directory.
One child group is created per attempt.

Rlimit requests above the current hard limit are clamped.
The hard limit is not changed.

`keep_caps` requires `drop_all_caps = true`.
The retained capabilities must already be available to the agent process.

`BlockDangerous` is a denylist.
It is not a complete syscall allowlist.

## Output

Stdout and stderr are read line by line.
`LogConfig` controls both tracing and live output:

| Field             | Default | Behavior                                         |
|-------------------|---------|--------------------------------------------------|
| `max_line_length` | `4096`  | Truncates after this many Unicode scalar values  |
| `max_line_bytes`  | `65536` | Drains the remainder after this byte limit       |
| `stdout_info`     | `true`  | Uses `INFO` for stdout; otherwise `DEBUG`        |
| `stderr_warn`     | `true`  | Uses `WARN` for stderr; otherwise `DEBUG`        |

Tracing output escapes control characters except tabs.
The `OutputSink` path keeps control characters unchanged after decoding and truncation.
Invalid UTF-8 is replaced during line decoding.
Child stdin is null.
Stdout and stderr are always piped.

After the leader exits, the runner waits up to five seconds for both pipes to close.
It then kills a process group that still holds them open.

The default `BuildContext` disables live output.
`solti-core` installs the standard output publisher.

## Exit and cancellation

| Event                                      | Task result                                      |
|--------------------------------------------|--------------------------------------------------|
| Exit code `0`                              | Success                                          |
| Non-zero with `fail_on_non_zero = true`    | Retryable failure with the exit code             |
| Non-zero with `fail_on_non_zero = false`   | Success                                          |
| Cooperative cancellation                   | `TaskError::Canceled`                            |
| Permanent spawn or materialization error   | Fatal failure                                    |
| Other operating-system I/O error           | Retryable failure                                |

On Unix, every attempt owns a process group.
Cancellation and dropped task futures kill the complete group.
Normal completion also stops remaining descendants.

On other platforms, cancellation falls back to the child process.

## Features

| Feature      | Default | Effect                                         |
|--------------|---------|------------------------------------------------|
| `subprocess` | Off     | Subprocess runner and host-process integration |
| `seccomp`    | Off     | Linux seccomp denylist; implies `subprocess`   |

With no features, the crate exposes no execution backend.

## Errors

`ExecError` covers runner registration and backend configuration:

| Variant               | Cause                                                |
|-----------------------|------------------------------------------------------|
| `Router`              | `RunnerRouter` rejected registration                 |
| `InvalidRunnerConfig` | Runner name or backend settings are invalid          |
| `Io`                  | An operating-system resource could not be prepared   |

Workload construction failures are returned through `solti_runner::RunnerError`.
Attempt failures use `taskvisor::TaskError`.

Runner names use Kubernetes label-value rules.
Registration adds `solti.io/runner-name=<name>` to the runner labels.
Duplicate names are rejected by `RunnerRouter`.
