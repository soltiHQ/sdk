# solti-model

> Shared task model for Solti agents and control planes.

`solti-model` contains the data types that all Solti crates speak.
It defines task specs, task status, ids, policies, runner selectors, environment variables, output events, and agent tokens.

Use it when you build a Solti API, runner, supervisor, control plane, or tool that reads or writes task data.

## The shape everyone shares

Without one model crate, each layer has to invent its own task shape:

```rust,ignore
struct ApiTaskSpec { /* ... */ }
struct RunnerTaskSpec { /* ... */ }
struct ControlPlaneTaskSpec { /* ... */ }
```

With `solti-model`, all layers use the same resource:

```text
Task
  metadata: ObjectMeta
  spec:     TaskSpec
  status:   TaskStatus
```

`TaskSpec` says what should run. `TaskStatus` says what happened. `ObjectMeta` carries identity, version, and timestamps.

## Quick Start

Build a task spec and validate it at the submit boundary:

```rust
use solti_model::{
    Flag, SubprocessMode, SubprocessSpec, TaskEnv, TaskKind, TaskSpec,
};

let kind = TaskKind::Subprocess(SubprocessSpec::new(
    SubprocessMode::Command {
        command: "echo".into(),
        args: vec!["hello".into()],
    },
    TaskEnv::default(),
    None,
    Flag::enabled(),
));

let spec = TaskSpec::builder("hello", kind, 5_000u64)
    .build()
    .expect("valid spec");

spec.validate().expect("submittable spec");
assert_eq!(spec.slot().as_str(), "hello");
```

Create a task resource from that spec:

```rust
use solti_model::{Task, TaskId, TaskKind, TaskPhase, TaskSpec};

let spec = TaskSpec::builder("cleanup", TaskKind::Embedded, 1_000u64)
    .build()
    .unwrap();

let task = Task::new(TaskId::from("embedded-cleanup-1"), spec);

assert_eq!(*task.phase(), TaskPhase::Pending);
assert_eq!(task.id().as_str(), "embedded-cleanup-1");
```

`TaskKind::Embedded` is valid as model data, but `TaskSpec::validate()` rejects it for runner-based submit. Embedded tasks must be submitted with a real `TaskRef`.

## What Ships

| Area        | Main Types                                                                             |
|-------------|----------------------------------------------------------------------------------------|
| Resource    | `Task`, `TaskSpec`, `TaskStatus`, `ObjectMeta`, `TaskRun`                              |
| Identity    | `Slot`, `TaskId`, `AgentId`                                                            |
| Execution   | `TaskKind`, `SubprocessSpec`, `SubprocessMode`, `WasmSpec`, `ContainerSpec`, `Runtime` |
| Policies    | `RestartPolicy`, `BackoffPolicy`, `JitterPolicy`, `AdmissionPolicy`, `Timeout`         |
| Routing     | `Labels`, `RunnerSelector`, `SelectorRequirement`, `SelectorOperator`                  |
| Environment | `TaskEnv`, `RunnerEnv`, `KeyValue`, `merge_env`                                        |
| Query       | `TaskQuery`, `TaskPage`                                                                |
| Output      | `OutputEvent`, `OutputChunk`, `StreamKind`                                             |
| Auth        | `Token`                                                                                |
| Errors      | `ModelError`, `ModelResult`                                                            |

## Core Model

```text
TaskSpec
  slot
  kind
  timeout
  restart
  backoff
  admission
  max_retries
  runner_selector
  labels

TaskStatus
  phase
  attempt
  exit_code
  error

ObjectMeta
  id
  resource_version
  created_at
  updated_at
```

Most fields are private on `TaskSpec`. Build specs with `TaskSpec::builder()` or parse them with serde.
Deserialization also validates the shape.

## Task Lifecycle

```text
Pending -> Running -> Succeeded
              |
              +-> Failed -> maybe restart
              +-> Timeout
              +-> Canceled
              +-> Exhausted
```

Terminal phases are `Succeeded`, `Failed`, `Timeout`, `Canceled`, and `Exhausted`.

`Task` keeps terminal updates stable. A late actor event should not turn a canceled run into an exhausted run. The only allowed refinement is `Failed` into a more specific terminal phase such as `Timeout` or `Exhausted`.

## Task Kinds

| Kind         | Meaning                | Routed by runner  |
|--------------|------------------------|-------------------|
| `Subprocess` | Host command or script | yes               |
| `Container`  | OCI image              | yes               |
| `Wasm`       | WASI module            | yes               |
| `Embedded`   | In-process task        | no                |

Subprocess command example:

