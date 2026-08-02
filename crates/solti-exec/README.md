# solti-exec

`solti-exec` provides concrete execution backends for the Solti SDK.

- `host-process` feature provides reusable policy and low-level process controls.
- `subprocess` feature implements a runner for `solti.io/v1`, kind `Subprocess`.
- `container` feature implements an engine-neutral runner for `solti.io/v1`, kind `Container`.
- `containerd` feature provides its native containerd 2.x engine.

Use this crate when an agent executes host processes or OCI containers.
The crate does not schedule tasks, store resources, or expose a network API.

> It does not discover or start engine daemons.

## Subprocess quick start

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

## Containerd quick start

Connect to one explicit containerd 2.x endpoint, then register its runner:

```rust,no_run
# #[cfg(feature = "containerd")]
# {
use std::sync::Arc;

use solti_exec::container::{
    containerd::{ContainerNetwork, ContainerdConfig, ContainerdEngine},
    register_container_runner,
};
use solti_runner::RunnerRouter;

async fn configure() -> Result<RunnerRouter, Box<dyn std::error::Error>> {
    let settings = ContainerdConfig::new(
        "/run/containerd/containerd.sock",
        "solti",
        "overlayfs",
        "io.containerd.runc.v2",
    )
    .with_network(ContainerNetwork::None);

    let engine = Arc::new(ContainerdEngine::connect(settings).await?);
    let mut router = RunnerRouter::new();
    register_container_runner(&mut router, "containerd", engine)?;
    Ok(router)
}
# }
```

`connect` probes the configured endpoint.
It requires containerd major version 2.
It also validates the selected snapshotter, platform, and OCI runtime.
Control RPCs use a 30-second deadline by default.
Image pull and unpack use a 10-minute deadline by default.
`ContainerdConfig` can override both deadlines and the cleanup window.
The workload wait has no deadline.

Building a container task performs no engine I/O.
Each attempt resolves and unpacks the image.
It creates a snapshot, container, and task.
It arms wait before start, streams output, waits, and requests cleanup of owned resources.
The same Taskvisor task can run more than once.
Retryable cleanup failures are retried on the same attempt.
A cleanup retry does not execute the workload again.
Permanent cleanup failures stop retry immediately.
The retries share a 30-second window by default.
Exhausted cleanup is a fatal attempt failure.

## What it does

### Subprocess

- registers a runner for the built-in `Subprocess` workload GVK;
- executes commands directly;
- decodes scripts and creates attempt-scoped script transport;
- applies environment and pinned working-directory policies;
- enforces an explicit file descriptor passlist on Linux;
- applies a bounded descriptor snapshot on other Unix platforms;
- streams stdout and stderr to tracing and the runner output sink;
- stops subprocesses on cancellation, timeout, or dropped task futures;
- applies POSIX rlimits;
- applies Linux cgroup, namespace, identity, capability, and seccomp controls;
- reports backend preparation and spawn failures through runner metrics;
- keeps host process controls separate from subprocess settings.

### Container

- provides an engine-neutral lifecycle for `Container` workloads;
- implements that lifecycle against native containerd 2.x services;
- renders an OCI runtime specification from the image, task, and runner policy;
- captures container stdout and stderr through private attempt-scoped FIFOs;
- removes only resources whose ownership is confirmed for the current attempt.

## Inputs and outputs

