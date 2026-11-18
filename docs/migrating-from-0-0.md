---
title: Migrate from 0.0 to 0.1
description: Replace the typed built-in workloads of 0.0.x with declared workload types, and update persisted data, runners and the task proto.
---

# Migrate from 0.0 to 0.1

0.1.0 moves the SDK from typed built-in workloads to workload types that each runner declares.
A workload is one routed shape, `{apiVersion, kind, spec}`.
A runner declares each type it runs with a GVK, a JSON Schema of the `spec`, and a check of the body; the agent advertises the schemas' fingerprints and sends the schemas to the control plane on request.
There is no compatibility layer: the 0.0.x names are gone, and a 0.0.x value that uses them no longer compiles or deserializes.

Read [declare workload types](routing-and-custom-runners.md#declare-workload-types) first; this page only maps the old names to the new ones.

## Removed items

| 0.0.x | 0.1.0 |
|---|---|
| `TaskWorkload::Subprocess(spec)` | `TaskWorkload::from_body(&spec)`, a `TaskWorkload::Routed` under `exec.solti.io/v1` `Subprocess` |
| `TaskWorkload::Container(spec)` | `TaskWorkload::from_body(&spec)`, a `TaskWorkload::Routed` under `exec.solti.io/v1` `Container` |
| `TaskWorkload::Extension(workload)` | `TaskWorkload::Routed(workload)`, or `TaskWorkload::from(workload)` |
| `TaskWorkload::Wasm(spec)`, `WasmSpec` | Removed with no replacement: no runner ever executed it. An application that embeds a WASM engine declares its own type, for example `wasm.example.io/v1` `Module` |
| `ExtensionWorkload` | `Workload`. `ExtensionWorkload::new` is `Workload::new`, and `spec()` is `body()` |
| `WORKLOAD_API_VERSION` (`solti.io/v1`) | `solti_exec::EXEC_API_VERSION` (`exec.solti.io/v1`), with the `subprocess` or `container` feature |
| `solti_model::SubprocessSpec`, `SubprocessMode` | `solti_exec::subprocess::{SubprocessSpec, SubprocessMode}` |
| `solti_model::ContainerSpec` | `solti_exec::container::ContainerSpec` |
| `ChainSpec::into_workload`, `TryFrom<ChainSpec>` for `TaskWorkload` and `ExtensionWorkload` | `TaskWorkload::from_body(&chain)` or `Workload::from_body(&chain)` |
| `ChainSpec::from_workload`, `TryFrom<&TaskWorkload>` and `TryFrom<&ExtensionWorkload>` for `ChainSpec` | `workload.decode::<ChainSpec>()` on the routed `Workload` |
| The `chain-schema` facade feature and `solti-chain`'s `schema` feature | Nothing to enable: a chain publishes a schema, so `solti-chain` always derives it. `chain` is enough |
| `Runner::workload_types() -> Vec<WorkloadTypeMeta>` | `Runner::workload_types() -> Vec<WorkloadDecl>`; see below |
| `RunnerCapability::new(name, labels, Vec<WorkloadTypeMeta>)` | `RunnerCapability::new(name, labels, Vec<WorkloadType>)`: a GVK with its schema fingerprint. `RunnerRouter::capabilities()` builds it for you |
| `Deserialize` and `JsonSchema` on `RunnerCapability` and `AgentCapabilities` | Removed: the agent builds capabilities and nothing reads them back |

`TaskWorkload::from_body` takes a body type that implements `WorkloadBody`, and does not run its `validate`; admission does.
`SubprocessSpec`, `ContainerSpec` and `ChainSpec` do.

```rust
use solti_chain::ChainSpec;
use solti_model::{ModelResult, TaskWorkload, Workload};

fn chain_workload(chain: &ChainSpec) -> ModelResult<TaskWorkload> {
    TaskWorkload::from_body(chain)
}

fn chain_spec(workload: &Workload) -> Option<ChainSpec> {
    workload.decode::<ChainSpec>().ok()
}
```

## Runners

A runner used to list the GVKs it handled.
It now declares each type with a schema and a check:

```rust
use solti_model::WorkloadDecl;

fn workload_types() -> Vec<WorkloadDecl> {
    vec![WorkloadDecl::object("tasks.example.io/v1", "Echo")]
}
```

`WorkloadDecl::object` accepts any JSON object, which is what a bare GVK did.
Use `WorkloadDecl::of::<B>()` to derive a schema and a check from a body type, or `WorkloadDecl::new` for a hand-written schema.
Registration now refuses a declaration whose schema Podium would refuse, with `RouterError::InvalidWorkloadType`.
The check runs under core's per-task lock and an admission permit, so keep it fast and non-blocking.

Routing is unchanged: the first runner, in registration order, whose exact GVK and labels match.
Admission is new: core runs that runner's check before a write commits.

## The workload GVK and the reserved group

The built-in workloads moved from `solti.io/v1` to `exec.solti.io/v1`:

| 0.0.x | 0.1.0 |
|---|---|
| `solti.io/v1` `Subprocess` | `exec.solti.io/v1` `Subprocess` |
| `solti.io/v1` `Container` | `exec.solti.io/v1` `Container` |
| `solti.io/v1` `Wasm` | none |
| `chain.solti.io/v1alpha1` `Chain` | unchanged |

The group `solti.io` is now reserved: only `solti.io/v1` `Embedded` uses it, and `Embedded` is never routed, never declared by a runner and never on the wire.
`Workload::new`, a declaration or a Task API request in that group is refused.
The `Subprocess` and `Container` spec fields are unchanged; only the `apiVersion` differs.

The Task resource itself stays `solti.io/v1` `Task`, and the HTTP API root stays `/apis/solti.io/v1`.

## The task proto and the gRPC clients

The Task API contract is 0.2.
Its `TaskWorkload` message keeps one member, `extension`, whose `spec.raw` carries the UTF-8 JSON body.
The typed messages are removed: `SubprocessTask`, `WasmTask`, `ContainerTask`, `CommandMode`, `ScriptMode`, `SubprocessMode`, `ContainerCommand` and `KeyValue`, with the `subprocess`, `wasm` and `container` fields of `TaskWorkload`.
A request that still sets one is refused with `InvalidArgument` naming `extension.spec.raw`.
Generated clients must be regenerated from the 0.2 proto, and send the body of `exec.solti.io/v1` `Subprocess` as JSON in `extension.spec.raw`.

## Persisted data

A 0.0.x JSON document that holds a typed workload no longer deserializes.
`TaskWorkload` reads `{"apiVersion": "solti.io/v1", "kind": "Subprocess", ...}` as a routed workload in the reserved group and refuses it.
Rewrite the stored `apiVersion` before loading:

```text
"apiVersion": "solti.io/v1", "kind": "Subprocess"   ->   "apiVersion": "exec.solti.io/v1", "kind": "Subprocess"
"apiVersion": "solti.io/v1", "kind": "Container"    ->   "apiVersion": "exec.solti.io/v1", "kind": "Container"
```

Embedded workloads and chains need no change.
A stored `Wasm` workload cannot be migrated; delete it or give it a type of your own.
Core persists nothing by itself, so only data that your persistence sinks wrote is affected.

## Discovery and Podium

Every heartbeat now carries a schema fingerprint for each declared type, and follow-up requests carry the schemas Podium asks for; see [exchange workload schemas](discovery.md#exchange-workload-schemas).
This needs a Podium that supports the schema exchange.
Podium validates a spec's body against the schema before it deploys it and again before every push.

## Build and dependency notes

- Install snippets read `0.1`.
- A direct `solti-model` dependency keeps its `schema` feature by default. Through the facade, `WorkloadDecl::of` needs `model-schema`; the `task_custom_workload` example needs `core,model-schema`.
- `solti-model`'s canonical-JSON crates enable `serde_json/float_roundtrip`, and Cargo feature unification applies it to every `serde_json` user in the build: floating-point parsing is correctly rounded and slightly slower.
- The old `custom_extension` example is now `custom_workload`.
