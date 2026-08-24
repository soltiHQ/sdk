# Task API Contract

The current Task API identity is `v1`.

This document describes the public Task API v1 wire contract.
It also describes the behavior implemented by `SupervisorApiAdapter`.

Task API v1 has two transport bindings:

| Transport | Protocol identity                              |
|-----------|------------------------------------------------|
| HTTP      | API root `/apis/solti.io/v1`                   |
| gRPC      | Package `solti.task.v1`, service `TaskService` |

The transports expose the same operations.
They do not use the same wire representation.

| Contract                   | Source                                             |
|----------------------------|----------------------------------------------------|
| HTTP shapes and operations | OpenAPI produced from the mounted `HttpApi` routes |
| gRPC shapes and operations | `proto/solti/task/v1/api.proto` and `types.proto`  |
| Behavior                   | This document                                      |

HTTP uses the CRD JSON representation owned by `solti-model`.
gRPC uses versioned protobuf messages.
`HttpApi::build` generates a standalone OpenAPI document.
`HttpApi::mount` adds the same routes to an application-owned document.
The application finalizes the combined router once.

## Version selection

One `solti-api` build exposes one Task API major version.
The crate exports its identity through `API_VERSION`, `HTTP_API_ROOT`, `GRPC_API_PACKAGE`, and `GRPC_API_SERVICE`.

The agent passes `API_VERSION` to `AgentEndpoint`.
Discovery advertises that number through `SyncRequest.api_version`.

The control plane selects a Task API adapter by `endpoint_type` and `api_version`.
Each supported major has its own HTTP routes, gRPC package, and wire models.
One agent server does not host multiple Task API majors.

## Boundary

`solti-api` owns public transport behavior.
It does not store or execute tasks.

The HTTP task subtree owns its handler state, access-control hooks, limits, metrics, and fallbacks.
Application routes keep their own state and perimeter.

```text
HTTP CRD JSON ── parse and validate ──┐
                                      ▼
                                 ApiHandler
                                      ├── custom backend
                                      └── SupervisorApiAdapter ──► solti-core
                                      ▲
gRPC v1 DTO ── convert and validate ──┘
```

Every operation delegates to one `ApiHandler` method.

| Operation | HTTP                                      | gRPC             |
|-----------|-------------------------------------------|------------------|
| Create    | `POST /apis/solti.io/v1/tasks`            | `CreateTask`     |
| Apply     | `PUT /apis/solti.io/v1/tasks/{name}`      | `ApplyTask`      |
| Get       | `GET /apis/solti.io/v1/tasks/{name}`      | `GetTask`        |
| List      | `GET /apis/solti.io/v1/tasks`             | `ListTasks`      |
| Watch     | `GET /apis/solti.io/v1/tasks?watch=true`  | `WatchTasks`     |
| Runs      | `GET /apis/solti.io/v1/tasks/{name}/runs` | `ListTaskRuns`   |
| Cancel    | `POST /apis/solti.io/v1/tasks/{name}/cancel` | `CancelTask`  |
| Delete    | `DELETE /apis/solti.io/v1/tasks/{name}`   | `DeleteTask`     |
| Logs      | `GET /apis/solti.io/v1/tasks/{name}/logs?taskUid={uid}` | `StreamTaskLogs` |

## Public workloads

The public API accepts these built-in workloads:

- `solti.io/v1`, kind `Subprocess`;
- `solti.io/v1`, kind `Wasm`;
- `solti.io/v1`, kind `Container`.

It also accepts application-provided workload GVKs.
Their API group must not be `solti.io`.
Their `spec` must be a JSON object.

Built-in workload specs reject unknown fields.
Extension workload fields are owned by the application.

`chain.solti.io/v1alpha1`, kind `Chain`, uses this extension boundary.
HTTP places the Chain object directly in `Task.spec.workload.spec`.
gRPC places the UTF-8 JSON bytes of that same Chain spec in `ExtensionTask.spec.raw`.