| API or value                              | Input                                   | Output                                        |
|-------------------------------------------|-----------------------------------------|-----------------------------------------------|
| `HostProcessPolicy`                       | Resource and process security controls  | Reusable host process policy                  |
| `HostProcessPolicy::prepare`              | Declarative host policy                 | Validated `PreparedHostProcessPolicy`         |
| `PreparedHostProcessPolicy`               | Optional attempt cgroup name            | `PreparedHostProcessAttempt`                  |
| `PreparedHostProcessAttempt`              | `std::process::Command`                 | Hooks and owning `AttemptProcessDomain`       |
| `register_subprocess_runner`              | Router and runner name                  | Registered default subprocess runner          |
| `register_subprocess_runner_with_backend` | Router, runner name, and backend config | Registered configured subprocess runner       |
| `RunnerRouter::build`                     | `Task` with a `Subprocess` workload     | Reusable `taskvisor::TaskRef`                 |
| `SubprocessBackendConfig`                 | Host policy, environment, cwd, output   | Runner-wide attempt settings                  |
| One task attempt                          | Resolved command or script              | Task result and optional stdout/stderr chunks |
| `ContainerProcessPolicy`                  | OCI process and resource controls       | Engine-neutral container process policy       |
| `ContainerdConfig`                        | Socket, namespace, plugins, and network | Native containerd adapter settings            |
| `ContainerdEngine::connect`               | `ContainerdConfig`                      | Connected and probed containerd 2.x engine    |
| `register_container_runner`               | Router, runner name, and engine         | Registered `Container` runner                 |
| One container attempt                     | Image, process overrides, and policy    | Task result and optional stdout/stderr chunks |

`SubprocessRunner` accepts only the exact built-in `Subprocess` GVK.
`ContainerRunner` accepts only the exact built-in `Container` GVK.
Routing and runner selection remain owned by `solti-runner`.

## Subprocess execution flow

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
 environment + pinned cwd + HostProcessPolicy
              ▼
      operating-system process
         ├── stdout ──► tracing + OutputSink
         ├── stderr ──► tracing + OutputSink
         └── exit / cancel / dropped future
                         ▼
               terminate process domain
                         ▼
                    wait / reap
                         ▼
                cleanup script transport
                    and cgroup
```

Attempt-scoped resources are created inside the Taskvisor task.
The same `TaskRef` can therefore run more than once under a restart policy.

## Containerd execution flow

```text
Task { workload: Container }
              │ exact GVK and optional runnerSelector
              ▼
         RunnerRouter
              ▼
       ContainerRunner
              │ each attempt
              ▼
     ContainerEngine
              ▼
  native containerd 2.x
     ├── pull and unpack image
     ├── prepare snapshot
     ├── create container and task
     ├── stream stdout and stderr
     ├── start and wait
     └── delete task, container, snapshot, and local I/O
