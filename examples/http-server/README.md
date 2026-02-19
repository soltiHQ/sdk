# solti HTTP Server Example
Demonstrates HTTP REST API server with periodic background tasks.

## Running
```bash
cargo run --bin http-server
```

Server starts on `http://0.0.0.0:8080` with 3 periodic tasks:
- **periodic-date**: Prints date every 10 seconds
- **periodic-uptime**: Shows system uptime every 30 seconds
- **periodic-echo**: Echoes message every 5 seconds

## Testing with curl

### Submit a new task
```bash
curl -X POST http://localhost:8080/api/v1/tasks \
  -H "Content-Type: application/json" \
  -d '{
    "spec": {
      "slot": "test-task",
      "kind": {
        "subprocess": {
          "command": "sleep",
          "args": ["2"],
          "env": [],
          "failOnNonZero": true
        }
      },
      "timeoutMs": 5000,
      "restart": "never",
      "backoff": {
        "jitter": "full",
        "firstMs": 1000,
        "maxMs": 5000,
        "factor": 2.0
      },
      "admission": "dropIfRunning",
      "labels": {}
    }
  }'
```

Expected response:
```json
{
  "task_id": "default-runner-test-task-5"
}
```

### Get task status
```bash
curl http://localhost:8080/api/v1/tasks/default-runner-test-task-5
```

Expected response (if task still running):
```json
{
  "info": {
    "id": "default-runner-test-task-5",
    "slot": "test-task",
    "status": "running",
    "attempt": 1,
    "createdAt": 1733734800,
    "updatedAt": 1733734801
  }
}
```

Expected response (if task not found):
```json
{
  "info": null
}
```

### Submit task with environment variables
```bash
curl -X POST http://localhost:8080/api/v1/tasks \
  -H "Content-Type: application/json" \
  -d '{
    "spec": {
      "slot": "env-demo",
      "kind": {
        "subprocess": {
          "command": "sh",
          "args": ["-c", "echo MESSAGE=$MESSAGE"],
          "env": [
            {"key": "MESSAGE", "value": "Hello from solti!"}
          ],
          "failOnNonZero": true
        }
      },
      "timeoutMs": 5000,
      "restart": "never",
      "backoff": {
        "jitter": "none",
        "firstMs": 0,
        "maxMs": 0,
        "factor": 1.0
      },
      "admission": "dropIfRunning",
      "labels": {}
    }
  }'
```

### Submit periodic task (runs every 15 seconds)
```bash
curl -X POST http://localhost:8080/api/v1/tasks \
  -H "Content-Type: application/json" \
  -d '{
    "spec": {
      "slot": "my-periodic",
      "kind": {
        "subprocess": {
          "command": "date",
          "args": [],
          "env": [],
          "failOnNonZero": true
        }
      },
      "timeoutMs": 5000,
      "restart": {
        "type": "always",
        "intervalMs": 15000
      },
      "backoff": {
        "jitter": "equal",
        "firstMs": 1000,
        "maxMs": 5000,
        "factor": 2.0
      },
      "admission": "replace",
      "labels": {}
    }
  }'
```

### Submit task with working directory
```bash
curl -X POST http://localhost:8080/api/v1/tasks \
  -H "Content-Type: application/json" \
  -d '{
    "spec": {
      "slot": "ls-home",
      "kind": {
        "subprocess": {
          "command": "ls",
          "args": ["-la"],
          "env": [],
          "cwd": "/home",
          "failOnNonZero": true
        }
      },
      "timeoutMs": 5000,
      "restart": "never",
      "backoff": {
        "jitter": "none",
        "firstMs": 0,
        "maxMs": 0,
        "factor": 1.0
      },
      "admission": "dropIfRunning",
      "labels": {}
    }
  }'
```

### Error handling examples

#### Invalid request (missing required field):
```bash
curl -X POST http://localhost:8080/api/v1/tasks \
  -H "Content-Type: application/json" \
  -d '{
    "spec": {
      "slot": "test"
    }
  }'
```

Response (400 Bad Request):
```json
{
  "error": "missing field `kind` at line 1 column 22"
}
```

#### Task not found:
```bash
curl http://localhost:8080/api/v1/tasks/nonexistent-task-id
```

Response (200 OK):
```json
{
  "info": null
}
```

## JSON Schema Examples

### CreateSpec Structure
```json
{
  "spec": {
    "slot": "string (required)",
    "kind": {
      "subprocess": {
        "command": "string (required)",
        "args": ["string (optional)"],
        "env": [{"key": "string", "value": "string"}],
        "cwd": "string (optional)",
        "failOnNonZero": "boolean (default: true)"
      }
    },
    "timeoutMs": "number (required)",
    "restart": "never | onFailure | always | {type: 'always', intervalMs: number}",
    "backoff": {
      "jitter": "none | full | equal | decorrelated",
      "firstMs": "number (required)",
      "maxMs": "number (required)",
      "factor": "number (required)"
    },
    "admission": "dropIfRunning | replace | queue",
    "labels": {"key": "value"}
  }
}
```

### TaskInfo Structure
```json
{
  "info": {
    "id": "string",
    "slot": "string",
    "status": "pending | running | succeeded | failed | timeout | canceled | exhausted",
    "attempt": "number",
    "createdAt": "unix_timestamp",
    "updatedAt": "unix_timestamp",
    "error": "string (optional)"
  }
}
```

## Architecture
```
┌─────────────┐
│ curl        │
│ (client)    │
└──────┬──────┘
       │ HTTP/JSON
       ▼
┌─────────────────┐
│ HttpApi         │
│ (axum router)   │
└────────┬────────┘
         │
         ▼
┌─────────────────────┐
│ SupervisorApiAdapter│
└────────┬────────────┘
         │
         ▼
┌──────────────────────┐
│ SupervisorApi        │
│ (solti-core)           │
└────────┬─────────────┘
         │
         ▼
┌──────────────────────┐
│ SubprocessRunner     │
│ (solti-exec)           │
└──────────────────────┘
```

## Comparison with gRPC

| Feature | HTTP | gRPC |
|---------|------|------|
| Protocol | REST/JSON | Protocol Buffers |
| Port | 8080 | 50051 |
| Client tool | curl | grpcurl |
| Content-Type | application/json | application/grpc |
| Field naming | camelCase | camelCase |
| Error format | JSON | gRPC Status |

## Testing Tips

### Pretty print JSON with jq:
```bash
curl http://localhost:8080/api/v1/tasks/task-id | jq
```

### Save task ID to variable:
```bash
TASK_ID=$(curl -s -X POST http://localhost:8080/api/v1/tasks \
  -H "Content-Type: application/json" \
  -d '{"spec": {...}}' | jq -r '.task_id')

echo "Task ID: $TASK_ID"

# Check status
curl http://localhost:8080/api/v1/tasks/$TASK_ID | jq
```

### Monitor task execution:
```bash
TASK_ID="default-runner-test-task-5"

while true; do
  clear
  curl -s http://localhost:8080/api/v1/tasks/$TASK_ID | jq
  sleep 1
done
```

### View metrics
```bash
curl http://localhost:8080/metrics
```