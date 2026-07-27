# solti-exec
Task execution backends for the solti task system.

Provides concrete `Runner` implementations that turn `Task` resources into running OS processes.

It currently ships a single backend: `SubprocessRunner` with POSIX rlimits and optional Linux sandboxing (cgroup v2, capabilities, seccomp, namespaces).

## Quick start
```rust,no_run
use solti_exec::subprocess::{
    SubprocessBackendConfig, register_subprocess_runner, register_subprocess_runner_with_backend,
};
use solti_exec::{RlimitConfig, SecurityConfig};
use solti_runner::RunnerRouter;

fn main() -> Result<(), solti_exec::ExecError> {
    let mut router = RunnerRouter::new();
    register_subprocess_runner(&mut router, "default")?;

    // With sandboxing:
    let backend = SubprocessBackendConfig::new()
        .with_rlimits(RlimitConfig { max_open_files: Some(1024), ..Default::default() })
        .with_security(SecurityConfig { no_new_privs: true, ..Default::default() });
    register_subprocess_runner_with_backend(&mut router, "secure", backend)?;
    Ok(())
}
```

## Architecture
```text
 Task { spec.workload: Subprocess(..) }
     │
     ▼  RunnerRouter::pick()
 SubprocessRunner
     │
     ├──► build_task_config(task, ctx)
     │     ├──► validate/resolve mode → command + args + optional script body
     │     ├──► merge_env(task_env, runner_env)
     │     └──► SubprocessTaskConfig { run_id, command, args, env, cwd }
     │
     └──► each task attempt
           └──► run_subprocess(ctx, cancel)
                 ├──► allocate attempt + OutputSink
                 ├──► prepare backend (cgroup dirs, if configured)
                 ├──► materialize script tempfile (Script mode)
                 ├──► build Command + apply pre_exec hooks
                 ├──► spawn + pipe stdout/stderr
                 ├──► acquire OutputSink + start log streams
                 ├──► select! biased { child.wait(), cancel → killpg }
                 ├──► record metrics
                 └──► cleanup cgroup (if any)
```

## Subprocess lifecycle
```text
 task attempt ──► allocate attempt/output ──► prepare_backend ──► script tempfile ──► spawn
                                      (when needed)        │
                                                           ├──► OutputSink + log streams
                                                           ├──► child.wait() → evaluate exit
                                                           ├──► cancel → kill + reap
                                                           └──► metrics + cleanup
```

`biased` select prefers `child.wait()` over `cancel.cancelled()` — a process that has already exited cleanly is never misreported as cancelled, even if the cancel token fired in the same microsecond.

## Key types

| Type                      | Description                                                 |
|---------------------------|-------------------------------------------------------------|
| `SubprocessRunner`        | `Runner` impl for `TaskWorkload::Subprocess`                |
| `SubprocessBackendConfig` | Builder for rlimits + cgroups + security + logger settings  |
| `LogConfig`               | Stdout/stderr logging: truncation length, log levels        |
| `RlimitConfig`            | POSIX rlimits (nofile, fsize, core)                         |
| `CgroupLimits`            | cgroup v2: CPU quota/period, memory, PIDs                   |
| `CpuMax`                  | CPU quota + period for `cpu.max`                            |
| `SecurityConfig`          | Capability drop + `no_new_privs`                            |
| `LinuxCapability`         | Capability enum with kernel `cap_value` constants           |
| `ExecError`               | Configuration and spawn-time errors                         |

## Backend config
```rust,no_run
use solti_exec::subprocess::{LogConfig, SubprocessBackendConfig};
use solti_exec::{CgroupLimits, CpuMax, LinuxCapability, RlimitConfig, SecurityConfig};

let backend = SubprocessBackendConfig::new()
    .with_rlimits(RlimitConfig {
        max_open_files: Some(1024),
        ..Default::default()
    })
    .with_cgroups(CgroupLimits {
        cpu: Some(CpuMax { quota: Some(50_000), period: 100_000 }),
        memory: Some(128 * 1024 * 1024),
        pids: Some(32),
        ..Default::default()
    })
    .with_security(SecurityConfig {
        drop_all_caps: true,
        keep_caps: vec![LinuxCapability::NetBindService],
        no_new_privs: true,
        ..Default::default()
    })
    .with_logger(LogConfig {
        max_line_length: 4096,
        stdout_info: true,
        stderr_warn: true,
        ..Default::default()
    });
```

