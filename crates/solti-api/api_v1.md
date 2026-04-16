# API v1

`API_VERSION = 1`

## API surface

| Operation   | gRPC RPC        | HTTP Endpoint                  | HTTP Method |
|-------------|-----------------|--------------------------------|-------------|
| Submit task | `SubmitTask`    | `/api/v1/tasks`                | POST        |
| Get task    | `GetTaskStatus` | `/api/v1/tasks/{id}`           | GET         |
| List tasks  | `ListTasks`     | `/api/v1/tasks`                | GET         |
| List runs   | `ListTaskRuns`  | `/api/v1/tasks/{id}/runs`      | GET         |
| Cancel task | `CancelTask`    | `/api/v1/tasks/{id}/cancel`    | POST        |
| Delete task | `DeleteTask`    | `/api/v1/tasks/{id}`           | DELETE      |

---

## HTTP examples

### Submit a task (command)

```bash
curl -X POST http://localhost:8080/api/v1/tasks \
  -H "Content-Type: application/json" \
  -d '{
    "spec": {
      "slot": "my-job",
      "kind": {
        "subprocess": {
          "mode": {
            "command": {
              "command": "echo",
              "args": ["hello world"]
            }
          },
          "env": [],
          "failOnNonZero": true
        }
      },
      "timeout": 30000,
      "restart": { "type": "never" },
      "backoff": {
        "jitter": "full",
        "firstMs": 1000,
        "maxMs": 10000,
        "factor": 2.0
      },
      "admission": "dropIfRunning"
    }
  }'
```

Response `201 Created`:
```json
{
  "task_id": "tsk_01JR..."
}
```

### Submit a task (script)

Script body is base64-encoded. `runtime` is one of `bash`, `python`, `node`, or a custom object.

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
          "mode": {
            "script": {
              "runtime": "bash",
              "body": "ZWNobyAiaGVsbG8gZnJvbSBzY3JpcHQiCg==",
              "args": []
            }
          },
          "env": [
            { "key": "ENV", "value": "production" }
          ],
          "failOnNonZero": true
        }
      },
      "timeout": 60000,
      "restart": { "type": "onFailure" },
      "backoff": {
        "jitter": "equal",
        "firstMs": 2000,
        "maxMs": 30000,
        "factor": 2.0
      },
      "admission": "replace"
    }
  }'
```

Custom runtime example:
```json
{
  "runtime": { "custom": { "command": "ruby", "flag": "-e" } },
  "body": "cHV0cyAnaGVsbG8n",
  "args": []
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
      "resourceVersion": 2,
      "createdAt": 1712750400000,
      "updatedAt": 1712750401000
    },
    "spec": {
      "slot": "my-job",
      "kind": {
        "subprocess": {
          "mode": {
            "command": { "command": "echo", "args": ["hello world"] }
          },
          "failOnNonZero": true
        }
      },
      "timeout": 30000,
      "restart": { "type": "never" },
      "backoff": { "jitter": "full", "firstMs": 1000, "maxMs": 10000, "factor": 2.0 },
      "admission": "dropIfRunning"
    },
    "status": {
      "phase": "succeeded",
      "attempt": 1,
      "exitCode": 0
    }
  }
}
```

### List tasks

```bash
# All tasks
curl http://localhost:8080/api/v1/tasks

# Filter by slot
curl "http://localhost:8080/api/v1/tasks?slot=my-job"

# Filter by status + pagination
curl "http://localhost:8080/api/v1/tasks?status=running&limit=10&offset=0"
```

Response `200 OK`:
```json
{
  "tasks": [
    {
      "metadata": { "id": "tsk_01JR...", "resourceVersion": 1, "createdAt": 1712750400000, "updatedAt": 1712750400000 },
      "spec": { "slot": "my-job", "..." : "..." },
      "status": { "phase": "running", "attempt": 1 }
    }
  ],
  "total": 1
}
```

Query parameters:

| Parameter | Type   | Description                                                                     |
|-----------|--------|---------------------------------------------------------------------------------|
| `slot`    | string | Filter by slot name                                                             |
| `status`  | string | `pending`, `running`, `succeeded`, `failed`, `timeout`, `canceled`, `exhausted` |
| `limit`   | u32    | Max results (default 100, max 1000)                                             |
| `offset`  | u32    | Skip first N results                                                            |

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
      "phase": "failed",
      "startedAt": 1712750400000,
      "finishedAt": 1712750402000,
      "error": "exit code 1",
      "exitCode": 1
    },
    {
      "attempt": 2,
      "phase": "succeeded",
      "startedAt": 1712750405000,
      "finishedAt": 1712750406000
    }
  ]
}
```

### Cancel a task

```bash
curl -X POST http://localhost:8080/api/v1/tasks/tsk_01JR.../cancel
```

Response `204 No Content`.

### Delete a task

```bash
curl -X DELETE http://localhost:8080/api/v1/tasks/tsk_01JR...
```

Response `204 No Content`.

### Error responses

```json
{
  "error": "InvalidRequest",
  "message": "slot cannot be empty"
}
```

| HTTP Status | `error` label    | When                                                                                |
|-------------|------------------|-------------------------------------------------------------------------------------|
| 400         | `InvalidRequest` | Validation failure (empty slot, bad spec, invalid status), also `Core::InvalidSpec` |
| 404         | `TaskNotFound`   | Task ID not found                                                                   |
| 413         | *(no body)*      | Request body exceeds 256 KiB (`RequestBodyLimitLayer`)                              |
| 500         | `Internal`       | Supervisor/infra error (also `Core::{Supervisor,Mapping,Runner}`)                   |

