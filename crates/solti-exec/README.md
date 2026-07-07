# solti-exec
Task execution backends for the solti task system.

Provides concrete `Runner` implementations that turn `TaskSpec` into running OS processes.

It currently ships a single backend - `SubprocessRunner` with optional Linux sandboxing (rlimits, cgroup v2, capabilities).

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
 TaskSpec { kind: Subprocess(..) }
     │
     ▼  RunnerRouter::pick()
 SubprocessRunner
     │
     ├──► build_task_config(spec, ctx)
     │     ├──► resolve SubprocessMode → (command, args [, script_tempfile])
     │     │        (Script mode: body → NamedTempFile 0600 → path as argv[0])
     │     ├──► merge_env(task_env, runner_env)
     │     └──► SubprocessTaskConfig { run_id, command, args, env, cwd }
     │
     ├──► prepare backend (cgroup dirs, if configured)
     │
     └──► run_subprocess(ctx, cancel)
           ├──► build Command + apply pre_exec hooks
           │     (process_group(0), kill_on_drop(true))
           ├──► spawn + pipe stdout/stderr
           ├──► select! biased { child.wait(), cancel → killpg }
           ├──► record metrics
           └──► cleanup cgroup (if any)
```

## Subprocess lifecycle
```text
 build_task ──► prepare_backend ──► spawn ──► log_stream (stdout/stderr)
                                   (pgid,                  ├──► tracing::info/warn
                                    kill_on_drop)          └──► OutputSink (if registry wired)
                                      │
                                      ├──► child.wait() → evaluate exit
                                      ├──► cancel.cancelled() → killpg -SIGKILL
                                      │                       → wait() to reap
                                      ▼
                                  metrics + cleanup
```

`biased` select prefers `child.wait()` over `cancel.cancelled()` — a process that has already exited cleanly is never misreported as cancelled, even if the cancel token fired in the same microsecond.

## Key types

| Type                      | Description                                                 |
|---------------------------|-------------------------------------------------------------|
| `SubprocessRunner`        | `Runner` impl for `TaskKind::Subprocess`                    |
| `SubprocessBackendConfig` | Builder for rlimits + cgroups + security + logger settings  |
| `SubprocessTaskConfig`    | Fully resolved per-task config (command, args, env, cwd)    |
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

All settings are optional — without a backend config the subprocess inherits parent process settings.

By default, confinement is **best-effort**: if the sandbox cannot be applied (non-Linux host, missing `CAP_SETPCAP`, cgroup join failure) the child runs *unconfined* with a warning. Call `.with_require_enforcement(true)` to fail **closed** instead — a non-Linux host is rejected at config time, and on Linux a cgroup-join or capability-drop failure aborts the spawn. `keep_caps` is only meaningful with `drop_all_caps = true` (validated at config time).

## Sandboxing (pre_exec hooks)
```text
 fork()
 ┌───────────────────────────────────────────────────────────────────┐
 │  child process (before execve)                                    │
 │                                                                   │
 │  1. rlimits: getrlimit → clamp → setrlimit                        │
 │  2. cgroup:  open /sys/fs/cgroup/{name}/cgroup.procs → write PID  │
 │  3. security: capget → mask → capset → no_new_privs               │
 │                                                                   │
 │  execve(command, args)                                            │
 └───────────────────────────────────────────────────────────────────┘
```

All pre_exec hooks are **async-signal-safe**: zero heap allocation, only raw libc syscalls, `Copy`-only captures.

## Registration
```text
 register_subprocess_runner(&mut router, "default")
     ├──► SubprocessRunner::new("default")
     ├──► label "runner-name" = "default"
     └──► router.register_with_labels()

 register_subprocess_runner_with_backend(&mut router, "secure", backend)
     ├──► validate backend config
     ├──► SubprocessRunner::with_config("secure", backend)
     ├──► label "runner-name" = "secure"
     └──► router.register_with_labels()
```

Duplicate names are rejected via `router.contains_label()` → `ExecError::DuplicateRunner`.

## Error model
```text
 Variant               When
 ──────                ────
 DuplicateRunner       runner with this name already registered
 InvalidRunnerConfig   backend config validation failure
 InvalidSpec           task spec validation failure (empty command, etc.)
 Internal              unexpected internal error
 Io                    OS-level I/O error
```

## Feature flags

| Flag         | What it enables                                        |
|--------------|--------------------------------------------------------|
| `subprocess` | `subprocess` module, `libc`, `base64`, `tokio/process` |

## Notes
- `SubprocessRunner` implements `Runner` trait from `solti-runner`.
- Mode resolution: `Command` → direct exec; `Script` → decode base64 body → write to a `NamedTempFile` (mode 0600) → exec interpreter with the path. The tempfile is kept alive for the task's lifetime via `Arc` and unlinked on drop.
- Script body is capped at `solti_model::MAX_SCRIPT_BODY_BYTES` (2 MiB, decoded) by the model; the tempfile transport avoids Linux's per-arg `MAX_ARG_STRLEN` (128 KiB) limit that `-c <inline>` would hit.
- Cancel uses **process-group kill** on Unix: `Command::process_group(0)` sets pgid = child pid, then cancel sends `SIGKILL` to `-pgid` so forked helpers (`sleep 1000 &`) die together with the parent. `kill_on_drop(true)` covers the drop-without-wait path.
- Environment merge: runner env overrides task env (last-writer-wins via `BTreeMap`). Parent env is currently inherited — no automatic `env_clear()`.
- Cgroup lifecycle is two-phase: `prepare` (mkdir + write limits in parent) → `attach` (join PID in child via pre_exec).
- Cgroup names are auto-generated: `{runner}-{slot}-{seq:x}-{timestamp:x}`.
- Line truncation uses `Cow::Borrowed` for the common case (zero-alloc hot path).
- `log_stream` is double-headed: every line goes to `tracing` (existing path) and, if the supervisor wired an `OutputRegistry` into `BuildContext`, also to a `solti_runner::OutputSink` (live-tail subscribers). When no registry is attached, the sink push is a no-op cost (one Arc clone, one `broadcast::send` that returns `Err` and is ignored).
- `LinuxCapability` values match `<linux/capability.h>` from Linux 6.x.
- On non-Linux platforms, all sandboxing is no-op with `tracing::warn`.
