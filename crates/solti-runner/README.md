# solti-runner
Runner plugin interface, routing, and execution metrics for the solti task system.
Sits between the domain model (`solti-model`) and the orchestration layer (`solti-core`), providing a stable plugin boundary.

## Quick start
Implement `Runner`, register it in a `RunnerRouter`, and let the router build tasks:

```rust,no_run
use std::sync::Arc;

use solti_model::{TaskKind, TaskSpec};
use solti_runner::{BuildContext, Runner, RunnerError, RunnerRouter};
use taskvisor::TaskRef;

struct MyRunner;

impl Runner for MyRunner {
    fn name(&self) -> &'static str {
        "my-runner"
    }

    fn supports(&self, spec: &TaskSpec) -> bool {
        matches!(spec.kind(), TaskKind::Subprocess(_))
    }

    fn build_task(&self, _spec: &TaskSpec, _ctx: &BuildContext) -> Result<TaskRef, RunnerError> {
        // build and return a TaskRef
        todo!()
    }
}

fn route(spec: &TaskSpec) -> Result<TaskRef, RunnerError> {
    let mut router = RunnerRouter::new();
    router.register(Arc::new(MyRunner));
    router.build(spec) // picks the first runner that supports `spec`
}
```

## Architecture
```text
  TaskSpec ──► RunnerRouter ──► Runner::build_task() ──► TaskRef
                   │                    ▲
                   │  label matching    │  BuildContext (env + metrics + output_registry)
                   │  + supports()      │  
                   ▼                    │
               RunnerEntry          MetricsHandle / OutputRegistry
               (runner + labels)    (Arc-shared with the supervisor)
```

`BuildContext` carries an `Arc<OutputRegistry>` alongside metrics. 
Runners that produce per-line output (e.g. subprocess) ask the registry for an `OutputSink` per attempt and push lines into it; subscribers (HTTP SSE, gRPC stream) read them out the other side.

```text
  per-task broadcast channel (lag-skip, capacity = N)
         ▲                                      │
   sink_for(task_id, attempt)             subscribe(task_id)
         │                                      ▼
  Runner produces lines              SSE / gRPC handler reads
  (per attempt: monotonic seq)       (sees Chunk + RunStarted / Finished across all attempts of one task)
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

| Type              | Purpose                                                                                              |
|-------------------|------------------------------------------------------------------------------------------------------|
| `Runner`          | Trait: `name()`, `supports()`, `build_task()`, `build_run_id()`                                      |
| `RunnerRouter`    | Selects runner by supports() + label matching                                                        |
| `BuildContext`    | Shared dependencies: `RunnerEnv` + `MetricsHandle` + `Arc<OutputRegistry>`                           |
| `OutputSink`      | Per-attempt writer (`stdout_line` / `stderr_line`); thin wrapper over `broadcast::Sender`            |
| `OutputRegistry`  | One broadcast channel per `TaskId`, reused across attempts; `ensure_channel` / `subscribe` / `evict` |
| `RunId`           | Human-readable id: `{runner}-{slot}-{seq}`                                                           |
| `RunnerError`     | Error enum: `NoRunner`, `UnsupportedKind`, `InvalidSpec`, `Internal`, `MissingField`, `Io`           |
| `MetricsBackend`  | Trait: `record_task_started`, `record_task_completed`, `record_runner_error`                         |
| `MetricsHandle`   | `Arc<dyn MetricsBackend>` - cloneable shared handle                                                  |
| `NoOpMetrics`     | Zero-size backend (`#[inline(always)]`)                                                              |
| `RunnerType`      | Metric label: `Subprocess`, `Container`, `Wasm`                                                      |
| `MetricOutcome`   | Metric label: `Success`, `Failure`, `Canceled`, `Timeout`                                            |
| `RunnerErrorKind` | Metric label: `CgroupPrepareFailed`, `BackendConfigFailed`, `SpawnFailed`, `ModuleLoadFailed`        |

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
      fn record_task_completed(&self, runner_type: RunnerType, outcome: MetricOutcome, duration_ms: u64);
      fn record_runner_error(&self, runner_type: RunnerType, error_kind: RunnerErrorKind);
  }
```

Default backend: `NoOpMetrics` (zero-size, `#[inline(always)]` - compiles to nothing).

Production backend: `solti-prometheus::PrometheusMetrics`.

## Notes
- Runners are checked in registration order; the first match wins.
- `RunId` sequence is process-global, monotonically increasing, starts at 1.
- `TaskKind::Embedded` is not routable: use `SupervisorApi::submit_with_task` directly.
- `BuildContext` defaults: empty `RunnerEnv` + `NoOpMetrics` + an empty `OutputRegistry` (no live subscribers).
- `OutputRegistry` channels are `tokio::sync::broadcast`: slow subscribers don't block the runner; they receive a `Lagged` signal and continue from the freshest event in the ring window.
