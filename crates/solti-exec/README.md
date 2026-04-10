# solti-exec
Task execution backends for the solti task system.

Provides concrete `Runner` implementations that turn `TaskSpec` into running OS processes.

Currently, ships a single backend - `SubprocessRunner` with optional Linux sandboxing (rlimits, cgroup v2, capabilities).

## Architecture
```text
 TaskSpec { kind: Subprocess(..) }
     │
     ▼  RunnerRouter::pick()
 SubprocessRunner
     │
     ├──► build_task_config(spec, ctx)
     │     ├──► resolve SubprocessMode → (command, args)
     │     ├──► merge_env(task_env, runner_env)
     │     └──► SubprocessTaskConfig { run_id, command, args, env, cwd }
     │
     ├──► prepare backend (cgroup dirs, if configured)
     │
     └──► run_subprocess(ctx, cancel)
           ├──► build Command + apply pre_exec hooks
           ├──► spawn + pipe stdout/stderr
           ├──► select! { child.wait(), cancel }
           ├──► record metrics
           └──► cleanup cgroup (if any)
```

## Subprocess lifecycle
```text
 build_task ──► prepare_backend ──► spawn ──► log_stream (stdout/stderr)
                                      │
                                      ├──► child.wait() → evaluate exit
                                      └──► cancel.cancelled() → kill
                                      │
                                      ▼
                                  metrics + cleanup
```

## Key types

| Type                      | Description                                                 |
|---------------------------|-------------------------------------------------------------|
| `SubprocessRunner`        | `Runner` impl for `TaskKind::Subprocess`                    |
| `SubprocessBackendConfig` | Builder for rlimits + cgroups + security + logger settings  |
| `SubprocessTaskConfig`    | Fully resolved per-task config (command, args, env, cwd)    |
| `LogConfig`               | Stdout/stderr logging: truncation length, log levels        |
| `RlimitConfig`            | POSIX rlimits (nofile, fsize, core, nproc, as)              |
| `CgroupLimits`            | cgroup v2: CPU quota/period, memory, PIDs                   |
| `CpuMax`                  | CPU quota + period for `cpu.max`                            |
| `SecurityConfig`          | Capability drop + `no_new_privs`                            |
| `LinuxCapability`         | Capability enum with kernel `cap_value` constants           |
| `ExecError`               | Configuration and spawn-time errors                         |

## Backend config
```text
 SubprocessBackendConfig::new()
     .with_rlimits(RlimitConfig { max_open_files: Some(1024), .. })
     .with_cgroups(CgroupLimits { cpu: Some(CpuMax { quota: Some(50_000), period: 100_000 }), memory: Some(128 MB), pids: Some(32) })
     .with_security(SecurityConfig { keep_capabilities: vec![NetBindService], no_new_privs: true })
     .with_logger(LogConfig { max_line_length: 4096, stdout_info: true, stderr_warn: true })
```

All settings are optional — without a backend config the subprocess inherits parent process settings.

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
- Mode resolution: `Command` → direct exec; `Script` → decode base64 body → `runtime.resolve()` → exec.
- Environment merge: runner env overrides task env (last-writer-wins via `BTreeMap`).
- Cgroup lifecycle is two-phase: `prepare` (mkdir + write limits in parent) → `attach` (join PID in child via pre_exec).
- Cgroup names are auto-generated: `{runner}-{slot}-{seq:x}-{timestamp:x}`.
- Line truncation uses `Cow::Borrowed` for the common case (zero-alloc hot path).
- `LinuxCapability` values match `<linux/capability.h>` from Linux 6.x.
- On non-Linux platforms, all sandboxing is no-op with `tracing::warn`.
