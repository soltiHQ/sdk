# solti-model

`solti-model` is the shared data contract for Solti agents, APIs, runners, and control planes.

It defines validated task resources, workload envelopes, policies, selectors, capabilities, queries, output events, identities, and bearer tokens.
The crate does not store, route, or execute tasks.
It has no optional features.

## Quick start

Build user-owned desired state:

```rust
use solti_model::{
    Flag, Labels, SubprocessMode, SubprocessSpec, TaskEnv, TaskManifest,
    Task, TaskSpec, TaskWorkload,
};

let workload = TaskWorkload::Subprocess(SubprocessSpec::new(
    SubprocessMode::Command {
        command: "echo".into(),
        args: vec!["hello".into()],
    },
    TaskEnv::default(),
    None,
    Flag::enabled(),
));

let spec = TaskSpec::builder("jobs", workload, 5_000_u64)
    .build()
    .expect("valid task spec");

let mut labels = Labels::new();
labels.insert("app.kubernetes.io/name", "hello");

let manifest = TaskManifest::new("hello-1", spec)
    .unwrap()
    .with_labels(labels)
    .unwrap();

assert_eq!(manifest.name().as_str(), "hello-1");
assert_eq!(manifest.slot().as_str(), "jobs");
let task = Task::from_manifest(manifest).unwrap();

assert_eq!(task.metadata().generation(), 1);
assert_eq!(task.status().observed_generation(), 0);
assert!(task.metadata().resource_version().is_empty());
```

`TaskManifest` contains only caller-owned desired state.
The state store assigns `resourceVersion`.
The model generates `uid`, `creationTimestamp`, and initial status.

## What it does

- defines the Kubernetes-shaped `Task` resource;
- separates caller-owned manifests from stored resources;
- validates constructors and deserialized values;
- supports built-in and application-defined workload GVKs;
- defines apply and status-transition invariants;
- provides selectors, queries, pagination cursors, and runner capabilities;
- provides the JSON contract for live output events;
- provides shared identifiers and bearer-token helpers.

## Inputs and outputs

| API                              | Input                                      | Output                                  |
|----------------------------------|--------------------------------------------|-----------------------------------------|
| `TaskSpec::builder`              | Slot, workload, timeout, optional policies | Validated `TaskSpec`                    |
| `TaskManifest::new`              | Resource name and `TaskSpec`               | Caller-owned CRD manifest               |
| `Task::from_manifest`            | `TaskManifest`                             | Stored `Task` with server-owned fields  |
| `Task::apply_desired`            | Labels, annotations, spec, store version   | `DesiredChange`                         |
| `TaskRun::starting`              | Generation, attempt, workload GVK          | Active run record                       |
| `TaskFilter` / `TaskQuery`       | Slot, phases, selector, pagination         | Validated collection query              |
| `RunnerCapability::new`          | Runner name, labels, workload GVKs         | Canonical capability entry              |
| `AgentCapabilities::new`         | Runner entries                             | Capability snapshot                     |
| `Token::new` / `Token::generate` | Raw token or OS entropy                    | Validated bearer token                  |
| Serde deserialization            | JSON model representation                  | Validated value or deserializer error    |

## Resource boundary

```text
caller
  │
  ▼
TaskManifest
  ├── apiVersion + kind
  ├── metadata.name
  ├── metadata.labels
  ├── metadata.annotations
  └── spec
        │
        ▼
state store materialization
        │
        ▼
Task
  ├── manifest fields
  ├── metadata.uid
  ├── metadata.resourceVersion
  ├── metadata.generation
  ├── metadata.creationTimestamp
  └── status
```

| Field                                  | Owner       |
|----------------------------------------|-------------|
| `metadata.name`                        | Caller      |
| `metadata.labels` / `annotations`      | Caller      |
| `spec`                                 | Caller      |
| `metadata.uid`                         | State store |
| `metadata.resourceVersion`             | State store |
| `metadata.generation`                  | Model/store |
| `metadata.creationTimestamp`           | Model/store |
| `status`                               | Controller  |

`TaskManifest` rejects `status` and server-owned metadata.
Stored `Task` values require both server metadata and status.
Both use `apiVersion: solti.io/v1` and `kind: Task`.

## Task specification

`TaskSpec::builder` requires three values:

| Field      | Meaning                              |
|------------|--------------------------------------|
| `slot`     | Admission and concurrency lane       |
| `workload` | Executable desired state             |
| `timeout`  | Positive attempt timeout in ms       |

The builder applies these defaults:

| Field            | Default                                         |
|------------------|-------------------------------------------------|
| `restart`        | `RestartPolicy::Never`                          |
| `backoff`        | Full jitter, 1 s to 30 s, factor 2             |
| `admission`      | `AdmissionPolicy::DropIfRunning`                |
| `maxRetries`     | Absent, which means unlimited failure retries   |
| `runnerSelector` | Absent                                          |

`maxRetries: 0` is invalid.
Omit the field for an unlimited retry budget.

## Workload variants

| Variant       | Input                                             | Routed by a runner |
|---------------|---------------------------------------------------|--------------------|
| `Subprocess`  | Command or explicit interpreter and base64 script | Yes                |
| `Container`   | OCI image, optional command, args, environment    | Yes                |
| `Wasm`        | WASM module path, args, environment               | Yes                |
| `Embedded`    | In-process implementation revision                | No                 |
| `Extension`   | Application GVK and JSON object spec              | Yes                |

