# solti-prometheus
Prometheus metrics for the solti task execution system.

## Architecture
```text
  ┌─────────────────────────────────────────────────────────┐
  │                  Shared Registry                        │
  │                                                         │
  │  PrometheusMetrics          PrometheusSubscriber        │
  │  (MetricsBackend)           (taskvisor::Subscribe)      │
  │  ├─ solti_runner_*          ├─ solti_sv_*               │
  │  └─ runner calls            └─ event bus fans out       │
  │     record_task_*()            events to on_event()     │
  └─────────────────────────────────────────────────────────┘
          │                            │
          ▼                            ▼
       BuildContext                 Supervisor
       └─► runners                  └─► event stream
```

## Metric namespaces

| Subsystem              | Prefix           | Source                 |
|------------------------|------------------|------------------------|
| PrometheusMetrics      | `solti_runner_*` | `MetricsBackend` trait |
| PrometheusSubscriber   | `solti_sv_*`     | `Subscribe` trait      |
| Controller (optional)  | `solti_ctrl_*`   | `Subscribe` trait      |

## Runner metrics (`solti_runner_*`)

| Metric                               | Type      | Labels              | Description                    |
|--------------------------------------|-----------|---------------------|--------------------------------|
| `solti_runner_tasks_started_total`   | Counter   | `runner`            | Task spawn events              |
| `solti_runner_tasks_completed_total` | Counter   | `runner`, `outcome` | Task completion events         |
| `solti_runner_task_duration_seconds` | Histogram | `runner`, `outcome` | Per-attempt execution duration |
| `solti_runner_errors_total`          | Counter   | `runner`, `error`   | Runner setup/teardown errors   |

Duration histogram buckets (seconds): `0.01, 0.05, 0.1, 0.5, 1, 5, 10, 30, 60, 120, 300, 600, 1800, 3600`.

## Supervision metrics (`solti_sv_*`)

| Metric                                   | Type      | Labels   | Description                  |
|------------------------------------------|-----------|----------|------------------------------|
| `solti_sv_tasks_in_flight`               | Gauge     | —        | Currently executing tasks    |
| `solti_sv_task_restarts_total`           | Counter   | —        | Restarts (attempt > 1)       |
| `solti_sv_task_backoff_count_total`      | Counter   | `source` | Backoff events               |
| `solti_sv_task_backoff_duration_seconds` | Histogram | —        | Backoff delay duration       |
| `solti_sv_task_terminal_total`           | Counter   | `reason` | Terminal task states         |
| `solti_sv_task_timeouts_total`           | Counter   | —        | Timeout events               |
| `solti_sv_subscriber_overflow_total`     | Counter   | —        | Queue overflow (lost events) |
| `solti_sv_subscriber_panicked_total`     | Counter   | —        | Subscriber panics            |

## Controller metrics (`solti_ctrl_*`, feature `controller`)

| Metric                         | Type    | Labels | Description            |
|--------------------------------|---------|--------|------------------------|
| `solti_ctrl_submissions_total` | Counter | —      | Controller submissions |
| `solti_ctrl_rejections_total`  | Counter | —      | Controller rejections  |

## Event → metric mapping
```text
  taskvisor event          metric update
  ───────────────          ─────────────
  TaskStarting           → tasks_in_flight.inc()
                           + task_restarts.inc()  (if attempt > 1)
  TaskStopped            → tasks_in_flight.dec()
  TaskFailed             → tasks_in_flight.dec()
  TimeoutHit             → task_timeouts.inc()
  BackoffScheduled       → task_backoff_count{source}.inc()
                           + task_backoff_duration.observe(delay)
  ActorExhausted         → task_terminal{reason="exhausted"}.inc()
  ActorDead              → task_terminal{reason="fatal"}.inc()
  SubscriberOverflow     → subscriber_overflow.inc()
  SubscriberPanicked     → subscriber_panicked.inc()
  ControllerSubmitted    → controller_submissions.inc()
  ControllerRejected     → controller_rejections.inc()
```

## Labels

| Label     | Values                                      | Cardinality |
|-----------|---------------------------------------------|-------------|
| `runner`  | `subprocess`, `wasm`, `container`           | low         |
| `outcome` | `success`, `failure`, `canceled`, `timeout` | low         |
| `error`   | `spawn_failed`, `backend_config_failed`, …  | low         |
| `source`  | `failure`, `success`                        | low         |
| `reason`  | `exhausted`, `fatal`                        | low         |

All label sets have low, bounded cardinality.

## Feature flags

| Flag         | Default | Effect                                                   |
|--------------|---------|----------------------------------------------------------|
| `controller` | off     | Adds `solti_ctrl_*` metrics for controller submit/reject |

## Notes
- `tasks_in_flight` gauge is guarded against going negative: a `TaskStopped` without a preceding `TaskStarting` is a no-op.
- Backoff duration is converted from milliseconds to seconds before histogram observation.
- `PrometheusSubscriber` uses `queue_capacity = 2048` (2x the taskvisor default) to reduce event loss under high throughput.
- Both collectors must share a single `prometheus::Registry` for a unified `/metrics` endpoint.
