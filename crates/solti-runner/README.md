# solti-runner
Runner plugin interface, routing, and execution metrics for the solti task system.
Sits between the domain model (`solti-model`) and the orchestration layer (`solti-core`), providing a stable plugin boundary.

## Architecture
```text
  TaskSpec ──► RunnerRouter ──► Runner::build_task() ──► TaskRef
                   │                    ▲
                   │  label matching    │  BuildContext
                   │  + supports()      │  (env + metrics)
                   ▼                    │
               RunnerEntry          MetricsHandle
               (runner + labels)    (Arc<dyn MetricsBackend>)
```

## Routing flow
```text
  RunnerRouter::build(spec)
    │
    ├─ 1. reject TaskKind::Embedded          (requires submit_with_task)
    │
    ├─ 2. for each registered runner:
    │      ├─ runner.supports(spec)?         kind check
    │      └─ selector.matches(labels)?      label matching (if selector set)
    │
    ├─ 3. first match → runner.build_task(spec, ctx)
    │
    └─ 4. no match → RunnerError::NoRunner
```

## Key types

| Type              | Purpose                                                                                       |
|-------------------|-----------------------------------------------------------------------------------------------|
| `Runner`          | Trait: `name()`, `supports()`, `build_task()`, `build_run_id()`                               |
| `RunnerRouter`    | Selects runner by supports() + label matching                                                 |
| `BuildContext`    | Shared dependencies: `RunnerEnv` + `MetricsHandle`                                            |
| `RunId`           | Human-readable id: `{runner}-{slot}-{seq}`                                                    |
| `RunnerError`     | Error enum: `NoRunner`, `UnsupportedKind`, `InvalidSpec`, `Internal`, `MissingField`, `Io`    |
| `MetricsBackend`  | Trait: `record_task_started`, `record_task_completed`, `record_runner_error`                  |
| `MetricsHandle`   | `Arc<dyn MetricsBackend>` - cloneable shared handle                                           |
| `NoOpMetrics`     | Zero-size backend (`#[inline(always)]`)                                                       |
| `RunnerType`      | Metric label: `Subprocess`, `Wasm`, `Container`                                               |
| `TaskOutcome`     | Metric label: `Success`, `Failure`, `Canceled`, `Timeout`                                     |
| `RunnerErrorKind` | Metric label: `CgroupPrepareFailed`, `BackendConfigFailed`, `SpawnFailed`, `ModuleLoadFailed` |

## Runner trait
```text
  trait Runner: Send + Sync {
      fn name(&self) -> &'static str;
      fn supports(&self, spec: &TaskSpec) -> bool;
      fn build_task(&self, spec: &TaskSpec, ctx: &BuildContext) -> Result<TaskRef, RunnerError>;
      fn build_run_id(&self, slot: &str) -> RunId;   // default: make_run_id(name, slot)
  }
```

## Error model
```text
  Variant            When                                          
  ───────            ────                                          
  NoRunner           no registered runner matches the spec         
  UnsupportedKind    runner does not handle this TaskKind          
  InvalidSpec        spec is malformed for this runner             
  MissingField       required field missing from spec              
  Internal           unexpected runner error                       
  Io                 I/O error during task setup (From<io::Error>) 
```

## Metrics interface
```text
  trait MetricsBackend: Send + Sync + 'static {
      fn record_task_started(&self, runner_type: RunnerType);
      fn record_task_completed(&self, runner_type: RunnerType, outcome: TaskOutcome, duration_ms: u64);
      fn record_runner_error(&self, runner_type: RunnerType, error_kind: RunnerErrorKind);
  }
```

Default backend: `NoOpMetrics` (zero-size, `#[inline(always)]` - compiles to nothing).

Production backend: `solti-prometheus::PrometheusMetrics`.

## Notes
- Runners are checked in registration order; the first match wins.
- `TaskKind::Embedded` is not routable: use `SupervisorApi::submit_with_task` directly.
- `RunId` sequence is process-global, monotonically increasing, starts at 1.
- `BuildContext` defaults: empty `RunnerEnv` + `NoOpMetrics`.
