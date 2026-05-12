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
 │  ├─ resource_version   ├─ kind: TaskKind   ├─ attempt    │
 │  ├─ created_at         ├─ timeout          ├─ exit_code  │
 │  └─ updated_at         ├─ restart          └─ error      │
 │                        ├─ backoff                        │
 │                        ├─ admission                      │
 │                        ├─ runner_selector                │
 │                        └─ labels                         │
 └──────────────────────────────────────────────────────────┘
```

## Resource model

| Section      | Type         | Responsibility                                                |
|--------------|--------------|---------------------------------------------------------------|
| **metadata** | `ObjectMeta` | Identity, `resource_version`, timestamps                      |
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

`Subprocess` has two execution strategies (see `SubprocessMode`):
- **Command**: `execve(command, args)` directly.
- **Script**: interpreter + script body. The body is **base64-encoded, UTF-8**, capped at `MAX_SCRIPT_BODY_BYTES` (2 MiB after decode). Interpreters: `Bash`, `Python`, `Node`, `Custom { command, flag }`.

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

| Type               | Description                                                    |
|--------------------|----------------------------------------------------------------|
| `Task`             | K8s-style aggregate: metadata + spec + status                  |
| `TaskSpec`         | Desired state (private fields, build via builder)              |
| `TaskSpecBuilder`  | Validated builder for `TaskSpec`                               |
| `TaskStatus`       | Observed state: phase, attempt, exit code, error               |
| `ObjectMeta`       | Identity, versioning, timestamps                               |
| `TaskRun`          | Per-attempt execution record with start/finish times           |
| `TaskPhase`        | Lifecycle phase enum (7 variants)                              |
| `TaskKind`         | Execution backend: Subprocess, Wasm, Container, Embedded       |
| `Slot`             | Logical execution lane (newtype over `Arc<str>`)               |
| `TaskId`           | Unique task identifier (newtype over `Arc<str>`)               |
| `AgentId`          | Agent identifier (newtype over `Arc<str>`)                     |
| `Timeout`          | Per-attempt timeout in milliseconds                            |
| `Labels`           | Key-value metadata for routing and filtering                   |
| `TaskEnv`          | Ordered environment variables for task execution               |
| `RunnerEnv`        | Ordered environment variables for runner injection             |
| `Flag`             | Boolean toggle with `enabled()`/`disabled()` constructors      |
| `RunnerSelector`   | Label selector for runner routing                              |
| `TaskQuery`        | Builder for filtered, paginated task listing                   |
| `TaskPage`         | Paginated query result                                         |
| `OutputChunk`      | One stdout/stderr line from a task attempt                     |
| `OutputEvent`      | Tagged enum: `Chunk` / `RunStarted` / `RunFinished` / `Lagged` |
| `StreamKind`       | `Stdout` or `Stderr` discriminator carried by `OutputChunk`    |

## Size limits

Exposed as `pub const` so downstream layers (API, CP, UI) share one source of truth.

| Constant                | Value   | Enforced by                         |
|-------------------------|---------|-------------------------------------|
| `MAX_SCRIPT_BODY_BYTES` | 2 MiB   | `SubprocessMode::Script::validate`  |
| `SLOT_MAX_LEN`          | 64      | `Slot::validate_format`             |
| `TASK_ID_MAX_LEN`       | 256     | `TaskId::validate_format`           |
| `AGENT_ID_MAX_LEN`      | 128     | `AgentId::validate_format`          |

## Identity rules

`Slot`, `TaskId`, `AgentId` allow `[A-Za-z0-9._-]` only, reject `.` and `..`.
No whitespace, no path separators, no non-ASCII: these values reach cgroup paths, tempfile names, `execve` argv, and log fields, where anything else misbehaves. 
Validation runs at `TaskSpec::validate` / submit time.

## Versioning

`ObjectMeta.resource_version` is a monotonic counter bumped on every change
(spec or status) for optimistic concurrency control.

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
 UnknownAdmission    unknown admission policy string
 UnknownRestart      unknown restart policy string
 UnknownJitter       unknown jitter policy string
 UnknownTaskPhase    unknown task phase string
 Invalid             structural validation failure (empty slot, bad backoff, etc.)
```

## Notes
- `TaskSpec` fields are private — use `TaskSpec::builder()` for construction and `serde` for deserialization.
- Deserialization goes through `#[serde(try_from = "TaskSpecRaw")]` which validates on parse.
- `BackoffPolicy` is **also** validated on deserialize via its own `try_from` raw; zero `first_ms`, inverted `max_ms`, or non-finite/`<1.0` `factor` are rejected at parse time.
- Identity newtypes (`Slot`, `TaskId`, `AgentId`) wrap `Arc<str>` via `arc_str_newtype!`; environment newtypes (`TaskEnv`, `RunnerEnv`) wrap `Vec<KeyValue>` via `env_newtype!`. Both macros keep parallel types in lockstep.
- `BackoffPolicy` implements `Eq`/`Hash` via `f64::to_bits()` for the `factor` field.
- `TaskPhase`, `RestartPolicy`, `AdmissionPolicy`, `JitterPolicy` all implement `FromStr` for CLI/config parsing.
- `Labels` is backed by `BTreeMap<String, String>` for deterministic iteration order.
- Most types derive `Serialize`/`Deserialize` with `camelCase` field renaming. The one exception is `SelectorOperator`, which serializes as PascalCase (`In`, `NotIn`, `Exists`, `DoesNotExist`) to match the Kubernetes `LabelSelectorOperator` convention.
- `TaskKind`, `TaskPhase`, `RestartPolicy`, `AdmissionPolicy`, `JitterPolicy`, `SelectorOperator` are `#[non_exhaustive]` — adding new variants is a non-breaking change.
- Pagination constants for list endpoints: `DEFAULT_LIMIT = 100`, `MAX_LIMIT = 1000` (re-exported as `pub const` so downstream API layers share one source of truth).
