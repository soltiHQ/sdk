---
title: Run subprocesses
description: Build Command and Script workloads, own their process resources, and close the subprocess runner safely.
---

# Run subprocesses

A subprocess workload runs a program on the agent host.
Use `Command` for an executable and an argument vector.
Use `Script` for a base64-encoded body and an explicit interpreter.
Neither mode provides a filesystem or security sandbox by itself.

## Participants

| Component | Role and reason to use it |
|---|---|
| `solti-model` | Defines `SubprocessSpec`, `Command`, `Script`, task environment, and the outer execution policy. |
| `solti-runner` | Selects the backend and supplies build cancellation, environment overrides, metrics, and output. |
| `solti-exec/subprocess` | Validates the workload, spawns attempts, reads pipes, terminates owned process scopes, and owns deferred cleanup. This feature also enables `host-process`. |
| `solti-exec/host-process` | Provides reusable host controls for the subprocess runner or an application-owned process backend. Linux host seccomp also needs `seccomp`. |
| `solti-core` and Taskvisor | Reconcile and supervise the Task, including admission, timeout, restart, cancellation, output subscriptions, and history. |

`solti-exec` has no default features.
With the facade crate, enable `core,exec-subprocess` for the complete supervised path.
See [containers and isolation](containers-and-isolation.md) for host policy and platform limits.

## Register the backend and keep its handle

```rust
use solti_exec::subprocess::{
    EnvPolicy, SubprocessBackendConfig, register_subprocess_runner_with_backend,
};
use solti_runner::RunnerRouter;

let mut router = RunnerRouter::new();
let backend = SubprocessBackendConfig::new()
    .with_env_policy(EnvPolicy::Clear)
    .with_max_script_body_bytes(256 * 1024)
    .with_cleanup_capacity(256);
let subprocess = register_subprocess_runner_with_backend(
    &mut router,
    "local",
    backend,
)?;
```

This is setup inside a fallible application function.
Registration validates the full backend configuration and adds `solti.io/runner-name=local`.
The default helper, `register_subprocess_runner`, uses the default configuration.
Keep the returned runner handle after moving the router into core; it owns a separate shutdown boundary.

The default cleanup capacity is 1024 active or deferred attempts.
The same configured number independently bounds build-time working-directory operations on a runner-owned worker.
It is not the controller queue capacity or the maximum number of Task resources.

## Describe work without starting a process

The built-in GVK is `solti.io/v1`, kind `Subprocess`.
This factory accepts an executable and arguments chosen by the application:

```rust
use solti_model::{
    Flag, RestartPolicy, SubprocessMode, SubprocessSpec, TaskEnv,
    TaskManifest, TaskSpec, TaskWorkload,
};

fn command_manifest(
    program: String,
    args: Vec<String>,
) -> Result<TaskManifest, Box<dyn std::error::Error>> {
    let workload = TaskWorkload::Subprocess(SubprocessSpec::new(
        SubprocessMode::Command { command: program, args },
        TaskEnv::new(),
        None,
        Flag::enabled(),
    ));
    let spec = TaskSpec::builder("commands", workload, 30_000_u64)
        .restart(RestartPolicy::Never)
        .build()?;
    Ok(TaskManifest::new("run-command", spec)?)
}
```

Submit the manifest through `SupervisorApi::create_task` and observe reconciliation before interpreting execution state.
The timeout and restart settings apply when Taskvisor supervises the built task.
Calling a built `TaskRef` directly does not install that outer policy.
See [managing tasks](managing-tasks.md) and [lifecycle and admission](lifecycle-and-admission.md).

Command mode passes arguments directly to the program. It does not expand shell syntax, variables, pipes, or globs.
An explicit command such as `/bin/sh` with `-c` is a caller-selected shell workload.
The complete [task_subprocess example](../crates/solti/examples/task_subprocess.rs) uses the current executable instead.

## Separate build, attempt, and cleanup

```text
build
    → validate executable, arguments, environment, and script
    → resolve and pin an explicit cwd
    → return a reusable TaskRef

each attempt
    → reserve cleanup ownership
    → prepare host domain and fresh script transport
    → spawn child and pipe readers
    → wait or observe cancellation
    → terminate owned process scope, reap leader, clean domain

dropped attempt
    → transfer reserved ownership to the runner finalizer
```

