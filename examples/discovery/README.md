# Discovery Example

Agent simulator with discovery sync, HTTP API, and 5 background tasks.

Simulates a real agent that:
- Periodically syncs with a control plane via `tno-discover`
- Exposes an HTTP API on `:8085` for task management
- Runs 5 background tasks with different strategies

## Background Tasks

| # | Slot              | Command          | Restart        | Description                        |
|---|-------------------|------------------|----------------|------------------------------------|
| 1 | `agent-heartbeat` | `echo`           | `Always` (5s)  | Periodic heartbeat                 |
| 2 | `sys-monitor`     | `uptime`         | `Always` (15s) | System load monitor                |
| 3 | `disk-check`      | `df -h`          | `Always` (30s) | Disk usage check                   |
| 4 | `oneshot-date`    | `date`           | `Never`        | One-shot, completes immediately    |
| 5 | `flaky-job`       | `sh -c "exit 1"` | `OnFailure`    | Always fails, retries with backoff |

Plus 2 internal tasks: `tno-discover-sync` and `tno-observe-timezone-sync`.

## Run

```bash
cargo run -p discovery
```

> Discovery sync will fail if there is no control plane on `:8082` — this is expected.
> The agent and HTTP API work fine regardless.

## API

Base URL: `http://localhost:8085`

---

### List all tasks (paginated)

```bash
curl -s http://localhost:8085/api/v1/tasks | jq
```

Response:

```json
{
  "tasks": [
    {
      "id": "...",
      "slot": "agent-heartbeat",
      "status": "running",
      "attempt": 3,
      "createdAt": 1739900000,
      "updatedAt": 1739900015
    }
  ],
  "total": 7
}
```

### Pagination

```bash
# First 2 tasks
curl -s 'http://localhost:8085/api/v1/tasks?limit=2' | jq

# Next 2
curl -s 'http://localhost:8085/api/v1/tasks?limit=2&offset=2' | jq
```

### Filter by slot

```bash
curl -s 'http://localhost:8085/api/v1/tasks?slot=flaky-job' | jq
```

### Filter by status

```bash
curl -s 'http://localhost:8085/api/v1/tasks?status=running' | jq
curl -s 'http://localhost:8085/api/v1/tasks?status=succeeded' | jq
curl -s 'http://localhost:8085/api/v1/tasks?status=failed' | jq
```

### Combined filters (slot + status + pagination)

```bash
curl -s 'http://localhost:8085/api/v1/tasks?slot=flaky-job&status=failed' | jq
curl -s 'http://localhost:8085/api/v1/tasks?status=running&limit=3&offset=0' | jq
```

### Get task by ID

```bash
# Replace TASK_ID with an actual ID from list response
curl -s http://localhost:8085/api/v1/tasks/TASK_ID | jq
```

### Submit a new task

```bash
# One-shot: list /tmp
curl -s -X POST http://localhost:8085/api/v1/tasks \
  -H 'Content-Type: application/json' \
  -d '{
    "spec": {
      "slot": "manual-ls",
      "kind": {
        "subprocess": {
          "command": "ls",
          "args": ["-la", "/tmp"]
        }
      },
      "timeoutMs": 5000,
      "restart": "never",
      "backoff": {
        "jitter": "none",
        "firstMs": 1000,
        "maxMs": 5000,
        "factor": 2.0
      },
      "admission": "dropIfRunning"
    }
  }' | jq
```

```bash
# Periodic: echo every 10s
curl -s -X POST http://localhost:8085/api/v1/tasks \
  -H 'Content-Type: application/json' \
  -d '{
    "spec": {
      "slot": "manual-echo",
      "kind": {
        "subprocess": {
          "command": "echo",
          "args": ["hello from curl"]
        }
      },
      "timeoutMs": 3000,
      "restart": { "type": "always", "intervalMs": 10000 },
      "backoff": {
        "jitter": "equal",
        "firstMs": 1000,
        "maxMs": 5000,
        "factor": 2.0
      },
      "admission": "replace"
    }
  }' | jq
```

```bash
# Long-running: sleep 30s (good for testing cancel)
curl -s -X POST http://localhost:8085/api/v1/tasks \
  -H 'Content-Type: application/json' \
  -d '{
    "spec": {
      "slot": "long-sleep",
      "kind": {
        "subprocess": {
          "command": "sleep",
          "args": ["30"]
        }
      },
      "timeoutMs": 60000,
      "restart": "never",
      "backoff": {
        "jitter": "none",
        "firstMs": 1000,
        "maxMs": 5000,
        "factor": 2.0
      },
      "admission": "dropIfRunning"
    }
  }' | jq
```

### Cancel a task

```bash
curl -s -X POST http://localhost:8085/api/v1/tasks/TASK_ID/cancel
```

### Prometheus metrics

```bash
curl -s http://localhost:8085/metrics
```

