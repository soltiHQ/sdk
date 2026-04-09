# solti-model
Domain model for the solti task execution system.

Defines core resource types: `Task`, `TaskSpec`, `TaskStatus`, `ObjectMeta`, and all supporting domain primitives (phases, policies, selectors, identity newtypes).

## Architecture
```text
 ┌──────────────────────────────────────────────────────────┐
 │                      Task                                │
 │                                                          │
 │  ObjectMeta            TaskSpec            TaskStatus    │
 │  ├─ id: TaskId         ├─ slot: Slot       ├─ phase      │
 │  ├─ generation         ├─ kind: TaskKind   ├─ attempt    │
 │  ├─ resource_version   ├─ timeout          ├─ exit_code  │
 │  ├─ created_at         ├─ restart          └─ error      │
 │  └─ updated_at         ├─ backoff                        │
 │                        ├─ admission                      │
 │                        ├─ runner_selector                │
 │                        └─ labels                         │
 └──────────────────────────────────────────────────────────┘
```

## Resource model

| Section      | Type         | Responsibility                                                |
|--------------|--------------|---------------------------------------------------------------|
| **metadata** | `ObjectMeta` | Identity, versioning (`generation` + `resource_version`)      |
| **spec**     | `TaskSpec`   | Desired state (private fields; build via `TaskSpec::builder`) |
| **status**   | `TaskStatus` | Observed state: phase, attempt count, exit code, error        |

## Task lifecycle
```text
 Pending ──► Running ──► Succeeded
               │
               ├──► Failed ──► (restart) ──► Running
               ├──► Timeout
               ├──► Canceled
               └──► Exhausted (max retries reached)
```

Terminal phases: `Succeeded`, `Failed`, `Timeout`, `Canceled`, `Exhausted`.

## Task kinds

| Variant      | Backend                        | Routable |
|--------------|--------------------------------|----------|
| `Subprocess` | OS process (`command`, `args`) | yes      |
| `Container`  | OCI container image            | yes      |
| `Wasm`       | WASI module (`.wasm`)          | yes      |
| `Embedded`   | In-process `TaskRef`           | no       |

`Embedded` tasks are submitted directly via `SupervisorApi::submit_with_task`.
Routable variants go through `RunnerRouter::pick()`.

## Policies

| Type              | Controls                                                 |
|-------------------|----------------------------------------------------------|
| `RestartPolicy`   | When to restart: `Never`, `OnFailure`, `Always`          |
| `BackoffPolicy`   | Delay between retries: initial, max, factor, jitter      |
| `JitterPolicy`    | Jitter strategy: `None`, `Full`, `Equal`, `Decorrelated` |
| `AdmissionPolicy` | Duplicate handling: `DropIfRunning`, `Replace`, `Queue`  |

## Runner selector
```text
 TaskSpec.runner_selector
 ┌──────────────────────────────────────────────────────┐
 │  match_labels:      { "zone": "eu" }                 │
 │  match_expressions: [ {key:"gpu", op:Exists} ]       │
 └──────────────────────────┬───────────────────────────┘
                            │  ALL requirements ANDed
                            ▼
 RunnerRouter::pick()
 ┌──────────────────────────────────────────────────────┐
 │  Runner A  labels: {"zone":"us","gpu":"a100"}  ✗     │
 │  Runner B  labels: {"zone":"eu","gpu":"h100"}  ✓     │
 │  Runner C  labels: {"zone":"eu"}               ✗     │
 └──────────────────────────────────────────────────────┘
```

Operators: `In`, `NotIn`, `Exists`, `DoesNotExist`.

## Key types

| Type               | Description                                               |
|--------------------|-----------------------------------------------------------|
| `Task`             | K8s-style aggregate: metadata + spec + status             |
| `TaskSpec`         | Desired state (private fields, build via builder)         |
| `TaskSpecBuilder`  | Validated builder for `TaskSpec`                          |
| `TaskStatus`       | Observed state: phase, attempt, exit code, error          |
| `ObjectMeta`       | Identity, versioning, timestamps                          |
| `TaskRun`          | Per-attempt execution record with start/finish times      |
| `TaskPhase`        | Lifecycle phase enum (7 variants)                         |
| `TaskKind`         | Execution backend: Subprocess, Wasm, Container, Embedded  |
| `Slot`             | Logical execution lane (newtype over `Arc<str>`)          |
| `TaskId`           | Unique task identifier (newtype over `Arc<str>`)          |
| `AgentId`          | Agent identifier (newtype over `Arc<str>`)                |
| `Timeout`          | Per-attempt timeout in milliseconds                       |
| `Labels`           | Key-value metadata for routing and filtering              |
| `TaskEnv`          | Ordered environment variables for task execution          |
| `RunnerEnv`        | Ordered environment variables for runner injection        |
| `Flag`             | Boolean toggle with `enabled()`/`disabled()` constructors |
| `RunnerSelector`   | Label selector for runner routing                         |
| `TaskQuery`        | Builder for filtered, paginated task listing              |
| `TaskPage`         | Paginated query result                                    |

## Versioning

`ObjectMeta` tracks two counters inspired by K8s:

| Counter            | Bumped on            | Purpose                           |
|--------------------|----------------------|-----------------------------------|
| `generation`       | spec mutations       | User-driven change detection      |
| `resource_version` | any change           | Optimistic concurrency control    |

## Construction
```text
let spec = TaskSpec::builder("my-slot", kind, 5_000u64)
    .restart(RestartPolicy::OnFailure)
    .backoff(BackoffPolicy { jitter: JitterPolicy::Equal, first_ms: 1_000, max_ms: 30_000, factor: 2.0 })
    .build()?;

spec.validate()?;  // submit-boundary validation (rejects Embedded)
```

## Error model
```text
 Variant             When
 ───────             ────
 Conflict            resource_version mismatch (optimistic concurrency)
 UnknownAdmission    unknown admission policy string
 UnknownRestart      unknown restart policy string
 UnknownJitter       unknown jitter policy string
 UnknownTaskKind     unknown task kind string
 Invalid             structural validation failure (empty slot, bad backoff, etc.)
```

## Notes
- `TaskSpec` fields are private use `TaskSpec::builder()` for construction and `serde` for deserialization.
- Deserialization goes through `#[serde(try_from = "TaskSpecRaw")]` which validates on parse.
- Identity newtypes (`Slot`, `TaskId`, `AgentId`) wrap `Arc<str>` for cheap cloning and comparison.
- `BackoffPolicy` implements `Eq`/`Hash` via `f64::to_bits()` for the `factor` field.
- `TaskPhase`, `RestartPolicy`, `AdmissionPolicy`, `JitterPolicy` all implement `FromStr` for CLI/config parsing.
- `Labels` is backed by `BTreeMap<String, String>` for deterministic iteration order.
- All types derive `Serialize`/`Deserialize` with `camelCase` field renaming.