Building does not spawn a process or create attempt-scoped script and cgroup resources.
Working-directory resolution is an owned, bounded blocking operation and observes build cancellation.
The core-managed build deadline is separate from the attempt timeout.

Each attempt checks preexisting cancellation before acquiring output or creating resources.
Cleanup admission is reserved before script transport, cgroups, or processes are created.
A full cleanup domain rejects admission; it does not create an untracked process and wait for capacity later.

The child has null stdin and piped stdout and stderr.
On Unix, the runner creates a dedicated session and process group.
Normal leader exit also triggers termination of the remaining owned process scope; descendants are not left running merely because the leader succeeded.
Without a usable cgroup kill operation, a descendant that leaves the process group is outside the group termination boundary.
On non-Unix platforms, this runner's termination boundary is the child process, not a Unix-style process tree.

## Understand Script transport

Script mode requires standard padded base64 containing nonempty UTF-8 text.
The model limit is 2 MiB of decoded script bytes.
`with_max_script_body_bytes` can lower that limit, not raise it.
The runner decodes and validates the body during build, then retains the decoded body for reuse.

Every attempt materializes fresh transport and passes its path as the interpreter's first argument, followed by the workload arguments:

| Platform | Transport and prerequisite |
|---|---|
| Linux | Sealed anonymous `memfd`, addressed through `/proc/self/fd`. The interpreter needs usable procfs descriptor paths. |
| Other Unix | An unlinked temporary file, addressed through `/dev/fd`. The interpreter needs descriptor-path support. |
| Non-Unix | A named temporary file retained for the attempt. |

The attempt owns the transport through execution and cleanup.
Reusing the built task does not reuse a live script file from an earlier attempt.
The [script example](../crates/solti-exec/examples/subprocess_script.rs) runs one built task twice with `/bin/sh` and checks distinct attempt numbers in output.

## Configure the host boundary

Environment is assembled in this order:

1. `EnvPolicy::Clear`, `Inherit`, or `Allowlist` determines the host environment base.
2. Task environment values override that base.
3. Runner environment values override task values.

The default is `Clear`, with `/usr/local/bin:/usr/bin:/bin` supplied as `PATH` when the merged values do not set it.
`Allowlist` supplies that fallback only when neither its list nor the merged values names `PATH`.
`Inherit` exposes the agent environment to the child.
See [configuration](configuration.md) before deciding which values a workload may receive.

`CwdPolicy::Unrestricted` accepts any explicit directory; absent `cwd` inherits the agent's working directory.
`CwdPolicy::Roots` requires an explicit directory below a configured root.
Roots are prepared at runner construction and the task directory is pinned during build.
On Unix, child startup uses the retained directory descriptor instead of resolving the path again.
Replacing the path after build does not redirect that start directory.
This policy does not restrict file access after startup.

The Unix descriptor boundary excludes unrelated agent descriptors.
Explicit `with_passed_fd` entries retain their descriptor numbers and must be at least 3; the runner owns standard streams.
Linux uses `close_range` with close-on-exec and fails closed when the required operation is unavailable.
The normal macOS path uses an atomic `posix_spawn` allowlist; policies requiring the fork path use the documented parent snapshot and child-side descriptor sweep.
See the [backend implementation](../crates/solti-exec/src/subprocess/backend.rs) and [execution architecture](../crates/solti-exec/ARCHITECTURE.md) for platform details.

An empty `HostProcessPolicy` does not request rlimits, resource cgroups, credentials, namespaces, capabilities, or seccomp.
Those controls are application-owned runner policy, not arbitrary fields accepted from the Task workload.
[Containers and isolation](containers-and-isolation.md) explains the available controls and termination authority they require.

## Read output without changing the execution contract

`LogConfig` defaults to a 4096-byte published line limit, a 65536-byte retained-input ceiling, and no copy to tracing.
The effective published prefix is the smaller of the two byte limits.
Oversized suffixes are drained through the next newline and the chunk is marked truncated.
Sink data preserves raw bytes, including invalid UTF-8.