```

The native adapter supports Linux containers on Linux.
It talks directly to containerd over the configured Unix socket.
It does not use CRI.

The `container` feature does not select an engine.
The final binary passes an `Arc<dyn ContainerEngine>` when it registers a runner.
`create_attempt` returns one stopped attempt with exit observation already armed.
Engine implementations must make `terminate` and `cleanup` idempotent.
They may clean only resources whose ownership is confirmed for that attempt.

`ContainerNetwork::None` creates an OCI network namespace.
The adapter does not configure an external interface, address, route, DNS, or NAT.
`ContainerNetwork::Host` omits that namespace and shares the host network namespace.
It does not add host `/etc/hosts` or `/etc/resolv.conf` mounts.
It does not change the OCI capability sets.
The native adapter's base capability set includes `CAP_NET_RAW`.
`ContainerProcessPolicy::with_capabilities` replaces that set when the final binary needs a narrower policy.
There is no CNI or bridge integration.

The task command overrides the image entrypoint.
Non-empty task arguments override the image command.
Task and runner environment values override image values.
Image users are accepted as exact `UID:GID` values.
Named users and numeric users without a group require explicit runner credentials.
The adapter does not create a user namespace.

The I/O root must be visible at the same path to the SDK process and containerd.
The adapter creates private `0700` attempt directories and `0600` FIFOs below it.
On Linux, every root path component must be a real directory owned by root or the effective UID.
Group-writable and world-writable components must have the sticky bit.

The upstream `containerd-client` dependency compiles its protocol bindings during the build.
Building the `containerd` feature therefore requires a usable `protoc` through `PROTOC` or `PATH`.

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
It creates fresh backing storage for every attempt.

Linux uses a sealed anonymous `memfd`.
The interpreter opens it through `/proc/self/fd`.
The final descriptor mode is `0444`.
Read access is published only after sealing.
This keeps script mode usable after an exact credential change.
The interpreter cannot open the script when procfs does not expose that path.
Other Unix platforms use an unlinked file with mode `0600`.
The interpreter opens it through `/dev/fd`.
The interpreter cannot open the script when that descriptor filesystem is unavailable.
Non-Unix platforms use a named temporary file.

The interpreter receives the file path before the configured arguments.
The backing storage is released when the attempt ends.

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

Every Unix path component is then opened without following symlinks.
The child enters the directory through the pinned descriptor.
Replacing the path after task construction cannot redirect the child.

The policy checks only the starting directory.
It does not confine file access after the process starts.

## File descriptors

Linux children inherit only standard streams and explicit passlist entries.
`SubprocessBackendConfig::with_passed_fd` adds an owned descriptor to that passlist.
The descriptor number is preserved.

Linux applies `close_range(CLOSE_RANGE_CLOEXEC)` to every descriptor from `3` upwards.
Process spawn fails when the running kernel does not support that operation.

Other Unix platforms inspect `/dev/fd` before process creation.
Descriptors opened concurrently after that snapshot must already use close-on-exec.
Process spawn fails when `/dev/fd` is unavailable.

## Process state

`ProcessConfig` contains optional Unix process normalization:

- `reset_signals` resets catchable signal dispositions and clears the signal mask;
- `new_session` creates a new session and process group with `setsid`;
- `umask` sets the file creation mask.

An empty `ProcessConfig` adds no process-state hook.
The subprocess runner still creates a dedicated process group on Unix.

## Backend configuration

Register a configured runner when attempts need resource or security controls:

```rust,no_run
use solti_exec::subprocess::{
    EnvPolicy, LogConfig, SubprocessBackendConfig,
    register_subprocess_runner_with_backend,
};
use solti_exec::host::{
    CgroupLimits, CpuMax, HostProcessPolicy, LinuxCapability, ProcessConfig,
    ProcessCredentials, RlimitConfig, SeccompPolicy, SecurityConfig,
};
use solti_runner::RunnerRouter;

fn configured() -> Result<(), solti_exec::ExecError> {
    let host_process = HostProcessPolicy::new()
        .with_process_config(ProcessConfig {
            reset_signals: true,
            new_session: true,
            umask: Some(0o077),
        })
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
            credentials: Some(ProcessCredentials::new(1000, 1000)),
            seccomp: SeccompPolicy::DenyHostControl,
            ..Default::default()
        });

    let backend = SubprocessBackendConfig::new()
        .with_host_process_policy(host_process)
        .with_env_policy(EnvPolicy::Clear)
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
The example requires features `subprocess` and `seccomp`.

The default backend uses an empty `HostProcessPolicy`.
It does not create a cgroup.
It does not enable optional rlimit, credential, namespace, capability, or seccomp controls.

The subprocess backend still clears the environment by default.
On Unix, it pins an explicit working directory and restricts descriptor inheritance.
It also gives every attempt a dedicated session and process group.

`HostProcessPolicy` is independent of the subprocess workload format.
Future process-based backends can translate it to their own execution model.
It must not be applied unchanged to an OCI runtime process or an in-process WASM engine.

## Custom process backends

The low-level API does not depend on Tokio, Taskvisor, models, or runner routing:

```text
HostProcessPolicy::prepare
              ▼
PreparedHostProcessPolicy
      ├── prepare_attempt
      └── attempt.apply_to_command(std::process::Command)
                         ▼
                        spawn
                         ▼
         terminate backend process boundary
                         +
          AttemptProcessDomain::terminate_tree
                         ▼
                     wait / reap
                         ▼
             AttemptProcessDomain::cleanup
```

The attempt token prevents configured cgroups from being skipped.
The cgroup name must contain one normal path component.
Keep the process domain until cleanup finishes.
The backend remains responsible for its process-specific termination boundary.

## Resource and security controls