### JSON field presence

- Scalar defaults (`0`, `false`, `""`), repeated fields and maps are always emitted (`emit_fields` is set in the proto-JSON codec).
- `optional` message fields are **omitted** when absent (canonical proto3-JSON). For example, `GetTaskStatusResponse` for an unknown task is `{}`, not `{"task": null}`.

---

## gRPC examples

Proto package: `solti.v1`, service: `SoltiApi`.

### Submit a task (command)

```bash
grpcurl -plaintext -d '{
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
    "restart": "RESTART_STRATEGY_NEVER",
    "backoff": {
      "jitter": "JITTER_STRATEGY_FULL",
      "firstMs": "1000",
      "maxMs": "10000",
      "factor": 2.0
    },
    "admission": "ADMISSION_STRATEGY_DROP_IF_RUNNING"
  }
}' localhost:50051 solti.v1.SoltiApi/SubmitTask
```

Response:
```json
{
  "taskId": "tsk_01JR..."
}
```

### Submit a task (script)

```bash
grpcurl -plaintext -d '{
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
    "restart": "RESTART_STRATEGY_ON_FAILURE",
    "backoff": {
      "jitter": "JITTER_STRATEGY_EQUAL",
      "firstMs": "2000",
      "maxMs": "30000",
      "factor": 2.0
    },
    "admission": "ADMISSION_STRATEGY_REPLACE"
  }
}' localhost:50051 solti.v1.SoltiApi/SubmitTask
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

### Get task status

```bash
grpcurl -plaintext -d '{"taskId": "tsk_01JR..."}' \
  localhost:50051 solti.v1.SoltiApi/GetTaskStatus
```

Response:
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
          "command": { "command": "echo", "args": ["hello world"] },
          "failOnNonZero": true
        }
      },
      "timeoutMs": "30000",
      "restart": "RESTART_STRATEGY_NEVER",
      "backoff": { "jitter": "JITTER_STRATEGY_FULL", "firstMs": "1000", "maxMs": "10000", "factor": 2 },
      "admission": "ADMISSION_STRATEGY_DROP_IF_RUNNING"
    },
    "status": {
      "phase": "TASK_STATUS_SUCCEEDED",
      "attempt": 1,
      "exitCode": 0
    }
  }
}
```

### List tasks

```bash
# All tasks
grpcurl -plaintext localhost:50051 solti.v1.SoltiApi/ListTasks

# With filters
grpcurl -plaintext -d '{"slot": "my-job", "status": "TASK_STATUS_RUNNING", "limit": 10}' \
  localhost:50051 solti.v1.SoltiApi/ListTasks
```

Response:
```json
{
  "tasks": [ { "metadata": {}, "spec": {}, "status": {} } ],
  "total": 1
}
```

### List task runs

```bash
grpcurl -plaintext -d '{"taskId": "tsk_01JR..."}' \
  localhost:50051 solti.v1.SoltiApi/ListTaskRuns
```

Response:
```json
{
  "runs": [
    {
      "attempt": 1,
      "status": "TASK_STATUS_FAILED",
      "startedAt": "1712750400000",
      "finishedAt": "1712750402000",
      "error": "exit code 1",
      "exitCode": 1
    }
  ]
}
```

### Cancel / Delete

```bash
grpcurl -plaintext -d '{"taskId": "tsk_01JR..."}' \
  localhost:50051 solti.v1.SoltiApi/CancelTask

grpcurl -plaintext -d '{"taskId": "tsk_01JR..."}' \
  localhost:50051 solti.v1.SoltiApi/DeleteTask
```

Both return `{}`.

### gRPC errors

| gRPC Status        | ApiError variant    | When                         |
|--------------------|---------------------|------------------------------|
| `INVALID_ARGUMENT` | `InvalidRequest`    | Validation failure           |
| `NOT_FOUND`        | `TaskNotFound`      | Task ID not found            |
| `INTERNAL`         | `Internal` / `Core` | Supervisor or internal error |

---

## Protobuf contract

Defined in `proto/solti/v1/`:
- `api.proto` - service definition, request/response messages
- `types.proto` - shared types: `TaskStatus`, `CreateSpec`, `TaskData`, `TaskRunInfo`, policies

Go package: `github.com/soltiHQ/control-plane/api/gen/v1`

## Wire type differences

| Aspect      | HTTP (serde JSON)                           | gRPC (protobuf)                            |
|-------------|---------------------------------------------|--------------------------------------------|
| Task shape  | Domain `Task` (nested metadata/spec/status) | `TaskData` (proto message)                 |
| Timestamps  | Unix ms as number (`1712750400000`)         | Unix ms as string (`"1712750400000"`)      |
| Enums       | camelCase (`"succeeded"`, `"onFailure"`)    | SCREAMING_SNAKE (`TASK_STATUS_SUCCEEDED`)  |
| uint64      | JSON number                                 | String-encoded                             |
| Null/absent | `null` or field omitted                     | Default value or field absent              |
| Field names | camelCase (`firstMs`, `failOnNonZero`)      | camelCase (`firstMs`, `failOnNonZero`)     |
| Restart     | Tagged: `{ "type": "never" }`               | Enum: `RESTART_STRATEGY_NEVER`             |
| Admission   | String: `"dropIfRunning"`                   | Enum: `ADMISSION_STRATEGY_DROP_IF_RUNNING` |