Built-in workloads use `apiVersion: solti.io/v1`.
Their envelope and spec reject unknown fields.

An extension workload keeps its application-owned fields:

```rust
use solti_model::{ExtensionWorkload, TaskWorkload};

let workload = TaskWorkload::Extension(
    ExtensionWorkload::new(
        "media.example.io/v1",
        "ImageResize",
        serde_json::json!({
            "width": 1280,
            "format": "webp"
        }),
    )
    .unwrap(),
);

assert_eq!(workload.api_version(), "media.example.io/v1");
assert_eq!(workload.kind(), "ImageResize");
```

Extension `spec` must be a JSON object.
The `solti.io` API group is reserved for built-in workloads.
`solti-runner` owns routability and runner-specific validation.

Subprocess script bodies use standard base64.
Decoded content must be UTF-8 and cannot exceed `MAX_SCRIPT_BODY_BYTES` (`2 MiB`).

## Apply behavior

`Task::apply_desired` classifies changes:

| Result                    | Generation | Status                | Resource version |
|---------------------------|------------|-----------------------|------------------|
| `DesiredChange::None`     | Unchanged  | Unchanged             | Unchanged        |
| `DesiredChange::Metadata` | Unchanged  | Unchanged             | Replaced         |
| `DesiredChange::Spec`     | Incremented| Reset for new desired state | Replaced    |

UID and creation time remain unchanged.
Invalid desired state is rejected before mutation.

Status updates carry an authoritative generation and attempt.
Stale generations are ignored.
Terminal attempt phases are sticky.
A generic `Failed` phase can be refined to `Timeout` or `Exhausted`.

## Validation and serde

Validated top-level constructors and deserializers call the same validation methods.
Direct construction and deserialization of collection values are not validated automatically.
Call `validate` for `Labels`, `Annotations`, `LabelSelector`, and `SelectorRequirement`.

- `TaskId` uses Kubernetes DNS-1123 subdomain rules and a 253-byte limit.
- `Slot` allows `[A-Za-z0-9._-]` and has a 64-byte limit.
- `AgentId` allows `[A-Za-z0-9._-]` and has a 128-byte limit.
- label keys and values use Kubernetes validation rules;
- annotations use qualified keys and allow arbitrary values;
- annotation key and value bytes are capped at 256 KiB in total;
- workload GVKs use CRD-compatible `apiVersion` and `kind` validation;
- resource, spec, `TaskFilter`, `TaskContinuation`, capability, and built-in workload shapes reject unknown fields;
- `creationTimestamp` uses RFC 3339 with millisecond precision.

## Selectors and capabilities

`LabelSelector` uses Kubernetes selector syntax:

```rust
use solti_model::{LabelSelector, Labels};

let selector: LabelSelector =
    "environment=production,tier in (frontend,backend),!tainted"
        .parse()
        .unwrap();

let mut labels = Labels::new();
labels.insert("environment", "production");
labels.insert("tier", "frontend");

assert!(selector.matches(&labels));
```

Requirements are ANDed.
`In` requires the key to exist.
`NotIn` and `!=` also match a missing key.

`RunnerCapability` contains one runner name, static labels, and workload GVKs.
Construction rejects empty declarations, duplicate GVKs, and the built-in `Embedded` GVK.
Workload GVKs are stored in canonical order.
`AgentCapabilities` preserves runner registration order and rejects duplicate runner names.

## Queries

`TaskFilter` contains slot, phase, and label filters.
Phases use OR semantics.
Slot, phase, and label filters are ANDed.

`TaskQuery` adds list pagination:

- default limit: `100`;
- maximum limit: `1000`;
- zero selects the default;
- larger values are capped;
- `TaskContinuation` carries snapshot `resourceVersion`, the fixed filter, and the last task name.

The continuation is a domain value.
Transport layers encode it into their own opaque wire token.

## Output events

`OutputEvent` is the HTTP/SSE JSON contract:

```text
{"type":"chunk","generation":2,"attempt":1,"stream":"stdout","seq":0,"ts":1700,"line":"aGk="}
{"type":"runStarted","generation":2,"attempt":1,"startedAt":1700}
{"type":"runFinished","generation":2,"attempt":1,"exitCode":0,"finishedAt":1701}
{"type":"lagged","skipped":42}
```

Output timestamps are Unix milliseconds.
Chunk bytes are standard base64 and can contain non-UTF-8 data.
gRPC uses the protobuf contract from `solti-api`; its shape is intentionally separate.

## Authentication

`Token` supports validated raw values, OS-generated values, environment variables, and files.

```rust
use solti_model::Token;

let token = Token::new("secret").unwrap();

assert!(token.verify("secret"));
assert!(!token.verify("other"));
assert_eq!(format!("{token:?}"), "Token(***redacted***)");
```

Generated tokens use 256 bits of OS entropy and the `solti_agt_` prefix.
Generation does not persist the value.
`verify` is constant-time for equal-length strings.
`Debug` never exposes the token.

## Errors

Constructors and validation return `ModelResult<T>`.
`ModelError::Invalid` covers structural and invariant failures.
Dedicated variants cover unknown admission, restart, jitter, and task-phase names.

`ModelError` and most public enums are non-exhaustive.
Match them with a fallback arm.