## gRPC API

> This example exposes only HTTP. To test gRPC, run `cargo run -p grpc-server` (port `50051`).
> All examples below assume gRPC server on `localhost:50051`.
> Install [grpcurl](https://github.com/fullstorydev/grpcurl): `brew install grpcurl`

Proto path for `-import-path` / `-proto`:

```
crates/tno-api/proto
```

---

### List services

```bash
grpcurl -plaintext -import-path crates/tno-api/proto -proto tno/v1/api.proto \
  localhost:50051 list
```

### ListTasks — unified query (filters + pagination)

```bash
# All tasks (default limit=100)
grpcurl -plaintext -import-path crates/tno-api/proto -proto tno/v1/api.proto \
  localhost:50051 tno.v1.TnoApi/ListTasks

# Filter by slot
grpcurl -plaintext -import-path crates/tno-api/proto -proto tno/v1/api.proto \
  -d '{"slot": "flaky-job"}' \
  localhost:50051 tno.v1.TnoApi/ListTasks

# Filter by status (TASK_STATUS_RUNNING = 2)
grpcurl -plaintext -import-path crates/tno-api/proto -proto tno/v1/api.proto \
  -d '{"status": "TASK_STATUS_RUNNING"}' \
  localhost:50051 tno.v1.TnoApi/ListTasks

# Combined: slot + status
grpcurl -plaintext -import-path crates/tno-api/proto -proto tno/v1/api.proto \
  -d '{"slot": "flaky-job", "status": "TASK_STATUS_FAILED"}' \
  localhost:50051 tno.v1.TnoApi/ListTasks

# Pagination: limit 2, offset 0
grpcurl -plaintext -import-path crates/tno-api/proto -proto tno/v1/api.proto \
  -d '{"limit": 2, "offset": 0}' \
  localhost:50051 tno.v1.TnoApi/ListTasks

# Next page
grpcurl -plaintext -import-path crates/tno-api/proto -proto tno/v1/api.proto \
  -d '{"limit": 2, "offset": 2}' \
  localhost:50051 tno.v1.TnoApi/ListTasks

# All combined: running tasks in slot, page 1
grpcurl -plaintext -import-path crates/tno-api/proto -proto tno/v1/api.proto \
  -d '{"slot": "agent-heartbeat", "status": "TASK_STATUS_RUNNING", "limit": 5, "offset": 0}' \
  localhost:50051 tno.v1.TnoApi/ListTasks
```

### ListAllTasks (legacy — no pagination)

```bash
grpcurl -plaintext -import-path crates/tno-api/proto -proto tno/v1/api.proto \
  localhost:50051 tno.v1.TnoApi/ListAllTasks
```

### ListTasksBySlot (legacy)

```bash
grpcurl -plaintext -import-path crates/tno-api/proto -proto tno/v1/api.proto \
  -d '{"slot": "sys-monitor"}' \
  localhost:50051 tno.v1.TnoApi/ListTasksBySlot
```

### ListTasksByStatus (legacy)

```bash
# TASK_STATUS_PENDING=1, RUNNING=2, SUCCEEDED=3, FAILED=4, TIMEOUT=5, CANCELED=6, EXHAUSTED=7
grpcurl -plaintext -import-path crates/tno-api/proto -proto tno/v1/api.proto \
  -d '{"status": "TASK_STATUS_RUNNING"}' \
  localhost:50051 tno.v1.TnoApi/ListTasksByStatus

grpcurl -plaintext -import-path crates/tno-api/proto -proto tno/v1/api.proto \
  -d '{"status": "TASK_STATUS_FAILED"}' \
  localhost:50051 tno.v1.TnoApi/ListTasksByStatus
```

### GetTaskStatus

```bash
grpcurl -plaintext -import-path crates/tno-api/proto -proto tno/v1/api.proto \
  -d '{"task_id": "TASK_ID"}' \
  localhost:50051 tno.v1.TnoApi/GetTaskStatus
```

### SubmitTask

```bash
# One-shot: ls /tmp
grpcurl -plaintext -import-path crates/tno-api/proto -proto tno/v1/api.proto \
  -d '{
    "spec": {
      "slot": "grpc-ls",
      "kind": {
        "subprocess": {
          "command": "ls",
          "args": ["-la", "/tmp"],
          "fail_on_non_zero": true
        }
      },
      "timeout_ms": 5000,
      "restart": "RESTART_STRATEGY_NEVER",
      "backoff": {
        "jitter": "JITTER_STRATEGY_NONE",
        "first_ms": 1000,
        "max_ms": 5000,
        "factor": 2.0
      },
      "admission": "ADMISSION_STRATEGY_DROP_IF_RUNNING"
    }
  }' \
  localhost:50051 tno.v1.TnoApi/SubmitTask
```

```bash
# Periodic: echo every 10s
grpcurl -plaintext -import-path crates/tno-api/proto -proto tno/v1/api.proto \
  -d '{
    "spec": {
      "slot": "grpc-echo",
      "kind": {
        "subprocess": {
          "command": "echo",
          "args": ["hello from grpcurl"]
        }
      },
      "timeout_ms": 3000,
      "restart": "RESTART_STRATEGY_ALWAYS",
      "restart_interval_ms": 10000,
      "backoff": {
        "jitter": "JITTER_STRATEGY_EQUAL",
        "first_ms": 1000,
        "max_ms": 5000,
        "factor": 2.0
      },
      "admission": "ADMISSION_STRATEGY_REPLACE"
    }
  }' \
  localhost:50051 tno.v1.TnoApi/SubmitTask
```

```bash
# Long-running: sleep 30s (good for testing cancel)
grpcurl -plaintext -import-path crates/tno-api/proto -proto tno/v1/api.proto \
  -d '{
    "spec": {
      "slot": "grpc-sleep",
      "kind": {
        "subprocess": {
          "command": "sleep",
          "args": ["30"]
        }
      },
      "timeout_ms": 60000,
      "restart": "RESTART_STRATEGY_NEVER",
      "backoff": {
        "jitter": "JITTER_STRATEGY_NONE",
        "first_ms": 1000,
        "max_ms": 5000,
        "factor": 2.0
      },
      "admission": "ADMISSION_STRATEGY_DROP_IF_RUNNING"
    }
  }' \
  localhost:50051 tno.v1.TnoApi/SubmitTask
```

### CancelTask

```bash
grpcurl -plaintext -import-path crates/tno-api/proto -proto tno/v1/api.proto \
  -d '{"task_id": "TASK_ID"}' \
  localhost:50051 tno.v1.TnoApi/CancelTask
```

### Proto enum reference

| Enum | Values |
|------|--------|
| `TaskStatus` | `TASK_STATUS_PENDING` (1), `TASK_STATUS_RUNNING` (2), `TASK_STATUS_SUCCEEDED` (3), `TASK_STATUS_FAILED` (4), `TASK_STATUS_TIMEOUT` (5), `TASK_STATUS_CANCELED` (6), `TASK_STATUS_EXHAUSTED` (7) |
| `RestartStrategy` | `RESTART_STRATEGY_NEVER` (1), `RESTART_STRATEGY_ON_FAILURE` (2), `RESTART_STRATEGY_ALWAYS` (3) |
| `JitterStrategy` | `JITTER_STRATEGY_NONE` (1), `JITTER_STRATEGY_FULL` (2), `JITTER_STRATEGY_EQUAL` (3), `JITTER_STRATEGY_DECORRELATED` (4) |
| `AdmissionStrategy` | `ADMISSION_STRATEGY_DROP_IF_RUNNING` (1), `ADMISSION_STRATEGY_REPLACE` (2), `ADMISSION_STRATEGY_QUEUE` (3) |

---

## Typical test workflow

```bash
# 1. Start the agent
cargo run -p discovery

# 2. (in another terminal) See what's running
curl -s http://localhost:8085/api/v1/tasks | jq '.total'
curl -s 'http://localhost:8085/api/v1/tasks?status=running' | jq

# 3. Check the flaky job — it should be retrying
curl -s 'http://localhost:8085/api/v1/tasks?slot=flaky-job' | jq

# 4. Check the oneshot — should be succeeded
curl -s 'http://localhost:8085/api/v1/tasks?slot=oneshot-date' | jq

# 5. Submit a long task and cancel it
TASK_ID=$(curl -s -X POST http://localhost:8085/api/v1/tasks \
  -H 'Content-Type: application/json' \
  -d '{
    "spec": {
      "slot": "cancel-me",
      "kind": { "subprocess": { "command": "sleep", "args": ["60"] } },
      "timeoutMs": 120000,
      "restart": "never",
      "backoff": { "jitter": "none", "firstMs": 1000, "maxMs": 5000, "factor": 2.0 },
      "admission": "dropIfRunning"
    }
  }' | jq -r '.task_id')

echo "Submitted: $TASK_ID"
curl -s http://localhost:8085/api/v1/tasks/$TASK_ID | jq
curl -s -X POST http://localhost:8085/api/v1/tasks/$TASK_ID/cancel
curl -s http://localhost:8085/api/v1/tasks/$TASK_ID | jq

# 6. Pagination — walk through all tasks 2 at a time
curl -s 'http://localhost:8085/api/v1/tasks?limit=2&offset=0' | jq
curl -s 'http://localhost:8085/api/v1/tasks?limit=2&offset=2' | jq
curl -s 'http://localhost:8085/api/v1/tasks?limit=2&offset=4' | jq

# 7. Metrics
curl -s http://localhost:8085/metrics | grep tno
```