Backend controls are optional. The child environment is cleared by default.

Configured limits and security controls are fail-closed. Invalid or unsupported settings are rejected when the runner is created. A pre-exec enforcement failure aborts the spawn. `keep_caps` requires `drop_all_caps = true`.

## Sandboxing (pre_exec hooks)
```text
 fork()
 ┌───────────────────────────────────────────────────────────────────┐
 │  child process (before execve)                                    │
 │                                                                   │
 │  1. rlimits: getrlimit → clamp → setrlimit                        │
 │  2. cgroup: join the prepared per-attempt group                   │
 │  3. security: namespaces → identity → capabilities → seccomp     │
 │                                                                   │
 │  execve(command, args)                                            │
 └───────────────────────────────────────────────────────────────────┘
```

Pre-exec hooks use operating-system process-control calls. Configuration is prepared before the process is forked.

## Registration
```text
 register_subprocess_runner(&mut router, "default")
     ├──► SubprocessRunner::new("default")
     ├──► label "solti.io/runner-name" = "default"
     └──► router.register_with_labels()

 register_subprocess_runner_with_backend(&mut router, "secure", backend)
     ├──► validate backend config
     ├──► SubprocessRunner::with_config("secure", backend)
     ├──► label "solti.io/runner-name" = "secure"
     └──► router.register_with_labels()
```

Duplicate names are rejected by `RunnerRouter`.

## Error model
```text
 Variant               When
 ──────                ────
 Router                runner registration failure
 InvalidRunnerConfig   backend config validation failure
 Io                    OS-level I/O error
```

## Feature flags

| Flag         | What it enables                                        |
|--------------|--------------------------------------------------------|
| `subprocess` | subprocess runner and its operating-system integration |
| `seccomp`    | Linux seccomp blocklist                                |

## Notes
- `SubprocessRunner` implements `Runner` trait from `solti-runner`.
- Mode resolution: `Command` → direct exec; `Script` → decode the body at build time, write a fresh 0600 tempfile for each attempt, and exec the interpreter with its path. The tempfile is unlinked after the attempt.
- Script body is capped at `solti_model::MAX_SCRIPT_BODY_BYTES` (2 MiB, decoded) by the model; the tempfile transport avoids Linux's per-arg `MAX_ARG_STRLEN` (128 KiB) limit that `-c <inline>` would hit.
- Cancel, dropped futures, and normal completion stop the full process group on Unix.
- The parent environment is cleared by default. `EnvPolicy::Inherit` and `EnvPolicy::Allowlist` are explicit choices.
- Cgroup lifecycle is two-phase: `prepare` (mkdir + write limits in parent) → `attach` (join PID in child via pre_exec).
- Cgroups are created under the process's current delegated cgroup unless `with_cgroup_parent` sets another parent.
- Cgroup names are auto-generated per attempt: `{runner}-{slot}-{seq:x}-{timestamp:x}-{attempt:x}`.
- Line truncation uses `Cow::Borrowed` for the common case (zero-alloc hot path).
- `log_stream` is double-headed: every line goes to `tracing` and, when enabled, to the attempt's `solti_runner::OutputSink`. `BuildContext` exposes only an `OutputPublisher`; its default disables live output, while `solti-core` injects the standard non-blocking publisher.
- `LinuxCapability` values match `<linux/capability.h>` from Linux 6.x.
- On non-Linux platforms, Linux-specific sandbox controls are unavailable; generic subprocess execution remains supported.
