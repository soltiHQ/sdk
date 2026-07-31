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

The HTTP task subtree owns its handler state, authentication, limits, metrics, and fallbacks.
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
| Delete    | `DELETE /apis/solti.io/v1/tasks/{name}`   | `DeleteTask`     |
| Logs      | `GET /apis/solti.io/v1/tasks/{name}/logs` | `StreamTaskLogs` |

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

The built-in `solti.io/v1` `Embedded` workload is SDK-only.
HTTP and gRPC reject it as input.
`SupervisorApiAdapter` also hides it from reads, watches, history, deletion, and output.

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

Apply and delete accept two optional preconditions:

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
gRPC conflicts contain encoded `WriteConflictDetails` status details.

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
| `phase`              | Current execution phase                       |
| `attempt`            | Attempt within `observedGeneration`           |
| `exitCode`           | Process exit code, when available             |
| `error`              | Execution diagnostic, when available          |
| `conditions`         | Extensible controller conditions              |

The current condition set always contains one `Reconciled` condition.

| Status    | Meaning                                         |
|-----------|-------------------------------------------------|
| `Unknown` | Reconciliation is scheduled or still unresolved |
| `True`    | Runtime accepted the referenced generation      |
| `False`   | Runtime rejected the referenced generation      |

The condition contains its own `observedGeneration`.
It also contains `reason`, `message`, and `lastTransitionTime`.

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
GET /apis/solti.io/v1/tasks/{name}/runs
```

The response shape is:

```json
{
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

Runs are ordered by generation and attempt.
An active run has phase `running` and no terminal fields.
A finished run has a terminal phase and `finishedAt`.

gRPC uses `ListTaskRunsRequest` and `ListTaskRunsResponse`.
Its timestamps are Unix milliseconds.

## Delete

HTTP:

```text
DELETE /apis/solti.io/v1/tasks/{name}?uid={uid}&resourceVersion={opaque-version}
```

Preconditions are optional.
A successful delete stops the task and purges its history.
HTTP returns `204 No Content`.

gRPC uses `DeleteTaskRequest`.
It returns an empty `DeleteTaskResponse`.

## Live output

HTTP:

```text
GET /apis/solti.io/v1/tasks/{name}/logs
Accept: text/event-stream
```

The response uses Server-Sent Events.
The current event names are:

- `chunk`;
- `run-started`;
- `run-finished`;
- `lagged`.

Example frames:

```text
event: run-started
data: {"type":"runStarted","generation":2,"attempt":1,"startedAt":1712750400000}

event: chunk
data: {"type":"chunk","generation":2,"attempt":1,"stream":"stdout","seq":0,"ts":1712750400123,"line":"aGVsbG8="}

event: run-finished
data: {"type":"runFinished","generation":2,"attempt":1,"exitCode":0,"finishedAt":1712750400456}

event: lagged
data: {"type":"lagged","skipped":42}
```

`line` contains standard padded base64.
It preserves non-UTF-8 output.

The stream is live-only.
It has no persistence or replay.
A slow subscriber can miss events.
`lagged.skipped` reports how many events were missed.

Run markers are best-effort observations.
They are not ordering barriers for chunks.
Clients group chunks by generation, attempt, and stream.
They order those chunks by `seq`.

With `SupervisorApiAdapter`, a subscription is pinned to the generation visible when it opens.
It can span later attempts of that generation.
Events from another generation are filtered out.

The HTTP transport sends periodic SSE keep-alive comments.

gRPC `StreamTaskLogs` is a server stream.
It carries the same four event variants in a protobuf `oneof`.
Protobuf carries `line` as raw bytes.

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
Extension workload specs contain one UTF-8 JSON object in `RawExtension.raw`.

Encoded and decoded messages are limited to 4 MiB.

## Authentication

Authentication is disabled unless the application configures a token.

HTTP uses:

```text
Authorization: Bearer <token>
```

gRPC uses the same value in `authorization` metadata.
The `Bearer` scheme comparison is case-insensitive.

Missing, malformed, or rejected credentials return HTTP `401`.
gRPC returns `Unauthenticated`.

TLS is configured separately.
The application hosting the HTTP router owns HTTP TLS.
The optional `grpc-tls` feature converts `solti-tls` server settings for tonic.

## Errors

Both transports use the same error categories.

| Category                     | HTTP                         | gRPC                |
|------------------------------|------------------------------|---------------------|
| Invalid request              | `400 Bad Request`            | `InvalidArgument`   |
| Missing or invalid token     | `401 Unauthorized`           | `Unauthenticated`   |
| Existing create target       | `409 Conflict`               | `AlreadyExists`     |
| Failed write precondition    | `409 Conflict`               | `Aborted`           |
| Unknown resource or route    | `404 Not Found`              | `NotFound`          |
| Unsupported method           | `405 Method Not Allowed`     | `Unimplemented`     |
| Unsupported media type       | `415 Unsupported Media Type` | `InvalidArgument`   |
| Oversized request            | `413 Payload Too Large`      | `ResourceExhausted` |
| Unavailable resource version | `410 Gone`                   | `OutOfRange`        |
| Service shutting down        | `503 Service Unavailable`    | `Unavailable`       |
| Internal failure             | `500 Internal Server Error`  | `Internal`          |

HTTP errors use a Kubernetes-style `Status` resource.

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

Internal diagnostics are logged by the server.
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
