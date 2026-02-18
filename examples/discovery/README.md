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