Tracing is separate and opt-in through `emit_output_to_tracing`.
It uses a sanitized, lossy text view; stdout uses INFO and stderr uses WARN by default.
With neither an output sink nor tracing enabled, the runner still drains the pipes.
It does not retain a durable log on its own.

After normal leader exit, output readers receive a bounded drain window before process-scope cleanup.
Cancellation requests termination before waiting for output drain.
The normal drain grace is five seconds; it is not a promise that every last byte reaches a subscriber.
Dropped attempts abort their owned reader tasks.
Reader failures produce diagnostics rather than replacing the process result.
See [output and history](output-and-history.md) for live delivery, lag, and retention boundaries.

## Interpret failures and cancellation

`failOnNonZero` controls the exit-status result.
When enabled, a nonzero exit or signal termination is a retryable task failure; an available exit code is preserved.
When disabled, an unsuccessful exit status does not itself fail the task.
This does not suppress spawn, termination, reap, or cleanup errors.

Permanent spawn and preparation errors, including missing executables, permission errors, invalid input, and unsupported controls, are fatal.
Other I/O errors use the backend's retryable classification.
Taskvisor applies the outer restart policy to the returned result; the subprocess runner does not start retries itself.

Cancellation is latched through cleanup and wins a simultaneous cancellation/leader-exit observation.
Successful cleanup returns `TaskError::Canceled`.
A fatal termination, reap, or cleanup error is not hidden by cancellation.
The agent must not use a competing process-wide child reaper or enable automatic `SIGCHLD` reaping for children owned by this runner.

## Shut down both owners

Stopping an attempt future cannot await asynchronous cleanup in `Drop`.
The runner transfers its already-reserved process ownership to a Tokio-independent finalizer.
The finalizer retains the child and host domain through termination, reap, and domain removal.
Persistent OS cleanup failures eventually quarantine the ownership, keep its capacity charged, and close new admission.
`finalizer_status()` exposes that state; a completed logical Task is not evidence that deferred cleanup is empty.

Stop all users of the runner, shut down core, then await the runner itself:

```rust
let core_result = supervisor.shutdown().await;
let runner_result = subprocess.shutdown(std::time::Duration::from_secs(5)).await;
core_result?;
runner_result?;
```

The runner call closes cleanup and cwd admission and joins its workers.
It is terminal and idempotent; a canceled or timed-out wait can be retried.
A timeout, lost worker, or quarantined ownership is an error, not a drained result.
Application error handling must still attempt both shutdowns when execution or core shutdown returns an error.
The complete example captures those results separately before returning them.

## Run existing examples

```bash
cargo run -p solti --example task_subprocess --features core,exec-subprocess
cargo run -p solti-exec --example subprocess_command --features subprocess
cargo run -p solti-exec --example subprocess_script --features subprocess
cargo run -p solti-exec --example host_process_policy --features host-process
```

`task_subprocess` is the complete supervised path: a real child, a readiness handshake before output subscription, terminal state, retained history, and both shutdown boundaries.
It uses a local TCP listener and the current executable, not a shell or external service.
The command and script examples exercise the runner directly with detached task contexts; they do not demonstrate Taskvisor timeout or restart policy.
The script and Unix host-policy examples require `/bin/sh`.
The host-policy example skips process execution on non-Unix platforms.

## See also

- [Routing and custom runners](routing-and-custom-runners.md): build contracts and cancellation.
- [Containers and isolation](containers-and-isolation.md): host controls, cgroups, credentials, and seccomp.
- [Cancellation and shutdown](cancellation-and-shutdown.md): logical settlement and physical ownership.
- [Example catalog](example-catalog.md): runnable integration paths.
- Source: [workload model](../crates/solti-model/src/domain/kind/subprocess.rs), [backend configuration](../crates/solti-exec/src/subprocess/backend.rs), [attempt lifecycle](../crates/solti-exec/src/subprocess/runner.rs), [script transport](../crates/solti-exec/src/subprocess/script.rs), and [output reader](../crates/solti-exec/src/output.rs).
