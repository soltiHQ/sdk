# API v1

`API_VERSION = 1`

## API surface

| Operation | gRPC RPC | HTTP Endpoint |
|-----------|----------|---------------|
| Submit task | `SubmitTask` | `POST /api/v1/tasks` |
| Get task | `GetTaskStatus` | `GET /api/v1/tasks/{id}` |
| List tasks | `ListTasks` | `GET /api/v1/tasks` |
| List runs | `ListTaskRuns` | `GET /api/v1/tasks/{id}/runs` |
| Cancel task | `CancelTask` | `POST /api/v1/tasks/{id}/cancel` |
| Delete task | `DeleteTask` | `DELETE /api/v1/tasks/{id}` |

## Protobuf contract

Defined in `proto/solti/v1/`:
- `api.proto` - service definition with 6 RPCs and request/response messages
- `types.proto` - shared types: `TaskStatus`, `CreateSpec`, `TaskData`, `TaskRunInfo`, policies

The proto carries `go_package` targeting `github.com/soltiHQ/control-plane/api/gen/v1` - the Go control-plane is the primary consumer.

## Wire types

### gRPC

Responses use nested `TaskData`:
```text
TaskData
    ├── ObjectMeta (id, created_at, updated_at, generation, resource_version)
    ├── CreateSpec (slot, kind, timeout, restart, backoff, admission, labels)
    └── TaskStatusInfo (phase, attempt, error, exit_code)
```

### HTTP

Responses return domain `Task` directly via serde:
```text
Task
    ├── metadata (ObjectMeta)
    ├── spec (TaskSpec)
    └── status (TaskStatus)
```

## Query parameters (ListTasks)

| Parameter | Type | Description |
|-----------|------|-------------|
| `slot` | string | Filter by slot name |
| `status` | string/i32 | Filter by task phase |
| `limit` | u32 | Max results per page |
| `offset` | u32 | Skip first N results |