The transports validate the extension GVK and require an object payload.
The Chain runner validates its entry, steps, transitions, reachability, and acyclicity when core reconciles the Task.
Chain remains one outer Task, existing Task status, run history, and output messages require no extra fields.

The built-in `solti.io/v1` `Embedded` workload is SDK-only.
HTTP and gRPC reject it as input.
`SupervisorApiAdapter` also hides it from reads, watches, history, cancellation, deletion, and output.

## Resource shapes

Create and apply accept a `TaskManifest`.
It contains caller-owned desired state.

```json
{
  "apiVersion": "solti.io/v1",
  "kind": "Task",
  "metadata": {
    "name": "daily-report",
    "labels": {
      "app": "reports"
    }
  },
  "spec": {
    "slot": "reports",
    "workload": {
      "apiVersion": "solti.io/v1",
      "kind": "Subprocess",
      "spec": {
        "mode": {
          "command": {
            "command": "report-generator",
            "args": ["--daily"]
          }
        },
        "failOnNonZero": true
      }
    },
    "timeout": 30000,
    "restart": {
      "type": "onFailure"
    },
    "backoff": {
      "jitter": "full",
      "firstMs": 1000,
      "maxMs": 30000,
      "factor": 2.0
    },
    "admission": "dropIfRunning",
    "maxRetries": 3
  }
}
```

A stored `Task` adds server-owned metadata and observed status.

| Field                        | Owner  | Meaning                                     |
|------------------------------|--------|---------------------------------------------|
| `metadata.name`              | Caller | Stable resource address                     |
| `metadata.labels`            | Caller | Selector metadata                           |
| `metadata.annotations`       | Caller | Free-form metadata                          |
| `metadata.uid`               | Server | Resource-incarnation identity               |
| `metadata.resourceVersion`   | Server | Opaque store version                        |
| `metadata.generation`        | Server | Desired-spec generation                     |
| `metadata.creationTimestamp` | Server | RFC 3339 creation time                      |
| `spec`                       | Caller | Desired execution state                     |
| `status`                     | Server | Observed reconciliation and execution state |

`metadata.name` uses the Kubernetes DNS-1123 subdomain format.
It is immutable because it is the resource address.

`metadata.uid` changes after deletion and recreation.
Clients must treat `metadata.resourceVersion` as opaque.

## Create

HTTP:

```text
POST /apis/solti.io/v1/tasks
Content-Type: application/json

TaskManifest
```

A successful HTTP create returns `201 Created` and the committed `Task`.

gRPC:

```text
/solti.task.v1.TaskService/CreateTask

CreateTaskRequest { manifest }
    ──► CreateTaskResponse { task }
```

Create requires a new retained name.
An existing name returns HTTP `409` or gRPC `AlreadyExists`.
When either retained Task admission budget would be exceeded, a new name
returns HTTP `429` or gRPC `ResourceExhausted`.

With `SupervisorApiAdapter`, success means desired state is committed.
It does not mean that a runtime has started.

## Apply

HTTP:

```text
PUT /apis/solti.io/v1/tasks/{name}
Content-Type: application/json

TaskManifest
```

`{name}` must equal `metadata.name`.
A successful HTTP apply returns `200 OK` and the committed `Task`.

gRPC:

```text
/solti.task.v1.TaskService/ApplyTask

ApplyTaskRequest { manifest, preconditions }
    ──► ApplyTaskResponse { task }
```

Apply without preconditions is an upsert.
It creates a missing resource.
It updates an existing resource.
An unchecked missing apply uses the same retained Task admission as create.
An existing apply is also rejected when its TaskManifest growth would exceed
the aggregate retained TaskManifest byte budget. Shrinking and no-op applies
remain allowed.

Apply, cancel, and delete accept two optional preconditions:

| Field             | Check                                     |
|-------------------|-------------------------------------------|
| `uid`             | Current resource incarnation must match   |
| `resourceVersion` | Current opaque store version must match   |

HTTP carries them as query parameters.
gRPC carries them in `WritePreconditions`.

Any precondition requires an existing public resource.
A missing resource returns `404` or gRPC `NotFound`.
A mismatch returns `409` or gRPC `Aborted`.

