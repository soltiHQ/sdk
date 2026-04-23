# solti-prometheus

Prometheus metrics for the Solti task-orchestration SDK.

Collectors and helpers share one `prometheus::Registry`. 
A single `/metrics` endpoint covers runner internals, supervisor events, API requests, discovery heartbeat, process stats, and build identity.

## What ships

| Component                     | Prefix                        | Feature    | Implements / emits                                        |
|-------------------------------|-------------------------------|------------|-----------------------------------------------------------|
| `PrometheusMetrics`           | `solti_runner_*`              | —          | `solti_runner::MetricsBackend`                            |
| `PrometheusSubscriber`        | `solti_sv_*` + `solti_ctrl_*` | —          | `taskvisor::Subscribe`                                    |
| `PrometheusApiMetrics`        | `solti_api_*`                 | `api`      | `solti_api::ApiMetricsBackend`                            |
| `PrometheusDiscoverMetrics`   | `solti_discover_*`            | `discover` | `solti_discover::DiscoverMetricsBackend`                  |
| `register_process_collector`  | `process_*`                   | `process`  | Prometheus' default process collector (Linux-only effect) |
| `register_build_info`         | `solti_build_info`            | —          | Gauge `= 1` carrying constant labels                      |
| `server`                      | —                             | `server`   | Embedded supervised HTTP task exposing `/metrics`         |

## Architecture

```text
  ┌─────────────────────────────────────────────────────────────────────────┐
  │                         Shared Registry                                 │
  │                                                                         │
  │   PrometheusMetrics         → solti_runner_*                            │
  │   PrometheusSubscriber      → solti_sv_* , solti_ctrl_*                 │
  │   PrometheusApiMetrics      → solti_api_*          [feature: api]       │
  │   PrometheusDiscoverMetrics → solti_discover_*     [feature: discover]  │
  │   register_process_collector→ process_*            [feature: process]   │
  │   register_build_info       → solti_build_info                          │
  └──────────┬──────────────────────────────────────────────┬───────────────┘
             │                                              │
             ▼                                              ▼
        BuildContext          Supervisor                /metrics HTTP
        runners call          event bus                 exposed by
        record_task_*()       fans events               solti_prometheus::server()
                              to on_event()             [feature: server]
```

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

## Controller metrics (`solti_ctrl_*`)

| Metric                         | Type    | Labels | Description            |
|--------------------------------|---------|--------|------------------------|
| `solti_ctrl_submissions_total` | Counter | —      | Controller submissions |
| `solti_ctrl_rejections_total`  | Counter | —      | Controller rejections  |

## API metrics (`solti_api_*`, feature `api`)

| Metric                               | Type      | Labels                                    | Description              |
|--------------------------------------|-----------|-------------------------------------------|--------------------------|
| `solti_api_requests_total`           | Counter   | `transport`, `method`, `path`, `status`   | Completed requests       |
| `solti_api_request_duration_seconds` | Histogram | `transport`, `method`, `path`             | Request duration         |
| `solti_api_in_flight_requests`       | Gauge     | `transport`                               | In-flight request count  |

Request duration buckets (seconds): `0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1, 2.5, 5, 10`.

## Discovery metrics (`solti_discover_*`, feature `discover`)

| Metric                                            | Type      | Labels    | Description                       |
|---------------------------------------------------|-----------|-----------|-----------------------------------|
| `solti_discover_attempts_total`                   | Counter   | —         | Total sync attempts               |
| `solti_discover_outcomes_total`                   | Counter   | `outcome` | `success` / `failure`             |
| `solti_discover_duration_seconds`                 | Histogram | `outcome` | Sync call duration                |
| `solti_discover_failures_total`                   | Counter   | `reason`  | Failures grouped by cause         |
| `solti_discover_last_success_timestamp_seconds`   | Gauge     | —         | UNIX time of last success         |
| `solti_discover_holds_total`                      | Counter   | —         | Server-advised retry holds        |
| `solti_discover_hold_duration_seconds`            | Histogram | —         | Duration of advised holds         |

