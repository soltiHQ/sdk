//! # solti-prometheus
//!
//! Prometheus metrics for the Solti task-orchestration SDK.
//!
//! Collectors and helpers share one [`prometheus::Registry`].
//! A single `/metrics` endpoint covers runner internals, supervisor events, API requests, discovery heartbeat, process stats, and build identity.
//!
//! ## Collectors and helpers
//!
//! | Component                     | Prefix                        | Feature    | Implements / emits                                        |
//! |-------------------------------|-------------------------------|------------|-----------------------------------------------------------|
//! | [`PrometheusMetrics`]         | `solti_runner_*`              | —          | [`solti_runner::MetricsBackend`]                          |
//! | [`PrometheusSubscriber`]      | `solti_sv_*` + `solti_ctrl_*` | —          | [`taskvisor::Subscribe`]                                  |
//! | [`PrometheusApiMetrics`]      | `solti_api_*`                 | `api`      | `solti_api::ApiMetricsBackend`                            |
//! | [`PrometheusDiscoverMetrics`] | `solti_discover_*`            | `discover` | `solti_discover::DiscoverMetricsBackend`                  |
//! | [`register_process_collector`]| `process_*`                   | `process`  | Prometheus' default process collector (Linux-only effect) |
//! | [`register_build_info`]       | `solti_build_info`            | —          | Gauge `= 1` carrying constant labels                      |
//! | [`server`]                    | —                             | `server`   | Embedded supervised HTTP task exposing `/metrics`         |
//! | [`PrometheusStateCollector`]  | `solti_sv_tasks_by_phase`     | `state`    | Pull-based collector over `solti_core::TaskState`         |
//!
//! ## Architecture
//!
//! ```text
//!   ┌─────────────────────────────────────────────────────────────────────────┐
//!   │                         Shared Registry                                 │
//!   │                                                                         │
//!   │   PrometheusMetrics         → solti_runner_*                            │
//!   │   PrometheusSubscriber      → solti_sv_* , solti_ctrl_*                 │
//!   │   PrometheusApiMetrics      → solti_api_*          [feature: api]       │
//!   │   PrometheusDiscoverMetrics → solti_discover_*     [feature: discover]  │
//!   │   register_process_collector→ process_*            [feature: process]   │
//!   │   register_build_info       → solti_build_info                          │
//!   └──────────┬──────────────────────────────────────────────┬───────────────┘
//!              │                                              │
//!              ▼                                              ▼
//!         BuildContext          Supervisor                /metrics HTTP
//!         runners call          event bus                 exposed by
//!         record_task_*()       fans events               solti_prometheus::server()
//!                               to on_event()             [feature: server]
//! ```
//!
//! ## Runner metrics (`solti_runner_*`)
//!
//! | Metric                               | Type      | Labels              | Description                    |
//! |--------------------------------------|-----------|---------------------|--------------------------------|
//! | `solti_runner_tasks_started_total`   | Counter   | `runner`            | Task spawn events              |
//! | `solti_runner_tasks_completed_total` | Counter   | `runner`, `outcome` | Task completion events         |
//! | `solti_runner_task_duration_seconds` | Histogram | `runner`, `outcome` | Per-attempt execution duration |
//! | `solti_runner_errors_total`          | Counter   | `runner`, `error`   | Runner setup/teardown errors   |
//!
//! Duration histogram buckets (seconds): [ `0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1, 2.5, 5, 10, 30, 60, 300, 1800, 3600`].
//! Dense through the `10 ms - 10 s` range, sparser long tail out to one hour.
//!
//! ## Supervision metrics (`solti_sv_*`)
//!
//! | Metric                                   | Type      | Labels   | Description                  |
//! |------------------------------------------|-----------|----------|------------------------------|
//! | `solti_sv_tasks_in_flight`               | Gauge     | —        | Currently executing tasks    |
//! | `solti_sv_task_restarts_total`           | Counter   | —        | Restarts (attempt > 1)       |
//! | `solti_sv_task_backoff_count_total`      | Counter   | `source` | Backoff events               |
//! | `solti_sv_task_backoff_duration_seconds` | Histogram | —        | Backoff delay duration       |
//! | `solti_sv_task_terminal_total`           | Counter   | `reason` | Terminal task states         |
//! | `solti_sv_attempts_to_finalize`          | Histogram | `outcome`| Attempts when task left loop |
//! | `solti_sv_task_timeouts_total`           | Counter   | —        | Timeout events               |
//! | `solti_sv_subscriber_overflow_total`     | Counter   | —        | Queue overflow (lost events) |
//! | `solti_sv_subscriber_panicked_total`     | Counter   | —        | Subscriber panics            |
//! | `solti_sv_tasks_by_phase`                | Gauge     | `phase`  | Current tasks per phase (feature `state`, pull-based snapshot) |
//!
//! ## Controller metrics (`solti_ctrl_*`)
//!
//! | Metric                         | Type      | Labels   | Description                            |
//! |--------------------------------|-----------|----------|----------------------------------------|
//! | `solti_ctrl_submissions_total` | Counter   | —        | Controller submissions                 |
//! | `solti_ctrl_rejections_total`  | CounterVec| `reason` | Controller rejections grouped by cause |
//!
//! `reason` values (bounded, classified from `Event.reason`):
//! [ `slot_full`, `slot_busy`, `superseded`, `removed`, `shutting_down`, `add_failed`, `remove_failed`, `queue_failed`, `recovery_failed`, `bus_lagged`, `controller_exited`, `other`, `unknown`].
//!
//! ## API metrics (`solti_api_*`, feature `api`)
//!
//! | Metric                               | Type      | Labels                                    | Description              |
//! |--------------------------------------|-----------|-------------------------------------------|--------------------------|
//! | `solti_api_requests_total`           | Counter   | `transport`, `method`, `path`, `status`   | Completed requests       |
//! | `solti_api_request_duration_seconds` | Histogram | `transport`, `method`, `path`             | Request duration         |
//! | `solti_api_in_flight_requests`       | Gauge     | `transport`                               | In-flight request count  |
//!
//! Request duration buckets (seconds): `0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1, 2.5, 5, 10`.
//!
//! ## Discovery metrics (`solti_discover_*`, feature `discover`)
//!
//! | Metric                                            | Type      | Labels    | Description                       |
//! |---------------------------------------------------|-----------|-----------|-----------------------------------|
//! | `solti_discover_attempts_total`                   | Counter   | —         | Total sync attempts               |
//! | `solti_discover_outcomes_total`                   | Counter   | `outcome` | `success` / `failure`             |
//! | `solti_discover_duration_seconds`                 | Histogram | `outcome` | Sync call duration                |
//! | `solti_discover_failures_total`                   | Counter   | `reason`  | Failures grouped by cause         |
//! | `solti_discover_last_success_timestamp_seconds`   | Gauge     | —         | UNIX time of last success         |
//! | `solti_discover_holds_total`                      | Counter   | —         | Server-advised retry holds        |
//! | `solti_discover_hold_duration_seconds`            | Histogram | —         | Duration of advised holds         |
//!
//! ## Process metrics (`process_*`, feature `process`)
//!
//! On Linux, [`register_process_collector`] adds the standard Prometheus process collector:
//!  - `process_cpu_seconds_total`
//!  - `process_resident_memory_bytes`
//!  - `process_virtual_memory_bytes`
//!  - `process_open_fds`
//!  - `process_max_fds`
//!  - `process_start_time_seconds`.
//!
//! On other targets the function is a no-op.
//! (compiles cleanly, registers nothing).
//!
//! ## Build info
//!
//! [`register_build_info`] registers a `solti_build_info` gauge whose value is always `1`.
//! Its labels (set as Prometheus *constant* labels) carry build-time identity.
//!
//! ## Event → metric mapping (supervision + controller)
//!
//! ```text
//!  TaskStarting        → tasks_in_flight.inc() (+ task_restarts.inc() if attempt > 1)
//!  TaskStopped         → tasks_in_flight.dec()
//!  TaskFailed          → tasks_in_flight.dec()
//!  TimeoutHit          → task_timeouts.inc()
//!  BackoffScheduled    → task_backoff_count{source}.inc() + task_backoff_duration.observe(delay)
//!  ActorExhausted      → task_terminal{reason}.inc() + attempts_to_finalize{outcome}.observe(attempt)
//!                        (reason/outcome = "completed" if policy_exhausted_success, else "exhausted")
//!  ActorDead           → task_terminal{reason="fatal"}.inc()     + attempts_to_finalize{outcome="fatal"}.observe(attempt)
//!  SubscriberOverflow  → subscriber_overflow.inc()
//!  SubscriberPanicked  → subscriber_panicked.inc()
//!  ControllerSubmitted → controller_submissions.inc()
//!  ControllerRejected  → controller_rejections{reason}.inc()  (reason classified from Event.reason)
//! ```
//!
//! ## Feature flags
//!
//! | Flag       | Default | Effect                                                                                                            |
//! |------------|---------|-------------------------------------------------------------------------------------------------------------------|
//! | `api`      | off     | Enables [`PrometheusApiMetrics`] (depends on `solti-api`)                                                         |
//! | `discover` | off     | Enables [`PrometheusDiscoverMetrics`] (depends on `solti-discover`)                                               |
//! | `process`  | off     | Makes [`register_process_collector`] register actual `process_*` metrics (Linux); propagates `prometheus/process` |
//! | `server`   | off     | Enables [`server`] - a supervised embedded HTTP task serving `/metrics`                                           |
//! | `state`    | off     | Enables [`PrometheusStateCollector`] — pull-based `solti_sv_tasks_by_phase` snapshot (depends on `solti-core`)    |
//!
//! ## Quick wire
//!
//! ```text
//! use std::sync::Arc;
//! use solti_prometheus::{
//!     PrometheusMetrics, PrometheusSubscriber, Registry,
//!     register_build_info, register_process_collector,
//! };
//!
//! let registry = Arc::new(Registry::new());
//!
//! // Core collectors.
//! let metrics = PrometheusMetrics::new(registry.clone())?;
//! let subscriber = PrometheusSubscriber::new(registry.clone())?;
//!
//! // Standard extras.
//! register_process_collector(&registry)?;
//! register_build_info(&registry, &[
//!     ("version", env!("CARGO_PKG_VERSION")),
//! ])?;
//!
//! // Wire into solti-runner:
//! let ctx = BuildContext::new(RunnerEnv::default(), Arc::new(metrics));
//! let router = RunnerRouter::new().with_context(ctx);
//!
//! // Wire into solti-core supervisor:
//! let subscribers: Vec<Arc<dyn Subscribe>> = vec![Arc::new(subscriber)];
//! ```
//!
//! For a full agent wiring — including the supervised `/metrics` HTTP task, `ApiMetricsBackend` (HTTP + gRPC), and `DiscoverMetricsBackend`
//! _see the reference agents under `examples/agentd-http` and `examples/agentd-grpc`_ .
//!
//! ## Notes
//!
//! - `tasks_in_flight` gauge is guarded against going negative: a
//!   [`TaskStopped`](taskvisor::EventKind::TaskStopped) without a preceding
//!   [`TaskStarting`](taskvisor::EventKind::TaskStarting) is a no-op.
//! - Backoff / discover / API durations are converted from ms → seconds before histogram observation.
//! - [`PrometheusSubscriber`] uses `queue_capacity = 2048` (2× taskvisor default) to reduce event loss under high throughput.
//! - All collectors must share a single [`prometheus::Registry`] for a unified `/metrics` endpoint.
//!
//! ## Also
//!
//! - [`solti_runner::MetricsBackend`] - trait backing [`PrometheusMetrics`].
//! - [`taskvisor::Subscribe`] - trait backing [`PrometheusSubscriber`].
//! - `solti_api::ApiMetricsBackend` - trait backing [`PrometheusApiMetrics`] (feature `api`).
//! - `solti_discover::DiscoverMetricsBackend` - trait backing [`PrometheusDiscoverMetrics`] (feature `discover`).

#![forbid(unsafe_code)]

/// Compiles the runnable Rust code blocks in `README.md` as doctests.
#[cfg(doctest)]
#[doc = include_str!("../README.md")]
struct ReadmeDoctests;

mod register;

mod subscriber;
pub use subscriber::{DEFAULT_QUEUE_CAPACITY, PrometheusSubscriber};

mod process;
pub use process::register_process_collector;

mod backend;
pub use backend::PrometheusMetrics;

mod info;
pub use info::register_build_info;

#[cfg(feature = "discover")]
mod discover;
#[cfg(feature = "discover")]
pub use discover::PrometheusDiscoverMetrics;

#[cfg(feature = "api")]
mod api;
#[cfg(feature = "api")]
pub use api::PrometheusApiMetrics;

#[cfg(feature = "server")]
mod server;
#[cfg(feature = "server")]
pub use server::{METRICS_SERVER_SLOT, server};

#[cfg(feature = "state")]
mod state;
#[cfg(feature = "state")]
pub use state::PrometheusStateCollector;

pub use prometheus::Registry;
