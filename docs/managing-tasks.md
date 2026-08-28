---
title: Managing tasks
description: Create, apply, observe, cancel, and delete desired task resources through the core API.
---

# Managing tasks

Use `SupervisorApi` to manage desired resources. A successful create or apply returns the committed resource, not an execution result.

## Participants

| Participant | Role |
| --- | --- |
| Application | Supplies complete desired manifests and, for Embedded work, the executable `TaskRef`. |
| `solti-model` | Provides `TaskManifest`, `Task`, queries, and `WritePreconditions`. |
| `solti-core` | Serializes operations by resource name, commits state, owns reconciliation, and exposes observation and cleanup APIs. |
| `solti-runner` and Taskvisor | Construct and execute the committed resource asynchronously. They are not part of the create/apply return guarantee. |

## Choose the write path

Start core with `SupervisorApi::builder(router).start().await`. Register the runners required by routed workloads before startup. Embedded tasks can use an empty router.

| API | Input | Existing name |
| --- | --- | --- |
| `create_task` | Routed manifest | `AlreadyExists`. |
| `apply_task` | Routed manifest | Compare and update desired state. Missing names are created. |
| `create_embedded_task` | Embedded manifest and `TaskRef` | `AlreadyExists`. |
| `apply_embedded_task` | Embedded manifest and `TaskRef` | Compare and update desired state. Missing names are created. |

Routed APIs reject an Embedded manifest. Embedded APIs reject a routed workload. Validation happens before desired-state commit.

After a write commits, core owns its reconciliation work. Dropping the caller's future does not undo the commit or cancel reconciliation already scheduled by that write. Read the resource again when a caller loses its response; do not infer whether a commit happened from the caller's timeout alone.

## Apply is comparison, not an unconditional restart

| Retained resource and supplied manifest | Result |
| --- | --- |
| Name is missing | Create, unless non-empty preconditions require an existing resource. |
| Manifest is identical and `Reconciled` is not `False` | No-op. No new generation or runtime submission. |
| Only labels or annotations differ | Metadata update. Preserve generation and execution state. |
| Spec differs | Advance generation, reset the current execution projection to pending, and schedule reconciliation. |
| Manifest is identical and `Reconciled=False` | Retry reconciliation at the same generation. |

The same-generation retry is specific to reconciliation failure. Applying an unchanged manifest to a task whose accepted runtime has finished or failed does not restart it merely because its phase is terminal.

A metadata-only change to a resource with `Reconciled=False` is still a metadata-only update. The exact-manifest retry branch requires no metadata or spec change.

For Embedded work, change `EmbeddedSpec.revision` when changing the implementation. Core compares the manifest, not pointer identity. Passing a different `TaskRef` with an unchanged healthy manifest does not request replacement.

An apply can replace an earlier pending reconciliation. It does not guarantee that every intermediate generation runs. See [reconciliation](reconciliation.md).

## Protect a read-modify-write

`WritePreconditions` can require the expected UID, resource version, or both. `WritePreconditions::from_task(&task)` captures both from a stored resource.

- A UID guard prevents acting on a replacement that reused the same name.
- A resource-version guard prevents acting on a changed resource. Status changes count as resource changes as well as desired-state changes.
- When both guards are present, both must match.

Use `apply_task_with_preconditions`, `apply_embedded_task_with_preconditions`, `cancel_task_with_preconditions`, or `delete_task_with_preconditions` at the corresponding write boundary.

A mismatch returns `CoreError::Conflict` with typed `WritePreconditionViolation` entries. Missing guarded resources return `NotFound`. An apply with non-empty guards never turns a missing resource into a create.

The `_where` APIs combine adapter visibility with the operation. A hidden resource is reported as missing. For apply, cancel, and delete, the visibility check and operation share the same per-name lock. These predicates must be pure and non-blocking and must not call back into `SupervisorApi`.

## Observe the boundary you need

| Need | API and boundary |
| --- | --- |
| Current retained resource | `get_task`: a point-in-time copy, or `None`. |
| Filtered resource collection | `query_tasks`: a snapshot page with a continuation. |
| Resource transitions | `watch_tasks`: initial state or exact-version replay followed by live changes. |
| Attempt records | `query_task_runs`: bounded, retained history for a task incarnation. |
| Live bytes and output markers | `subscribe_output`: live-only output, not history. |

Open a task watch before a write when an example needs to observe the write's transitions. Check the task identity and expected generation, not only a phase that could belong to another generation. Watches and queries can report expired history; [collections and watches](collections-and-watches.md) explains resynchronization.

`Reconciled=True` confirms Taskvisor command intake. `Running` confirms an observed attempt start. Neither proves useful application work completed. A terminal phase can be an intermediate attempt result under a restart policy.

## Cancel or delete

`cancel_task` stops current reconciliation or requests cancellation of the current accepted runtime. Desired state and run history remain retained. Unknown names return `NotFound`.

`delete_task` first settles the current logical runtime outcome, then removes the resource and retained run history. Unguarded deletion of a missing name is an idempotent no-op. Guarded and visibility-filtered delete instead report a missing resource as `NotFound`.

Both operations become SDK-owned after their worker is registered. A caller abandoning the wait does not cancel that registered operation. Cancellation does not disable future reconciliation. See [cancellation and shutdown](cancellation-and-shutdown.md) for ordering, deadlines, and physical-exit limits.

## Write failures and retained-state limits

Synchronous write errors include invalid manifests, a wrong Embedded/routed API, `AlreadyExists`, failed guards, retained-task count or manifest-byte limits, and `ShuttingDown`.

`StateConfig` bounds retained resources independently from runtime concurrency. A full task-count limit rejects a new name without evicting another task. An update to an existing name does not consume another task-count slot, but positive manifest-byte growth still has to fit its byte budget.

With a state persistence sink installed, state writes can also wait for bounded persistence admission before commit. This is backpressure, not successful execution or a slot-policy decision.

Failures after commit are observed through reconciliation conditions and runtime phases. A runner build failure does not roll back the accepted manifest.

## Examples and source

- [Embedded lifecycle example](../crates/solti-core/examples/embedded_lifecycle.rs): create, observe, replace a generation, read history, and shut down.
- [Routed output example](../crates/solti-core/examples/routed_output.rs): register a custom runner and follow a committed resource into execution.
- [Public core operations](../crates/solti-core/src/supervisor/mod.rs), [write preconditions](../crates/solti-model/src/resource/preconditions.rs), and [typed core errors](../crates/solti-core/src/error.rs).

## See also

- [Task resources](task-resources.md) for identity and status fields.
- [Building an agent](building-an-agent.md) for assembling the process.
- [Persistence](persistence.md) for write admission and callback delivery.
