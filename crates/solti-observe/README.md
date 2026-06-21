# solti-observe
Observability primitives for the solti task execution system.

Wires [`tracing`](https://docs.rs/tracing) into solti: logger initialization, supervision event logging, and timezone sync.

## Architecture
```text
  main()
  ├─ init_local_offset()          before tokio runtime (single-threaded)
  └─ tokio::Runtime::new()
      └─ async_main()
          ├─ init_logger(&cfg)    installs global tracing subscriber
          │   ├─ Text  → fmt::Layer (colored, RFC 3339 timestamps)
          │   ├─ Json  → fmt::Layer::json()
          │   └─ Journald → tracing_journald::layer() (Linux only)
          │
          ├─ TracingEventSubscriber     (feature: subscriber)
          │   └─ on_event() → trace!/debug!/info!/warn!/error!
          │
          └─ timezone_sync()            (feature: timezone-sync)
              └─ periodic re-detection of local UTC offset
```

## Logger formats

| Format     | Backend                          | Use case                         |
|------------|----------------------------------|----------------------------------|
| `Text`     | `tracing_subscriber::fmt`        | Local development, human reading |
| `Json`     | `tracing_subscriber::fmt::json`  | Log aggregation (ELK, Loki)      |
| `Journald` | `tracing_journald`               | systemd services (Linux only)    |

## Configuration

| Field          | Default | Description                               |
|----------------|---------|-------------------------------------------|
| `format`       | `Text`  | Human-readable colored output             |
| `level`        | `info`  | `EnvFilter` expression                    |
| `tz`           | `Utc`   | Timestamp timezone                        |
| `with_targets` | `true`  | Include module/target names in output     |
| `use_color`    | `true`  | Colored output (auto-disabled if not TTY) |

Supports serde deserialization with missing-field defaults:
```text
{}                                → all defaults
{"level": "debug"}                → debug level, rest defaults
{"format": "json", "tz": "local"} → JSON with local timestamps
```

## Event → log level mapping (feature `subscriber`)
```text
  Level     Events
  ─────     ──────
  trace     TaskAddRequested, TaskRemoveRequested, TaskRemoved,
            TaskStopped, ControllerSubmitted
  debug     TaskAdded, ActorExhausted, BackoffScheduled,
            ControllerSlotTransition
  info      TaskStarting, ShutdownRequested, AllStoppedWithinGrace
  warn      GraceExceeded, TimeoutHit, ControllerRejected
  error     TaskFailed, ActorDead, SubscriberPanicked,
            SubscriberOverflow
```

## Structured fields

| Field        | Type  | Present when                                 |
|--------------|-------|----------------------------------------------|
| `task`       | `str` | Most events (task name)                      |
| `attempt`    | `u32` | TaskStarting, TaskFailed, BackoffScheduled   |
| `reason`     | `str` | TaskFailed, ActorDead, SubscriberPanicked    |
| `delay_ms`   | `u32` | BackoffScheduled                             |
| `timeout_ms` | `u32` | TimeoutHit                                   |

## Local timezone support
```text
  Problem:  UtcOffset::current_local_offset() reads /etc/localtime
            which is unsafe in multi-threaded processes (tokio)

  Solution: init_local_offset()    call in main() before Runtime::new()
            timezone_sync()        periodic re-detection (handles DST)

  Fallback: if init_local_offset() is not called → UTC + stderr warning
```

## Timezone sync task (feature `timezone-sync`)
```text
  Scenario      Delay           Strategy
  ────────      ─────           ────────
  Success       1 hour          periodic restart (RestartPolicy::Always)
  Failure       5 s → 5 min     exponential backoff with equal jitter
  Duplicate     replaces        AdmissionPolicy::Replace
```

## Feature flags

| Flag            | Default | Dependencies                             | Effect                          |
|-----------------|---------|------------------------------------------|---------------------------------|
| `subscriber`    | off     | `taskvisor`, `async-trait`               | TracingEventSubscriber          |
| `timezone-sync` | off     | `taskvisor`, `tokio-util`, `solti-model` | timezone_sync() periodic task   |

## Key types

| Type                     | Purpose                                                   |
|--------------------------|-----------------------------------------------------------|
| `LoggerConfig`           | Logger configuration (serde-deserializable)               |
| `LoggerFormat`           | Output format: `Text`, `Json`, `Journald`                 |
| `LoggerLevel`            | Validated `EnvFilter` expression wrapper                  |
| `LoggerTimeZone`         | Timestamp timezone: `Utc`, `Local`                        |
| `LoggerRfc3339`          | RFC 3339 formatter implementing `FormatTime`              |
| `LoggerError`            | Error type for initialization and parsing                 |
| `TracingEventSubscriber` | Logs taskvisor events via tracing (feature: `subscriber`) |
| `View`                   | Helper trait for extracting event fields with defaults    |

## Error model
```text
  Variant                When
  ───────                ────
  InvalidFormat          unknown format string (not text/json/journald)
  JournaldNotSupported   journald on non-Linux platform
  JournaldInitFailed     journald layer init error (Linux)
  AlreadyInitialized     init_logger() called twice
  InvalidTimeZone        unknown timezone string (not utc/local)
  InvalidLevel           invalid EnvFilter expression
```

## Notes
- `init_logger` can only be called **once** per process - subsequent calls return `AlreadyInitialized`.
- `init_local_offset` is idempotent: safe to call multiple times, only the first triggers detection.
- `TracingEventSubscriber` uses `queue_capacity = 2048`.
- `EventKind` is `#[non_exhaustive]`; a `_` arm tolerates future taskvisor variants (logged at `debug`).
- `BackoffScheduled` differentiates retry-after-failure vs scheduled-next-run by checking `reason` field presence.