| Control                            | Platform                 | Behavior                                               |
|------------------------------------|--------------------------|--------------------------------------------------------|
| `ProcessConfig`                    | Unix                     | Resets signal state, creates a session, or sets umask  |
| `RlimitConfig`                     | Unix                     | Sets hard `NOFILE`, `FSIZE`, and `CORE` ceilings       |
| `CgroupLimits`                     | Linux cgroup v2          | Limits CPU, memory, and process count per attempt      |
| `Namespaces`                       | Linux                    | Creates mount, network, IPC, UTS, or cgroup namespaces |
| `ProcessCredentials`               | Linux                    | Replaces all user, group, and supplementary IDs        |
| Linux capabilities                 | Linux                    | Drops the bounding set and keeps explicit capabilities |
| `no_new_privs`                     | Linux                    | Prevents privilege gain through `execve`               |
| `SeccompPolicy::DenyHostControl`   | Linux, feature `seccomp` | Rejects a host-control syscall denylist with `EPERM`   |

The subprocess runner creates a session for every Unix attempt.
`ProcessConfig::new_session` provides the same primitive to custom process backends.

Cgroups are created below the process's current cgroup v2 directory.
The current directory is resolved through `/proc/self/cgroup` and the active cgroup2 mount.
`HostProcessPolicy::with_cgroup_parent` selects another absolute cgroup v2 directory.
One child group is created per attempt.
The parent, attempt directory, and control files are pinned before process creation.
Its `cgroup.max.depth` is set to zero before process creation.
Configured cgroups use `cgroup.kill` when the running kernel provides it.
Unix attempts also signal the dedicated process group before reaping the leader.
Without `cgroup.kill`, the process group is the only attempt-wide primitive.
`AttemptProcessDomain::cleanup` requires `cgroup.events` to report `populated 0`.
It refuses to remove a pathname that no longer identifies the owned cgroup.

Rlimit requests above the current hard limit are clamped.
The resulting value becomes both the soft and hard limit.
`PreparedHostProcessPolicy::rlimits` exposes the resolved ceilings.
A Linux child retaining `CAP_SYS_RESOURCE` can raise a hard limit again.

`ProcessCredentials` sets the real, effective, and saved user and group IDs.
Its supplementary group list is exact.
An empty list clears inherited supplementary groups.
Credentials require explicit `no_new_privs = true`.

`keep_caps` requires `drop_all_caps = true`.
Capability dropping requires explicit `no_new_privs = true`.
The retained capabilities must already be available to the agent process.

`DenyHostControl` enables `no_new_privs` before installing its filter.
This implicit setting does not replace the explicit credential and capability contract.
It rejects host-control operations such as mounts, namespace entry, kernel module loading, BPF, and ptrace.
On LP64 `x86_64`, it also rejects the x32 syscall ABI.

`DenyHostControl` is a denylist.
It is not a complete syscall allowlist.

These controls provide host process hardening.
They do not form a complete sandbox for untrusted code.
The delegated cgroup parent must not be writable by workloads.
A workload that can write its parent `cgroup.procs` can leave the attempt cgroup.
Concurrent parent mutations also invalidate cleanup ownership.

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

After leader exit is observed, the runner waits up to five seconds for both pipes to close.
It then terminates the cgroup and process-group boundaries before reaping the leader.
A running leader receives its own termination request too.

The default `BuildContext` disables live output.
`solti-core` installs the standard output publisher.

## Exit and cancellation

| Event                                      | Task result                                      |
|--------------------------------------------|--------------------------------------------------|
| Exit code `0`                              | Success                                          |
| Non-zero with `fail_on_non_zero = true`    | Retryable failure with the exit code             |
| Non-zero with `fail_on_non_zero = false`   | Success                                          |
| Cooperative cancellation                   | `TaskError::Canceled`                            |
| Process-domain lifecycle error             | Fatal failure                                    |
| Permanent spawn or materialization error   | Fatal failure                                    |
| Other operating-system I/O error           | Retryable failure                                |