## Process metrics (`process_*`, feature `process`)

On Linux, `register_process_collector` adds the standard Prometheus process collector: 
 - `process_cpu_seconds_total`
 - `process_resident_memory_bytes`
 - `process_virtual_memory_bytes`
 - `process_open_fds`
 - `process_max_fds`
 - `process_start_time_seconds`

On other targets the function is a no-op.

## Build info

`register_build_info` registers a `solti_build_info` gauge whose value is always `1`. 
Its labels (set as Prometheus *constant* labels) carry build-time identity.

## Event → metric mapping (supervision + controller)

```text
  TaskStarting        → tasks_in_flight.inc()  (+ task_restarts.inc() if attempt > 1)
  TaskStopped         → tasks_in_flight.dec()
  TaskFailed          → tasks_in_flight.dec()
  TimeoutHit          → task_timeouts.inc()
  BackoffScheduled    → task_backoff_count{source}.inc() + task_backoff_duration.observe(delay)
  ActorExhausted      → task_terminal{reason="exhausted"}.inc()
  ActorDead           → task_terminal{reason="fatal"}.inc()
  SubscriberOverflow  → subscriber_overflow.inc()
  SubscriberPanicked  → subscriber_panicked.inc()
  ControllerSubmitted → controller_submissions.inc()
  ControllerRejected  → controller_rejections.inc()
```

## Labels cardinality

| Label       | Values                                                                                                                                    | Cardinality                 |
|-------------|-------------------------------------------------------------------------------------------------------------------------------------------|-----------------------------|
| `runner`    | `subprocess`, `wasm`, `container`                                                                                                         | low                         |
| `outcome`   | `success`, `failure`, `canceled`, `timeout` (runner); `success`, `failure` (discover)                                                     | low                         |
| `error`     | `spawn_failed`, `backend_config_failed`, …                                                                                                | low                         |
| `source`    | `failure`, `success`                                                                                                                      | low                         |
| `reason`    | `exhausted`, `fatal` (terminal); `connect`, `timeout`, `rejected_client`, `rejected_server`, `parse`, `auth`, `other` (discover failures) | low                         |
| `transport` | `http`, `grpc`                                                                                                                            | low                         |
| `method`    | HTTP method for HTTP, RPC method name for gRPC                                                                                            | low                         |
| `path`      | Templated route (`/api/v1/tasks/{id}`) for HTTP, full RPC path for gRPC                                                                   | low (bounded by route set)  |
| `status`    | HTTP status code (HTTP), gRPC code number (gRPC)                                                                                          | low                         |

All label sets have low, bounded cardinality.

## Feature flags

| Flag       | Default | Effect                                                                                               |
|------------|---------|------------------------------------------------------------------------------------------------------|
| `api`      | off     | `PrometheusApiMetrics` (depends on `solti-api`)                                                      |
| `discover` | off     | `PrometheusDiscoverMetrics` (depends on `solti-discover`)                                            |
| `process`  | off     | Makes `register_process_collector` register actual `process_*` metrics on Linux                      |
| `server`   | off     | `server` — a supervised embedded HTTP task serving `/metrics`                                        |

## Example

For a full agent wiring: shared registry, runner metrics, subscriber, supervised`/metrics` HTTP task, HTTP/gRPC `ApiMetricsBackend`, and `DiscoverMetricsBackend` see the reference agents:
- [`examples/agentd-http`](../../examples/agentd-http)
- [`examples/agentd-grpc`](../../examples/agentd-grpc)

## Notes

- `tasks_in_flight` gauge is guarded against going negative: a `TaskStopped` without a preceding `TaskStarting` is a no-op.
- Backoff / discover / API durations are converted from ms → seconds before histogram observation.
- `PrometheusSubscriber` uses `queue_capacity = 2048` (2× taskvisor default) to reduce event loss under high throughput.
- All collectors must share one `prometheus::Registry` for a unified `/metrics` endpoint.
