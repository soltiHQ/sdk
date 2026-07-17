# API v1

`API_VERSION = 1`

## API surface

| Operation   | gRPC RPC         | HTTP Endpoint                   | HTTP Method | Success          |
|-------------|------------------|---------------------------------|-------------|------------------|
| Submit task | `SubmitTask`     | `/api/v1/tasks`                 | POST        | `201 Created`    |
| Apply task  | `ApplyTask`      | `/api/v1/tasks`                 | PUT         | `200 OK`         |
| Get task    | `GetTaskStatus`  | `/api/v1/tasks/{id}`            | GET         | `200 OK`         |
| List tasks  | `ListTasks`      | `/api/v1/tasks`                 | GET         | `200 OK`         |
| List runs   | `ListTaskRuns`   | `/api/v1/tasks/{id}/runs`       | GET         | `200 OK`         |
| Stream logs | `StreamTaskLogs` | `/api/v1/tasks/{id}/logs`       | GET (SSE)   | `200 OK`         |
| Delete task | `DeleteTask`     | `/api/v1/tasks/{id}`            | DELETE      | `204 No Content` |

`SubmitTask` honors the admission policy declared in the spec. `ApplyTask` is the
declarative upsert: it **forces `ADMISSION_POLICY_REPLACE`**, overriding whatever
admission the spec declares. If the slot is busy, the controller requests removal
of its current owner and puts the new submission next; a later apply can supersede
it before admission.

Both operations return the new submission's task id after the bounded controller
command queue accepts it. This is not confirmation of slot admission, runtime
registration, or task start. Query task status to observe the asynchronous result.

`DeleteTask` is the single teardown primitive: it stops the task and purges its run history.

---

## One contract, two transports

Both transports carry the **same protobuf messages** defined in
`proto/solti/task/v1/` (package `solti.task.v1`):

- **gRPC** sends them as binary protobuf (`solti.task.v1.TaskService`).
- **HTTP** sends them as canonical proto3-JSON (pbjson): camelCase field names,
  64-bit integers encoded as strings, enums as `SCREAMING_SNAKE` names, `oneof`
  fields flattened into the parent object.