On Unix, every attempt owns a session and process group.
A configured cgroup uses `cgroup.kill` when it is available.
The runner signals the dedicated process group and a running leader before reap.
Normal completion applies the same cleanup.
Termination, reap, and cleanup errors are fatal.
Without `cgroup.kill`, only the process-group boundary remains.
That boundary cannot reach descendants that enter another process group or session.

The runner owns the wait status of every child it starts.
Before process creation, it prepares one Tokio-independent reaper worker.
A dropped runner future moves the child and host domain to that worker.
The worker reaps the leader before cgroup cleanup.
The dropped future does not wait for process exit.
The embedding process must not call process-wide `waitpid` for arbitrary children.
It must not configure automatic `SIGCHLD` reaping.
If wait ownership is lost, the attempt fails and releases its numeric process identity.

On other platforms, cancellation falls back to the child process.

## Features

| Feature        | Default | Effect                                            |
|----------------|---------|---------------------------------------------------|
| `container`    | Off     | Engine-neutral container runner                   |
| `containerd`   | Off     | Native containerd 2.x engine; implies `container` |
| `host-process` | Off     | Host process policy and low-level controls        |
| `subprocess`   | Off     | Subprocess runner; implies `host-process`         |
| `seccomp`      | Off     | Linux seccomp denylist; implies `host-process`    |

Enable both `subprocess` and `seccomp` to apply the filter to subprocess attempts.
The container runner passes `SeccompPolicy` to its engine.
The native containerd engine renders `Disabled` and `DenyHostControl` into the OCI specification.

With no features, the crate exposes no policy or execution backend.

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

## Examples

### Internal examples

These examples exercise only `solti-exec` backends and their direct contracts.
They do not schedule tasks, persist resources, or expose a network API.
Each example starts with a text flow diagram, then explains its inputs, lifecycle, and result.

Start with one real subprocess command:

```bash
cargo run -p solti-exec --example subprocess_command --features subprocess
```

| Example                                                           | Features       | What it shows                                                        |
|-------------------------------------------------------------------|----------------|----------------------------------------------------------------------|
| [subprocess_command.rs](examples/subprocess_command.rs)           | `subprocess`   | Routing, environment, cwd, output, execution, and cleanup.           |
| [subprocess_script.rs](examples/subprocess_script.rs)             | `subprocess`   | Script decoding, reusable tasks, and attempt-scoped transport.       |
| [container.rs](examples/container.rs)                             | `containerd`   | One real native container attempt, output, and owned cleanup.        |
| [host_process_policy.rs](examples/host_process_policy.rs)         | `host-process` | Low-level preparation, attachment, wait ownership, and cleanup.      |
| [custom_container_engine.rs](examples/custom_container_engine.rs) | `container`    | Custom engine contract, process policy, output, and lifecycle order. |
| [containerd_config.rs](examples/containerd_config.rs)             | `containerd`   | Native adapter configuration, network modes, and explicit connect.   |

Run the remaining examples explicitly:

```bash
cargo run -p solti-exec --example subprocess_script --features subprocess
cargo run -p solti-exec --example container --features containerd
cargo run -p solti-exec --example host_process_policy --features host-process
cargo run -p solti-exec --example custom_container_engine --features container
cargo run -p solti-exec --example containerd_config --features containerd
```

The native container example requires Linux, containerd 2.x, its configured plugins, and a cached or reachable image.
Its endpoint and runtime values can be overridden through the environment variables printed by the example.

The `containerd_config` example does not contact a daemon by default.
Pass `--connect` only when its configured socket and a compatible containerd 2.x daemon are available.

### Full examples

Application-level compositions live in the [`solti` examples](https://github.com/soltiHQ/sdk/tree/main/crates/solti/examples).
They combine component crates and own routing, reconciliation, supervision, and API lifecycle.

## Contributor guide

See the [solti-exec source guide](https://github.com/soltiHQ/sdk/blob/main/crates/solti-exec/ARCHITECTURE.md) for module ownership, execution flows, resource ownership, and invariants.