HTTP conflicts contain `Status.details.causes`.
Their `reason` is one of `UIDMismatch`, `ResourceVersionMismatch`, or
`PreconditionFailed`.
gRPC conflicts contain encoded `WriteConflictDetails` status details. Each
`WriteConflictCause.reason` is a `WriteConflictReason` enum value; clients must
not branch on the readable cause message.

### Retained Task admission

The bundled adapter uses two independent core budgets.

- The current Task count defaults to 1024.
- Aggregate retained TaskManifest bytes default to 256 MiB.

The byte budget measures only each current Task's compact canonical
`TaskManifest` JSON. It does not measure status, run history, watch history,
output, indexes, or allocator overhead.

Create and unchecked apply for a missing name are rejected atomically when
either budget would be exceeded. An existing apply is rejected atomically when
positive TaskManifest growth would exceed the byte budget. Shrinking and no-op
applies remain allowed. Core does not evict Tasks to admit a write.

Admission rejection returns HTTP `429` with `reason=TooManyRequests` and no
`Retry-After` header. gRPC returns `ResourceExhausted`.

## Desired-state commit

`SupervisorApiAdapter` uses this flow:

```text
create or apply
      ├── validate manifest and preconditions
      ├── commit desired resource
      ├── return committed Task
      └── reconcile generation asynchronously
                    ├── accepted ──► Reconciled=True
                    └── rejected ──► Reconciled=False
```

A spec change increments `metadata.generation`.
It resets execution status to pending.
The `Reconciled` condition becomes `Unknown` for that generation.

A metadata-only change preserves the generation.
It does not rebuild the runtime.

An identical apply is normally a no-op.
When `Reconciled=False`, one identical apply schedules a manual retry.
The condition becomes `Unknown` without incrementing the generation.

Reconciliation is latest-wins by generation.
A stale reconciliation cannot bind or replace the current runtime.
The API does not provide a staged-rollout or availability guarantee.

## Status

`Task.status` contains execution state and controller conditions.

| Field                | Meaning                                       |
|----------------------|-----------------------------------------------|
| `observedGeneration` | Latest generation processed by the controller |
| `phase`              | Current logical lifecycle phase               |
| `attempt`            | Attempt within `observedGeneration`           |
| `exitCode`           | Process exit code, when available             |
| `error`              | Execution diagnostic, when available          |
| `conditions`         | Extensible controller conditions              |

`Task.status.error` is normalized before storage to the longest UTF-8-safe
prefix of at most 32 KiB.

The current condition set always contains one `Reconciled` condition.

| Status    | Meaning                                         |
|-----------|-------------------------------------------------|
| `Unknown` | Reconciliation is scheduled or still unresolved |
| `True`    | Runtime accepted the referenced generation      |
| `False`   | Runtime rejected the referenced generation      |

The condition contains its own `observedGeneration`.
It also contains `reason`, `message`, and `lastTransitionTime`.
While core waits for Taskvisor intake, the condition stays `Unknown` with
`reason=TaskvisorOwnershipAndControllerIntakePending`. Its message names the
combined ownership and controller command-intake wait. Taskvisor exposes that
wait as one future. The condition therefore does not identify which capacity
is currently blocking.

Execution phases are:

```text
pending
running
succeeded
failed
timeout
canceled
exhausted
```

## Get

HTTP:

```text
GET /apis/solti.io/v1/tasks/{name}
```

The response is one `Task`.
An unknown or hidden name returns `404`.

gRPC `GetTask` returns `GetTaskResponse.task`.
An unknown or hidden name returns `NotFound`.

## List

HTTP:

```text
GET /apis/solti.io/v1/tasks
```

The response is a Kubernetes-shaped `TaskList`.

```json
{
  "apiVersion": "solti.io/v1",
  "kind": "TaskList",
  "metadata": {
    "resourceVersion": "opaque-version",
    "continue": "opaque-token",
    "remainingItemCount": 12
  },
  "items": []
}
```