The single exception is the HTTP SSE log stream, whose payload is the domain JSON
encoding of `OutputEvent` — see
[Stream task logs](#stream-task-logs-server-sent-events) and
[Wire encodings](#wire-encodings).

## Authentication

Both transports are unauthenticated by default. To require a shared bearer token:

- **HTTP**: `HttpApi::new(handler).with_auth(token)` — every request must carry an
  `Authorization: Bearer <token>` header, otherwise it is rejected with
  `401 Unauthorized` before reaching any handler.
- **gRPC**: `GrpcApi::new(handler).with_auth(token).server()` — every call must
  carry `authorization: Bearer <token>` metadata, otherwise it fails with
  `UNAUTHENTICATED`.

The `Bearer` scheme is matched case-insensitively and the token is compared in
constant time. This is the same shared secret the agent presents to the control
plane in discovery — one config value enables both directions. Orthogonal to TLS.

---

## Size limits

These caps apply to every request regardless of transport. Exceeding them
is a hard rejection at the boundary — the supervisor is never invoked.

| Limit                            | Value   | Rejected with                                               |
|----------------------------------|---------|-------------------------------------------------------------|
| Script body (decoded, per task)  | 2 MiB   | HTTP 400 `InvalidRequest` / gRPC `INVALID_ARGUMENT`         |
| Request body / gRPC message size | 4 MiB   | HTTP 413 `PayloadTooLarge` / gRPC `RESOURCE_EXHAUSTED`      |

### Why 2 MiB on the script body

Real shell/python/ruby scripts rarely exceed 100 KiB. The 2 MiB cap gives
generous headroom for mega-scripts with inline data while making sure
specs stay small enough to fit in a single gRPC frame (after base64
inflation + proto envelope).

Anything larger belongs out-of-band: a container image layer, a volume
mount, or an object-storage artifact that the script downloads at runtime.
Do not try to stuff megabytes of data into the spec.

### Why 4 MiB on the wire

- Matches the **tonic** server-side and **grpc-go** client-side defaults
  (`max_decoding_message_size`, `MaxCallRecvMsgSize`) — no hidden surprise
  when a client forgets to set explicit options.
- Accommodates a 2 MiB script body (`×4/3` base64 + proto/JSON overhead)
  with ~33% headroom.

### Symmetrical clients

Control-plane or SDK consumer libraries calling into the agent over gRPC
should set **their** `MaxCallRecvMsgSize` / `max_decoding_message_size`
to 4 MiB as well. Otherwise a large `ListTasks` / `ListTaskRuns` response
on a busy agent will fail with `ResourceExhausted` on the client.

For Rust: use [`solti_api::GrpcApi`](#) on the server side — `server()`
applies both limits. For Go clients: set
`grpc.WithDefaultCallOptions(grpc.MaxCallRecvMsgSize(4 << 20),
grpc.MaxCallSendMsgSize(4 << 20))` at dial time.

---

## HTTP examples

Every request/response body is canonical proto3-JSON of the messages in
`proto/solti/task/v1/api.proto`. Watch for three conventions throughout:
64-bit integers are strings, enums are `SCREAMING_SNAKE` names, `oneof`s are
flattened.

### Submit a task (command)

```bash
curl -X POST http://localhost:8080/api/v1/tasks \
  -H "Content-Type: application/json" \
  -d '{
    "spec": {
      "slot": "my-job",
      "kind": {
        "subprocess": {
          "command": {
            "command": "echo",
            "args": ["hello world"]
          },
          "env": [],
          "failOnNonZero": true
        }
      },
      "timeoutMs": "30000",
      "restart": "RESTART_POLICY_NEVER",
      "backoff": {
        "jitter": "JITTER_POLICY_FULL",
        "firstMs": "1000",
        "maxMs": "10000",
        "factor": 2.0
      },
      "admission": "ADMISSION_POLICY_DROP_IF_RUNNING"
    }
  }'
```

`kind` and the subprocess `mode` are proto `oneof`s and arrive **flattened**:
the subprocess object carries `command` *or* `script` directly — there is no
`"mode"` wrapper key. 64-bit integers (`timeoutMs`, `firstMs`, `maxMs`) are
canonically strings; plain JSON numbers are also accepted on input.

Response `201 Created`:
```json
{
  "taskId": "tsk_01JR..."
}
```

### Submit a task (script)

Script body is base64-encoded. The script `runtime` oneof is either `wellKnown`
(`SCRIPT_RUNTIME_BASH`, `SCRIPT_RUNTIME_PYTHON`, `SCRIPT_RUNTIME_NODE`) or a
`custom` interpreter object.

```bash
# echo 'echo "hello from script"' | base64
# ZWNobyAiaGVsbG8gZnJvbSBzY3JpcHQiCg==

curl -X POST http://localhost:8080/api/v1/tasks \
  -H "Content-Type: application/json" \
  -d '{
    "spec": {
      "slot": "my-script",
      "kind": {
        "subprocess": {
          "script": {
            "wellKnown": "SCRIPT_RUNTIME_BASH",
            "body": "ZWNobyAiaGVsbG8gZnJvbSBzY3JpcHQiCg==",
            "args": []
          },
          "env": [
            { "key": "ENV", "value": "production" }
          ],
          "failOnNonZero": true
        }
      },
      "timeoutMs": "60000",
      "restart": "RESTART_POLICY_ON_FAILURE",
      "backoff": {
        "jitter": "JITTER_POLICY_EQUAL",
        "firstMs": "2000",
        "maxMs": "30000",
        "factor": 2.0
      },
      "admission": "ADMISSION_POLICY_REPLACE"
    }
  }'
```

Custom runtime example:
```json
{
  "script": {
    "custom": { "command": "ruby", "flag": "-e" },
    "body": "cHV0cyAnaGVsbG8n",
    "args": []
  }
}
```

`flag` remains required for wire compatibility. The built-in `solti-exec`
runner executes a temporary script file and ignores this legacy inline flag.

### Apply a task (declarative upsert)

Same body shape as submit. The `admission` in the spec is **ignored**: apply
always uses `ADMISSION_POLICY_REPLACE`. For a busy slot, this requests removal of
the current owner and makes the new submission next.

```bash
curl -X PUT http://localhost:8080/api/v1/tasks \
  -H "Content-Type: application/json" \
  -d '{
    "spec": {
      "slot": "my-job",
      "kind": {
        "subprocess": {
          "command": { "command": "echo", "args": ["v2"] },
          "failOnNonZero": true
        }
      },
      "timeoutMs": "30000",
      "restart": "RESTART_POLICY_NEVER",
      "backoff": {
        "jitter": "JITTER_POLICY_FULL",
        "firstMs": "1000",
        "maxMs": "10000",
        "factor": 2.0
      },
      "admission": "ADMISSION_POLICY_DROP_IF_RUNNING"
    }
  }'
```

Response `200 OK` — the new submission's task id after controller-queue intake:
```json
{
  "taskId": "tsk_01JR..."
}
```

### Get task status

```bash
curl http://localhost:8080/api/v1/tasks/tsk_01JR...
```

Response `200 OK`:
```json
{
  "task": {
    "metadata": {
      "id": "tsk_01JR...",
      "createdAt": "1712750400000",
      "updatedAt": "1712750401000",
      "resourceVersion": "2"
    },
    "spec": {
      "slot": "my-job",
      "kind": {
        "subprocess": {
          "env": [],
          "failOnNonZero": true,
          "command": { "command": "echo", "args": ["hello world"] }
        }
      },
      "timeoutMs": "30000",
      "restart": "RESTART_POLICY_NEVER",
      "backoff": { "jitter": "JITTER_POLICY_FULL", "firstMs": "1000", "maxMs": "10000", "factor": 2.0 },
      "admission": "ADMISSION_POLICY_DROP_IF_RUNNING",
      "labels": {}
    },
    "status": {
      "phase": "TASK_PHASE_SUCCEEDED",
      "attempt": 1,
      "exitCode": 0
    }
  }
}
```

Unknown id → `200 OK` with `{}`: the optional `task` field is omitted, not
`null` (see [JSON field presence](#json-field-presence)).

### List tasks

```bash
# All tasks
curl http://localhost:8080/api/v1/tasks

# Filter by slot
curl "http://localhost:8080/api/v1/tasks?slot=my-job"

# Filter by phase + pagination
curl "http://localhost:8080/api/v1/tasks?phase=running&limit=10&offset=0"
```

Response `200 OK` — each entry has the same shape as `task` in
[Get task status](#get-task-status); `total` counts all tasks matching the
filters across pages, not the page size:
```json
{
  "tasks": [
    {
      "metadata": { "id": "tsk_01JR...", "createdAt": "1712750400000", "updatedAt": "1712750400000", "resourceVersion": "1" },
      "spec": { "slot": "my-job", "..." : "..." },
      "status": { "phase": "TASK_PHASE_RUNNING", "attempt": 1 }
    }
  ],
  "total": 1
}
```

Query parameters:

| Parameter | Type   | Description                                                                     |
|-----------|--------|---------------------------------------------------------------------------------|
| `slot`    | string | Filter by slot name                                                             |
| `phase`   | string | `pending`, `running`, `succeeded`, `failed`, `timeout`, `canceled`, `exhausted` |
| `limit`   | u32    | Max results (default 100, max 1000)                                             |
| `offset`  | u32    | Skip first N results                                                            |

Note the asymmetry: the `phase` query parameter takes the short lowercase name,
while phases inside JSON bodies are proto enum names (`TASK_PHASE_RUNNING`).

### List task runs

```bash
curl http://localhost:8080/api/v1/tasks/tsk_01JR.../runs
```

Response `200 OK`:
```json
{
  "runs": [
    {
      "attempt": 1,
      "phase": "TASK_PHASE_FAILED",
      "startedAt": "1712750400000",
      "finishedAt": "1712750402000",
      "error": "exit code 1",
      "exitCode": 1
    },
    {
      "attempt": 2,
      "phase": "TASK_PHASE_SUCCEEDED",
      "startedAt": "1712750405000",
      "finishedAt": "1712750406000",
      "exitCode": 0
    }
  ]
}
```

`finishedAt` is omitted while the attempt is still running; `exitCode` is
omitted when the process was killed or timed out.

### Stream task logs (Server-Sent Events)

Live-only tail of stdout/stderr. A subscription can remain open across retries;
run-boundary markers are best-effort lifecycle observations. Output is neither
persisted nor replayed, so a new subscriber sees only later events. `404` if the
task has no live channel.

```bash
curl -N http://localhost:8080/api/v1/tasks/tsk_01JR.../logs
```

**This is the one endpoint that does not speak proto-JSON.** Each SSE frame's
event name is one of `chunk`, `run-started`, `run-finished`, `lagged`, and its
`data` payload is the domain JSON encoding of `OutputEvent` from `solti-model`:
`type`-tagged camelCase JSON with millisecond timestamps as plain numbers — the
same JSON direct in-process subscribers see. The shape is pinned by the
`sse_wire_shape_is_pinned` test in `solti-model/src/domain/output.rs`.

```text
event: run-started
data: {"type":"runStarted","attempt":1,"startedAt":1712750400000}

event: chunk
data: {"type":"chunk","attempt":1,"stream":"stdout","seq":0,"ts":1712750400123,"line":"hello world"}

event: run-finished
data: {"type":"runFinished","attempt":1,"exitCode":0,"finishedAt":1712750400456}

event: lagged
data: {"type":"lagged","skipped":42}
```

- `run-started` / `run-finished` identify observed attempt boundaries, but they are lossy and are not ordering barriers for chunks. `seq` resets to 0 independently for `stdout` and `stderr` on every new run.
- `exitCode` in `runFinished` is omitted when the process was killed or timed out.
- `lagged` means the subscriber fell behind and `skipped` events were dropped.

### Delete a task

```bash
curl -X DELETE http://localhost:8080/api/v1/tasks/tsk_01JR...
```

Response `204 No Content` (empty body). Stops the task and purges its run history.
Safe to retry — deleting an already-gone task is a no-op.

### Error responses

```json
{
  "error": "InvalidRequest",
  "message": "slot cannot be empty"
}
```

| HTTP Status | `error` label     | When                                                                                |
|-------------|-------------------|-------------------------------------------------------------------------------------|
| 400         | `InvalidRequest`  | Validation failure (empty slot, bad spec, invalid phase), also `Core::InvalidSpec`  |
| 401         | `Unauthenticated` | Bearer token missing or invalid (only when [auth](#authentication) is enabled)      |
| 404         | `TaskNotFound`    | Task ID not found or no live log channel, also `Core::NotFound`                     |
| 409         | `AlreadyExists`   | `Core::AlreadyExists` — a live submission still owns the same task id (including between attempts) |
| 413         | `PayloadTooLarge` | Request body exceeds 4 MiB (`RequestBodyLimitLayer`) — see "Size limits"            |
| 500         | `Internal`        | Supervisor/infra error (also `Core::{Supervisor,Mapping,Runner}`)                   |

### JSON field presence

- Scalar defaults (`0`, `false`, `""`), repeated fields and maps are always emitted (`emit_fields` is set in the proto-JSON codec).
- `optional` message fields are **omitted** when absent (canonical proto3-JSON). For example, `GetTaskStatusResponse` for an unknown task is `{}`, not `{"task": null}`.

---

## gRPC examples

Proto package: `solti.task.v1`, service: `TaskService`. The server does not
expose reflection, so point `grpcurl` at the proto sources with
`-import-path`/`-proto`; the paths below are relative to the `solti-api` crate
root. `grpcurl` encodes requests and responses as the same canonical proto-JSON
the HTTP transport uses, so all bodies match the HTTP examples.

### Submit a task (command)

```bash
grpcurl -plaintext \
  -import-path proto \
  -proto solti/task/v1/api.proto \
  -d '{
  "spec": {
    "slot": "my-job",
    "kind": {
      "subprocess": {
        "command": {
          "command": "echo",
          "args": ["hello world"]
        },
        "failOnNonZero": true
      }
    },
    "timeoutMs": "30000",
    "restart": "RESTART_POLICY_NEVER",
    "backoff": {
      "jitter": "JITTER_POLICY_FULL",
      "firstMs": "1000",
      "maxMs": "10000",
      "factor": 2.0
    },
    "admission": "ADMISSION_POLICY_DROP_IF_RUNNING"
  }
}' localhost:50051 solti.task.v1.TaskService/SubmitTask
```

Response:
```json
{
  "taskId": "tsk_01JR..."
}
```

### Submit a task (script)

```bash
grpcurl -plaintext \
  -import-path proto \
  -proto solti/task/v1/api.proto \
  -d '{
  "spec": {
    "slot": "my-script",
    "kind": {
      "subprocess": {
        "script": {
          "wellKnown": "SCRIPT_RUNTIME_BASH",
          "body": "ZWNobyAiaGVsbG8gZnJvbSBzY3JpcHQiCg==",
          "args": []
        },
        "env": [{ "key": "ENV", "value": "production" }],
        "failOnNonZero": true
      }
    },
    "timeoutMs": "60000",
    "restart": "RESTART_POLICY_ON_FAILURE",
    "backoff": {
      "jitter": "JITTER_POLICY_EQUAL",
      "firstMs": "2000",
      "maxMs": "30000",
      "factor": 2.0
    },
    "admission": "ADMISSION_POLICY_REPLACE"
  }
}' localhost:50051 solti.task.v1.TaskService/SubmitTask
```

Custom runtime:
```json
{
  "script": {
    "custom": { "command": "ruby", "flag": "-e" },
    "body": "cHV0cyAnaGVsbG8n",
    "args": []
  }
}
```

`flag` remains required for wire compatibility. The built-in `solti-exec`
runner executes a temporary script file and ignores this legacy inline flag.

### Apply a task

Same request shape as `SubmitTask`; the spec's `admission` is ignored and
`ADMISSION_POLICY_REPLACE` is always used. Returns the new submission's task id
after controller-queue intake; admission and task start complete asynchronously.

```bash
grpcurl -plaintext \
  -import-path proto \
  -proto solti/task/v1/api.proto \
  -d '{"spec": { ... }}' \
  localhost:50051 solti.task.v1.TaskService/ApplyTask
```

### Get task status

```bash
grpcurl -plaintext \
  -import-path proto \
  -proto solti/task/v1/api.proto \
  -d '{"taskId": "tsk_01JR..."}' \
  localhost:50051 solti.task.v1.TaskService/GetTaskStatus
```

Response — identical to the HTTP [Get task status](#get-task-status) body
(`grpcurl` prints canonical proto-JSON): `metadata` timestamps and
`resourceVersion` as strings, `timeoutMs` as a string, phases as
`TASK_PHASE_*` names.

### List tasks

```bash
# All tasks
grpcurl -plaintext \
  -import-path proto \
  -proto solti/task/v1/api.proto \
  localhost:50051 solti.task.v1.TaskService/ListTasks

# With filters
grpcurl -plaintext \
  -import-path proto \
  -proto solti/task/v1/api.proto \
  -d '{"slot": "my-job", "phase": "TASK_PHASE_RUNNING", "limit": 10}' \
  localhost:50051 solti.task.v1.TaskService/ListTasks
```

Response — same shape as the HTTP [List tasks](#list-tasks) body:
```json
{
  "tasks": [ { "metadata": {}, "spec": {}, "status": {} } ],
  "total": 1
}
```

### List task runs

```bash
grpcurl -plaintext \
  -import-path proto \
  -proto solti/task/v1/api.proto \
  -d '{"taskId": "tsk_01JR..."}' \
  localhost:50051 solti.task.v1.TaskService/ListTaskRuns
```

Response — same shape as the HTTP [List task runs](#list-task-runs) body:
`phase` as `TASK_PHASE_*`, `startedAt`/`finishedAt` as strings.

### Delete

```bash
grpcurl -plaintext \
  -import-path proto \
  -proto solti/task/v1/api.proto \
  -d '{"taskId": "tsk_01JR..."}' \
  localhost:50051 solti.task.v1.TaskService/DeleteTask
```

Returns `{}`. Stops the task and purges its run history. Idempotent.

### Stream logs

Server-streaming RPC with the same semantics as the HTTP/SSE variant but a
different wire shape: each message is a proto `StreamTaskLogsResponse` whose
`oneof kind` carries `chunk`, `runStarted`, `runFinished`, or `lagged`.
Closes with `NOT_FOUND` if no live channel exists.

```bash
grpcurl -plaintext \
  -import-path proto \
  -proto solti/task/v1/api.proto \
  -d '{"taskId": "tsk_01JR..."}' \
  localhost:50051 solti.task.v1.TaskService/StreamTaskLogs
```

### gRPC errors

| gRPC Status          | ApiError variant      | When                                                                |
|----------------------|-----------------------|---------------------------------------------------------------------|
| `INVALID_ARGUMENT`   | `InvalidRequest`      | Validation failure, also `Core::InvalidSpec`                        |
| `UNAUTHENTICATED`    | `Unauthenticated`     | Bearer token missing or invalid (only when [auth](#authentication) is enabled) |
| `NOT_FOUND`          | `TaskNotFound`        | Task ID not found or no live log channel, also `Core::NotFound`     |
| `ALREADY_EXISTS`     | `Core::AlreadyExists` | A live submission still owns the same task id (including between attempts) |
| `RESOURCE_EXHAUSTED` | `PayloadTooLarge`     | Message exceeds the 4 MiB cap — see "Size limits"                   |
| `INTERNAL`           | `Internal` / `Core`   | Supervisor or internal error (also `Core::{Supervisor,Mapping,Runner}`) |

---

## Protobuf contract

Defined in `proto/solti/task/v1/` (package `solti.task.v1`):
- `api.proto` - `TaskService` definition, request/response messages, log-stream events
- `types.proto` - shared types: `TaskPhase`, `CreateSpec`, `TaskData`, `TaskRunInfo`, policies

## Wire encodings

Both transports carry the same proto messages — one contract, two encodings.
There is no separate "HTTP shape": the HTTP transport (de)serializes the
generated proto types through pbjson, which implements canonical proto3-JSON.

| Aspect         | HTTP (proto-JSON)                                              | gRPC (binary protobuf) |
|----------------|-----------------------------------------------------------------|------------------------|
| Messages       | `SubmitTaskRequest`, `TaskData`, ... from `proto/solti/task/v1/` | the same               |
| Field names    | camelCase (`timeoutMs`, `failOnNonZero`)                         | field numbers          |
| 64-bit ints    | strings (`"30000"`); plain numbers accepted on input             | varint                 |
| Enums          | `SCREAMING_SNAKE` names (`RESTART_POLICY_NEVER`)                 | integers               |
| `oneof` fields | flattened into the parent object (no wrapper key)                | tagged field           |
| `optional`     | omitted when absent                                              | field absent           |

The one exception: the SSE payload on `GET /api/v1/tasks/{id}/logs` is the
domain serde encoding of `OutputEvent` (`type`-tagged camelCase JSON, numeric ms
timestamps), not proto-JSON — see
[Stream task logs](#stream-task-logs-server-sent-events). The equivalent gRPC
stream (`StreamTaskLogs`) carries proto `StreamTaskLogsResponse` messages
instead; the two log encodings differ by design.

The HTTP request/response shapes documented above are pinned by
`tests/wire_shape.rs`; the SSE payload is pinned in `solti-model`.
