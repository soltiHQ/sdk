---
title: Task resources
description: Separate desired state, resource identity, reconciliation status, and execution attempts.
---

# Task resources

A task resource describes what should run and records what the supervisor has observed. It is not the executable task object.

## Participants

| Participant | Role |
| --- | --- |
| `solti-model` | Defines and validates manifests, stored resources, workload schemas, policies, queries, and attempt records. It does not execute work. |
| `solti-core` | Assigns resource identity and versions, stores desired state, reconciles it, and projects runtime results. |
| `solti-runner` | Reads workload identity and runner selectors to construct an executable task. |
| Taskvisor | Owns the executable task and its attempts. Its runtime ID is separate from the SDK resource identity. |

## Manifest and stored resource

`TaskManifest` is the write contract. It contains the Task type metadata, a name, labels, annotations, and `TaskSpec`. The built-in Task resource type is `solti.io/v1`, kind `Task`.

`Task` is the stored read contract. It adds a UID, resource version, generation, creation timestamp, and status. Use `TaskManifest::from(&task)` when deriving another desired-state write from a stored resource. This conversion does not turn server-owned fields into writable fields.

The core API accepts a complete manifest. `apply_task` compares that manifest with retained desired state; it is not a field-level patch or a multi-writer field-ownership system.

## Identity at each layer

| Value | Meaning |
| --- | --- |
| Task name, `TaskId` | Key used by core create, apply, get, cancel, and delete operations. |
| `metadata.uid` | Identity of one resource incarnation. Deleting and recreating the same name produces a new UID. |
| `metadata.generation` | Desired-spec revision. It starts at one and advances when the spec changes. |
| `metadata.resourceVersion` | Opaque version assigned by the state store to a resource change. Metadata and status changes can advance it without changing generation. |
| `status.observedGeneration` | Generation for which core recorded reconciliation acceptance or failure. It can lag the current desired generation. |
| `status.attempt` | Attempt number within the current generation. Zero means that no attempt has been recorded as started. |
| `spec.slot` | Taskvisor admission key. Different task names can compete for the same slot. |
| Taskvisor task ID and runner `RunId` | Runtime registration identities. They are not resource UIDs or `TaskRun` records. |

Treat resource versions as opaque strings. They are not portable numeric counters or durable cross-restart checkpoints. Task collection versions and run-history collection versions belong to separate state-store histories.

An attempt record is identified in context by task UID, generation, and attempt. The task name alone cannot distinguish records from a deleted resource and its replacement.

## Workload and execution policy

`TaskSpec` groups the slot, workload, positive per-attempt timeout in milliseconds, restart policy, backoff, admission policy, optional retry limit, and optional runner selector.

Each workload has its own `apiVersion`, `kind`, and `spec` envelope. This is distinct from the outer Task resource type.

| Workload | Construction path |
| --- | --- |
| `Subprocess`, `Container`, `Wasm` | A registered runner must match the workload's exact group/version and kind. A model variant alone does not install a backend. |
| `Extension` | Application-owned JSON object and workload identity, implemented by a matching runner. |
| `Embedded` | Caller supplies a Taskvisor `TaskRef` through an embedded core API. It is not routed. |

`EmbeddedSpec.revision` is a caller-owned implementation revision. It participates in spec equality. The manifest does not contain or serialize the `TaskRef`.

Task labels describe the resource and can be used by collection filters. `runnerSelector` instead matches the labels captured when runners are registered. Changing resource labels does not select another runner. Changing `runnerSelector` changes the spec and therefore the generation.

See [routing and custom runners](routing-and-custom-runners.md) for registration and selection, and [lifecycle and admission](lifecycle-and-admission.md) for execution policies.

## Read reconciliation and execution separately

Every valid `TaskStatus` has exactly one `Reconciled` condition.

| Condition | Meaning in the core process |
| --- | --- |
| `Unknown` | Reconciliation is pending, including a build or Taskvisor intake wait. |
| `True` | The desired generation crossed Taskvisor command intake. It may still be queued, rejected later, or not yet running. |
| `False` | Reconciliation failed before accepted execution, or user cancellation stopped pre-intake reconciliation. Inspect the condition's reason and message. |

The condition carries its own `observedGeneration`. While reconciliation is pending, this can identify the new generation while `status.observedGeneration` still records an older one. Once the condition is `True` or `False`, those observed-generation values agree.

`phase` is the execution projection: `Pending`, `Running`, `Succeeded`, `Failed`, `Timeout`, `Canceled`, or `Exhausted`. A pending resource has attempt zero and no execution error or exit code. A running resource has a positive attempt and no terminal diagnostics. Non-pending phases require `Reconciled=True`.

A terminal phase does not always mean that the managed task has stopped. An attempt can fail before a retry, or succeed before an `Always` restart. `TaskPhase::is_terminal()` classifies a phase; it does not prove physical task exit or an idle admission slot. See [lifecycle and admission](lifecycle-and-admission.md).

`status.error` and `exitCode` are optional details. Core selects phases from typed runtime results, not by parsing diagnostic strings.

## What a desired-state change preserves

| Change | Generation | Execution projection |
| --- | --- | --- |
| Identical manifest | Unchanged | Unchanged unless an identical apply retries `Reconciled=False`. |
| Labels or annotations only | Unchanged | Preserved; no runtime rebuild is scheduled. |
| Any spec change | Incremented | Reset to pending for the new generation; prior observed generation is retained until reconciliation advances. |

Old runtime observations are tied to their exact resource identity and generation. They must not overwrite the execution projection of a newer desired generation. Retained history can still describe attempts from older generations.

## Validation boundaries

Constructors and deserialization validate the model. Built-in workload specs reject unknown fields. Extension specs preserve application-owned JSON object fields while enforcing the model's structural bounds.

`MAX_TASK_MANIFEST_BYTES` limits compact JSON for caller-owned manifest fields to 4 MiB. The bound is not a transport response limit. Server-owned status and retained run history have separate budgets. Lifecycle diagnostics are bounded by `MAX_TASK_DIAGNOSTIC_BYTES`, a 32 KiB UTF-8-safe prefix.

Some derivation methods, such as `TaskSpec::with_workload`, deliberately do not validate immediately. Resource constructors and core writes validate the completed resource. A valid model is still not a promise that a matching runner, executable, image, or runtime capacity exists.

## Examples and source

- [Manifest construction example](../crates/solti-model/examples/task_manifest.rs): build and serialize desired state.
- [Model lifecycle example](../crates/solti-model/examples/task_lifecycle.rs): follow metadata, generation, and status transitions without executing a task.
- [Task resource implementation](../crates/solti-model/src/resource/task.rs), [status invariants](../crates/solti-model/src/resource/status.rs), and [workload contracts](../crates/solti-model/src/domain/kind/task.rs).

## See also

- [Managing tasks](managing-tasks.md) for write and guard behavior.
- [Reconciliation](reconciliation.md) for the desired-state-to-runtime flow.
- [Output and history](output-and-history.md) for attempt records and output identity.