`continue` and `remainingItemCount` are absent on the final page.

HTTP list parameters are:

| Parameter       | Meaning                                               |
|-----------------|-------------------------------------------------------|
| `slot`          | Match one slot                                        |
| `phase`         | Match any supplied phase; may be repeated             |
| `labelSelector` | Match a Kubernetes-style label selector               |
| `limit`         | Page size; omitted or `0` means `100`; maximum `1000` |
| `continue`      | Resume the snapshot identified by a previous page     |

Filters are combined with AND.
Repeated phases are combined with OR.
Unknown query parameters are rejected.
Singleton parameters cannot be repeated.

gRPC `ListTasksRequest` carries the same filters.
Its `phases` field is repeated.

With `SupervisorApiAdapter`, list results are ordered by task name.
Pagination uses a snapshot captured for the first page.
Every page in one chain has the same collection `resourceVersion`.
The count limit is a ceiling.
The bundled adapter also limits compact-JSON Task item payloads to 4 MiB before
transport encoding. It passes an oversized first Task through alone for native
measurement.
HTTP also limits the complete compact-JSON `TaskList` body to 4 MiB.
gRPC limits the encoded `ListTasksResponse` protobuf message to 4 MiB.
Each transport returns the largest complete prefix from that domain page that
fits its native response limit.
Truncation advances the continuation only through the last returned Task and
adds the deferred Tasks to the exact `remainingItemCount`.

If one Task cannot fit with required collection metadata, HTTP returns `429`
with `reason=TooManyRequests` and gRPC returns `ResourceExhausted`.

The continuation token is opaque.
Clients must return it unchanged.
Filters must remain unchanged for the full continuation chain.

An invalid continuation returns `400` or gRPC `InvalidArgument`.
An unavailable snapshot returns `410 Gone` or gRPC `OutOfRange`.

## Watch

HTTP selects watch mode on the list route:

```text
GET /apis/solti.io/v1/tasks?watch=true&resourceVersion={opaque-version}
Accept: application/json
```

`watch` accepts `true`, `false`, `1`, or `0`.
`limit` and `continue` are rejected in watch mode.
`resourceVersion` is accepted only in watch mode.

The response is a sequence of JSON documents.
Each document ends with a newline.

```json
{"type":"ADDED","object":{"apiVersion":"solti.io/v1","kind":"Task"}}
{"type":"MODIFIED","object":{"apiVersion":"solti.io/v1","kind":"Task"}}
{"type":"DELETED","object":{"apiVersion":"solti.io/v1","kind":"Task"}}
```

Watch event types are:

| Type       | Meaning                                            |
|------------|----------------------------------------------------|
| `ADDED`    | A task entered the selected collection             |
| `MODIFIED` | A selected task changed                            |
| `DELETED`  | A task left the collection or was deleted          |
| `ERROR`    | The stream ended with an API error                 |

An absent `resourceVersion` or `"0"` emits the current snapshot first.
Snapshot tasks are emitted as `ADDED` in task-name order.
The stream then emits live changes.

A specific version replays later retained changes.
It then continues with live changes.

The bundled core admits 256 concurrent Task watches by default. Compact Task
JSON retained by initial and replay buffers has a 64 MiB aggregate default.
When either admission limit is full, the initial request returns HTTP `429`
with `reason=TooManyRequests`, or gRPC `ResourceExhausted`. Rejection is atomic
and does not evict an existing watch.

Buffered bytes are released as events are sent. Lag recovery waits for byte
capacity without retaining replay payload across the wait. History compaction
during that wait still produces the existing expired-version error. Live
delivery and events already yielded to the client are outside the internal
retained-payload budget.

The initial request returns `410 Gone` when its position is no longer retained.
An error after streaming starts becomes one final `ERROR` document.
The HTTP stream closes after that document.

gRPC `WatchTasks` is a server stream.
An initial error is a normal gRPC status.
A later error terminates the stream with that status.

Clients resume from the latest processed `object.metadata.resourceVersion`.

## Run history

HTTP:

```text
GET /apis/solti.io/v1/tasks/{name}/runs?limit=100&continue={opaque-token}
```

`limit` is optional. Omitted or `0` means `100`; the maximum is `1000`.
`continue` is an opaque token returned by the previous page.
Unknown or repeated query parameters are rejected.

The response shape is:

```json
{
  "metadata": {
    "taskUid": "task-incarnation-uid",
    "resourceVersion": "opaque-run-version",
    "continue": "opaque-token",
    "remainingItemCount": 2
  },
  "runs": [
    {
      "workload": {
        "apiVersion": "solti.io/v1",
        "kind": "Subprocess"
      },
      "generation": 1,
      "attempt": 1,
      "phase": "succeeded",
      "startedAt": "2026-07-29T10:00:00Z",
      "finishedAt": "2026-07-29T10:00:01Z",
      "exitCode": 0
    }
  ]
}
```

`continue` and `remainingItemCount` are absent on the final page.
Runs are ordered by generation and attempt.
An active run has phase `running` and no terminal fields.
A terminal run has a terminal phase and `finishedAt`.
That timestamp records the supervisor's logical outcome. A force-aborted run
can remain physically active afterward.
`TaskRun.error`, when present, is normalized before storage to the longest
UTF-8-safe prefix of at most 32 KiB.

The first page captures a separate TaskRun collection snapshot.
`metadata.taskUid` identifies the exact Task incarnation whose runs are in that
snapshot. It remains constant across the full continuation chain.
Its continuation binds the Task name, Task UID, run collection version, and
last returned generation and attempt.
Every page in one chain reads that snapshot, including the frozen value of a
run that later finishes.
Deletion and recreation under the same name do not move the chain to the new
Task UID.

Visibility filtering happens before pagination.
The count and `remainingItemCount` cover only public runs.
HTTP limits the complete compact-JSON response to 4 MiB.
gRPC limits the encoded `ListTaskRunsResponse` message to 4 MiB.
Each transport returns the largest complete run prefix that fits and advances
the continuation only through the last returned run.
One run that cannot fit returns HTTP `429` with `reason=TooManyRequests` or
gRPC `ResourceExhausted`.

A malformed token or a token for another Task returns `400` or gRPC
`InvalidArgument`.
An unavailable run snapshot returns `410 Gone` or gRPC `OutOfRange`.

gRPC uses `ListTaskRunsRequest` and `ListTaskRunsResponse`.
The request carries `name`, `limit`, and `continue`.
The response carries `runs`, `task_uid`, `resource_version`, `continue`, and
`remaining_item_count`. `task_uid` is the same Task-incarnation identity exposed
as HTTP `metadata.taskUid`.
Its timestamps are Unix milliseconds.

## Cancel

HTTP:

```text
POST /apis/solti.io/v1/tasks/{name}/cancel?uid={uid}&resourceVersion={opaque-version}
```

Preconditions are optional. A successful cancel requests a terminal logical
outcome while retaining desired Task state and run history. HTTP returns `204 No
Content`.

gRPC uses `CancelTaskRequest { name, preconditions }` and returns an empty
`CancelTaskResponse`.

Cancellation does not suppress later reconciliation. Force-aborted task code can
remain physically active after the response.

## Delete

HTTP:

```text
DELETE /apis/solti.io/v1/tasks/{name}?uid={uid}&resourceVersion={opaque-version}
```

Preconditions are optional.
A successful delete records a terminal logical outcome and purges retained state.
Force-aborted task code can remain physically active after the response.
HTTP returns `204 No Content`.

gRPC uses `DeleteTaskRequest`.
It returns an empty `DeleteTaskResponse`.

## Live output

HTTP:

```text
GET /apis/solti.io/v1/tasks/{name}/logs?taskUid={uid}
Accept: text/event-stream
```

`taskUid` is required and must identify the current Task incarnation. A missing
or invalid value returns `400 Bad Request`. A valid non-current UID returns `404
Not Found`. Both failures happen before streaming starts.

The response uses Server-Sent Events.
The current event names are:

