# tno gRPC Server Example
Demonstrates gRPC API server with periodic background tasks.

## Running
```bash
cargo run --bin grpc-server
```

Server starts on `[::1]:50051` with 3 periodic tasks:
- **periodic-date**: Prints date every 10 seconds
- **periodic-uptime**: Shows system uptime every 30 seconds
- **periodic-echo**: Echoes message every 5 seconds

## Testing with grpcurl
Install grpcurl:
```bash
# macOS
brew install grpcurl

# Linux
go install github.com/fullstorydev/grpcurl/cmd/grpcurl@latest
```

### List available services
```bash
grpcurl -plaintext localhost:50051 list
```

Expected output:
```
tno.v1.TnoApi
```

### Describe service methods
```bash
grpcurl -plaintext localhost:50051 describe tno.v1.TnoApi
```

### Submit a new task
```bash
grpcurl -plaintext -d '{
  "spec": {
    "slot": "test-task",
    "kind": {
      "subprocess": {
        "command": "sleep",
        "args": ["2"],
        "failOnNonZero": true
      }
    },
    "timeoutMs": 5000,
    "restart": "RESTART_STRATEGY_NEVER",
    "backoff": {
      "jitter": "JITTER_STRATEGY_FULL",
      "firstMs": 1000,
      "maxMs": 5000,
      "factor": 2.0
    },
    "admission": "ADMISSION_STRATEGY_DROP_IF_RUNNING"
  }
}' localhost:50051 tno.v1.TnoApi/SubmitTask
```

Expected response:
```json
{
  "taskId": "default-runner-test-task-5"
}
```

### Get task status
```bash
grpcurl -plaintext -d '{
  "taskId": "default-runner-test-task-5"
}' localhost:50051 tno.v1.TnoApi/GetTaskStatus
```

Expected response (if task still running):
```json
{
  "info": {
    "id": "default-runner-test-task-5",
    "slot": "test-task",
    "status": "TASK_STATUS_RUNNING",
    "attempt": 1,
    "createdAt": "1733734800",
    "updatedAt": "1733734801"
  }
}
```

### Submit task with environment variables
```bash
grpcurl -plaintext -d '{
  "spec": {
    "slot": "env-demo",
    "kind": {
      "subprocess": {
        "command": "sh",
        "args": ["-c", "echo MESSAGE=$MESSAGE"],
        "env": [
          {"key": "MESSAGE", "value": "Hello from tno!"}
        ],
        "failOnNonZero": true
      }
    },
    "timeoutMs": 5000,
    "restart": "RESTART_STRATEGY_NEVER",
    "backoff": {
      "jitter": "JITTER_STRATEGY_NONE",
      "firstMs": 0,
      "maxMs": 0,
      "factor": 1.0
    },
    "admission": "ADMISSION_STRATEGY_DROP_IF_RUNNING"
  }
}' localhost:50051 tno.v1.TnoApi/SubmitTask
```

### Submit periodic task (runs every 15 seconds)
```bash
grpcurl -plaintext -d '{
  "spec": {
    "slot": "my-periodic",
    "kind": {
      "subprocess": {
        "command": "date",
        "failOnNonZero": true
      }
    },
    "timeoutMs": 5000,
    "restart": "RESTART_STRATEGY_ALWAYS",
    "restartIntervalMs": 15000,
    "backoff": {
      "jitter": "JITTER_STRATEGY_EQUAL",
      "firstMs": 1000,
      "maxMs": 5000,
      "factor": 2.0
    },
    "admission": "ADMISSION_STRATEGY_REPLACE"
  }
}' localhost:50051 tno.v1.TnoApi/SubmitTask
```

## Proto Schema

View full proto definitions:
```bash
grpcurl -plaintext localhost:50051 describe tno.v1.CreateSpec
grpcurl -plaintext localhost:50051 describe tno.v1.TaskInfo
```

## Architecture
```
┌─────────────┐
│ grpcurl     │
│ (client)    │
└──────┬──────┘
       │ gRPC
       ▼
┌─────────────────┐
│ TnoApiService   │
│ (tno-api)       │
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
│ (tno-core)           │
└────────┬─────────────┘
         │
         ▼
┌──────────────────────┐
│ SubprocessRunner     │
│ (tno-exec)           │
└──────────────────────┘
```