```rust
use solti_model::{Flag, SubprocessMode, SubprocessSpec, TaskEnv, TaskKind};

let kind = TaskKind::Subprocess(SubprocessSpec::new(
    SubprocessMode::Command {
        command: "date".into(),
        args: vec![],
    },
    TaskEnv::default(),
    None,
    Flag::enabled(),
));

assert_eq!(kind.kind(), "subprocess");
```

Subprocess scripts store their body as standard base64. The decoded UTF-8 body is capped by `MAX_SCRIPT_BODY_BYTES`.

## Policies

`RestartPolicy` controls when the supervisor runs another attempt.

```rust
use solti_model::RestartPolicy;

let once = RestartPolicy::Never;
let retry_on_error = RestartPolicy::OnFailure;
let hourly = RestartPolicy::periodic(60 * 60 * 1000);

let _ = (once, retry_on_error, hourly);
```

`BackoffPolicy` controls delay between failure retries:

```rust
use solti_model::{BackoffPolicy, JitterPolicy};

let backoff = BackoffPolicy {
    jitter: JitterPolicy::Equal,
    first_ms: 1_000,
    max_ms: 30_000,
    factor: 2.0,
};

backoff.validate().unwrap();
```

`AdmissionPolicy` controls duplicate submissions into the same slot: drop, replace, or queue.

## Runner Selectors

Runner selectors match runner labels. All requirements are ANDed.

```rust
use solti_model::{Labels, RunnerSelector, SelectorRequirement};

let selector = RunnerSelector {
    match_labels: {
        let mut labels = Labels::new();
        labels.insert("zone", "eu");
        labels
    },
    match_expressions: vec![SelectorRequirement::exists("gpu")],
};

let mut runner = Labels::new();
runner.insert("zone", "eu");
runner.insert("gpu", "h100");

assert!(selector.matches(&runner));
```

## Environment

`TaskEnv` comes from the task. `RunnerEnv` comes from the runner. When they are merged, runner values win:

```rust
use solti_model::{RunnerEnv, TaskEnv, merge_env};

let mut task = TaskEnv::new();
task.push("PATH", "/user/bin");
task.push("APP_MODE", "batch");

let mut runner = RunnerEnv::new();
runner.push("PATH", "/safe/bin");

let env = merge_env(&task, &runner);
assert_eq!(env.get("PATH").map(String::as_str), Some("/safe/bin"));
assert_eq!(env.get("APP_MODE").map(String::as_str), Some("batch"));
```

## Identity Rules

`Slot`, `TaskId`, and `AgentId` are cheap `Arc<str>` wrappers.
They allow only `[A-Za-z0-9._-]`, reject empty strings, reject `"."` and `".."`, and have length limits:

| Type      | Limit     |
|-----------|-----------|
| `Slot`    | 64 bytes  |
| `AgentId` | 128 bytes |
| `TaskId`  | 256 bytes |

These values can reach cgroup names, temp paths, logs, and wire protocols, so the model keeps them boring on purpose.

## Authentication

`Token` is the shared bearer secret between an agent and the control plane.
The agent can present it to the control plane, and the agent API can verify inbound calls with the same value.

```rust
use solti_model::Token;

let token = Token::new("secret");
assert!(token.verify("secret"));
assert!(!token.verify("other"));
assert_eq!(format!("{token:?}"), "Token(***redacted***)");
```

`Token::generate()` creates a fresh random token and does not persist it. The agent binary decides where to store it: file, secret manager, Kubernetes secret, or another store.

## Output Events

Live task output uses `OutputEvent`.
The HTTP SSE shape is this crate's serde JSON shape. gRPC uses protobuf through `solti-api`.

```rust
use bytes::Bytes;
use solti_model::{OutputChunk, OutputEvent, StreamKind};
use std::time::SystemTime;

let event = OutputEvent::Chunk(OutputChunk {
    attempt: 1,
    stream: StreamKind::Stdout,
    seq: 0,
    ts: SystemTime::UNIX_EPOCH,
    line: Bytes::from_static(b"hello"),
});

let json = serde_json::to_string(&event).unwrap();
assert!(json.contains(r#""type":"chunk""#));
```

## Notes

- Most public enums are `#[non_exhaustive]`; match them with a fallback arm.
- `Labels` uses `BTreeMap`, so iteration order is stable.
- `TaskEnv` and `RunnerEnv` preserve insertion order and use last-value-wins lookup.
- `BackoffPolicy` validates on construction through serde and through `TaskSpecBuilder::build`.
- Pagination uses `DEFAULT_LIMIT = 100` and `MAX_LIMIT = 1000`.