- `chunk`;
- `run-started`;
- `run-finished`;
- `lagged`.

Example frames:

```text
event: run-started
data: {"taskUid":"task-incarnation-uid","type":"runStarted","generation":2,"attempt":1,"startedAt":1712750400000}

event: chunk
data: {"taskUid":"task-incarnation-uid","type":"chunk","generation":2,"attempt":1,"stream":"stdout","seq":0,"ts":1712750400123,"line":"aGVsbG8="}

event: run-finished
data: {"taskUid":"task-incarnation-uid","type":"runFinished","generation":2,"attempt":1,"exitCode":0,"finishedAt":1712750400456}

event: lagged
data: {"taskUid":"task-incarnation-uid","type":"lagged","skipped":42}
```

`line` contains standard padded base64.
It preserves non-UTF-8 output.
It contains the exact retained source prefix without the recognized line delimiter.
When source bytes were omitted, the chunk also contains `"truncated":true`.
The field is absent when it is false.

The stream is live-only.
It has no persistence or replay.
A slow subscriber can miss events.
`lagged.skipped` reports how many events were missed and
`lagged.skippedBytes` reports the exact retained line bytes carried by those
events. `skippedBytes` is omitted when zero. Lag metadata is separate from
`line`; it never changes or inserts text into raw output bytes.

Run markers are best-effort observations.
They are not ordering barriers for chunks.
Clients identify a run by Task UID, generation, and attempt.
They group chunks by Task UID, generation, attempt, and stream.
They order those chunks by `seq`.

With `SupervisorApiAdapter`, a subscription is atomically pinned to the supplied
Task UID and the generation visible when it opens.
It can span later attempts of that generation.
Events from another generation are filtered out.
Deleting and recreating the same name does not retarget an existing stream.
Every SSE event contains the same `taskUid`, including `lagged` events.

The HTTP transport sends periodic SSE keep-alive comments.

gRPC `StreamTaskLogs` is a server stream. `StreamTaskLogsRequest.task_uid` is
required and has the same current-incarnation check as HTTP `taskUid`. A missing
or invalid value returns `InvalidArgument`; a valid non-current UID returns
`NotFound`. Both failures happen before streaming starts.
It carries the same four event variants in a protobuf `oneof`.
Every `StreamTaskLogsResponse.task_uid` contains the supplied Task UID and remains
constant for the lifetime of the stream.
Protobuf carries `line` as raw bytes.
`OutputChunk.truncated` distinguishes a retained prefix from a complete line.
`Lagged.skipped_bytes` reports retained line bytes lost before the next event.

## HTTP encoding

HTTP uses JSON field names from `solti-model`.
It does not use protobuf JSON encoding.

| Value                              | HTTP encoding              |
|------------------------------------|----------------------------|
| Resource and run timestamps        | RFC 3339 string            |
| Live-output timestamps             | Unix milliseconds          |
| Output bytes                       | Standard padded base64     |
| `resourceVersion` and `continue`   | Opaque strings             |
| Empty optional maps and lists      | Usually omitted            |

Create and apply require `Content-Type: application/json`.
Missing or unsupported media types return `415`.

Every HTTP request body is limited to 4 MiB.
An oversized body returns `413`.

## gRPC encoding

The protobuf package is `solti.task.v1`.
The complete service method prefix is:

```text
/solti.task.v1.TaskService/
```

Resource and live-output timestamps are Unix milliseconds.
Output chunks contain raw protobuf bytes.
`Subprocess.cwd` and `Wasm.module` carry exact UTF-8 text in both HTTP JSON and
protobuf. Native paths that are not valid UTF-8 are rejected; no wire encoder
performs lossy substitution.
Extension workload specs contain one UTF-8 JSON object in `RawExtension.raw`.
For a Chain workload, `TaskWorkload.api_version` is `chain.solti.io/v1alpha1`, `kind` is `Chain`, and `RawExtension.raw` contains the serialized `ChainSpec` object.
Protobuf JSON represents those bytes as base64; generated clients pass the UTF-8 bytes directly.

Encoded and decoded messages are limited to 4 MiB.
`ListTasks` and `ListTaskRuns` enforce that limit before returning a protobuf response.

## Authentication and authorization

Task API authentication is disabled unless the application configures a static
token or an `ApiAuthenticator`.

HTTP uses:

```text
Authorization: Bearer <token>
```

gRPC uses the same value in `authorization` metadata.
The `Bearer` scheme comparison is case-insensitive.

Missing, malformed, or rejected credentials return HTTP `401` with
`WWW-Authenticate: Bearer`.
gRPC returns `Unauthenticated`.

Applications can replace static token verification with `ApiAuthenticator`.
The authenticator receives the bearer credential and returns an `ApiIdentity`.
Identity subjects and attributes are application-owned.

Applications can install `ApiAuthorizer` independently.
It receives the identity, Task operation, and validated target before the handler operation.
A policy denial returns HTTP `403` or gRPC `PermissionDenied`.

List and Watch use a collection target.
The hook does not filter collection items or stream events.
Tenant or row-level visibility requires a separate scoped-handler design.
Stream authorization is checked when the stream opens.
Solti does not define users, roles, tenants, RBAC rules, or policy storage.

TLS is configured separately.
The application hosting the HTTP router owns HTTP TLS.
The optional `grpc-tls` feature converts `solti-tls` server settings for tonic.

## Errors

Both transports use the same error categories.

| Category                     | HTTP                         | gRPC                |
|------------------------------|------------------------------|---------------------|
| Invalid request              | `400 Bad Request`            | `InvalidArgument`   |
| Missing or invalid token     | `401 Unauthorized`           | `Unauthenticated`   |
| Authorization policy denial  | `403 Forbidden`              | `PermissionDenied`  |
| Existing create target       | `409 Conflict`               | `AlreadyExists`     |
| Failed write precondition    | `409 Conflict`               | `Aborted`           |
| Unknown resource or route    | `404 Not Found`              | `NotFound`          |
| Unsupported method           | `405 Method Not Allowed`     | `Unimplemented`     |
| Unsupported media type       | `415 Unsupported Media Type` | `InvalidArgument`   |
| Oversized request            | `413 Payload Too Large`      | `ResourceExhausted` |
| Resource capacity exhausted  | `429 Too Many Requests`      | `ResourceExhausted` |
| Unavailable resource version | `410 Gone`                   | `OutOfRange`        |
| Service shutting down        | `503 Service Unavailable`    | `Unavailable`       |
| Internal failure             | `500 Internal Server Error`  | `Internal`          |

HTTP errors use a Kubernetes-style `Status` resource.
Resource exhaustion uses `reason=TooManyRequests` and does not add a
`Retry-After` header. It covers retained Task count admission, retained
TaskManifest byte admission, initial Task watch admission, and a Task or
TaskRun that cannot fit into an empty list page.

```json
{
  "apiVersion": "v1",
  "kind": "Status",
  "metadata": {},
  "status": "Failure",
  "message": "task not found",
  "reason": "NotFound",
  "code": 404
}
```

Write conflicts also contain:

```json
{
  "details": {
    "name": "daily-report",
    "group": "solti.io",
    "kind": "Task",
    "causes": [
      {
        "reason": "ResourceVersionMismatch",
        "field": "resourceVersion",
        "message": "expected `10`, current `11`"
      }
    ]
  }
}
```

Internal failures are logged by stable category.
The transport boundary does not write the diagnostic string to logs.
Clients receive the fixed message `internal server error`.

## Version identity

The Task resource API version is `solti.io/v1`.
The HTTP API group path is `/apis/solti.io/v1`.
The protobuf package is `solti.task.v1`.

The OpenAPI document version and the Task API version are separate values.
`HttpApi::build` returns a standalone OpenAPI 3.1 document.
`HttpApi::mount` contributes Task API paths and schemas to the application document.
The standalone document uses `info.version` for Task API v1.
The mounted document keeps the application `info.version`.
The `x-solti-task-api-version` extension identifies the mounted Task API.
