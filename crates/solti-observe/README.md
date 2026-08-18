# solti-observe

`solti-observe` is the logging setup for a Solti agent binary.
Call `init_logger` once at startup to validate settings and install one global `tracing` subscriber: 
text, JSON, or optional journald output, RFC 3339 timestamps, and an optional supervised task that refreshes the local UTC offset.

## Quick start

```rust,no_run
use solti_observe::{LoggerConfig, LoggerLevel, init_logger};

fn main() -> Result<(), Box<dyn std::error::Error>> {
    let config = LoggerConfig {
        level: LoggerLevel::new("solti_core=debug,info")?,
        ..Default::default()
    };

    init_logger(&config)?;
    tracing::info!("agent ready");
    Ok(())
}
```

`init_logger` installs a process-global subscriber.
Only one installation can succeed.

## What it does

- validates `EnvFilter` expressions before logger installation;
- keeps format, level, timezone, target, and color settings in one serializable value;
- installs text, JSON, or journald output;
- formats text and JSON timestamps as RFC 3339;
- detects the local UTC offset when local timestamps are selected;
- provides an optional supervised task for offset refresh;
- keeps journald, `log` compatibility, and Taskvisor dependencies behind features.

## Inputs and outputs

| API or value        | Input                                      | Output                                      |
|---------------------|--------------------------------------------|---------------------------------------------|
| `LoggerConfig`      | Format, filter, timezone, targets, colors  | Validated logger settings                   |
| `LoggerFormat`      | String or Serde string                     | Text, JSON, or optional journald selection  |
| `LoggerLevel`       | `EnvFilter` expression                     | Preserved and validated filter string       |
| `LoggerTimeZone`    | `utc` or `local`                           | Timestamp offset selection                  |
| `init_logger`       | `&LoggerConfig`                            | Installed global tracing subscriber         |
| `timezone_sync`     | No runtime input                           | `TaskManifest` and embedded `TaskRef`       |

`LoggerConfig` does not install anything by itself.
For local text or JSON timestamps, `init_logger` detects the offset before global subscriber installation.

## Configuration

| Field          | Default | Used by                                  |
|----------------|---------|------------------------------------------|
| `format`       | `Text`  | Backend selection                        |
| `level`        | `info`  | Every backend                            |
| `timezone`     | `Utc`   | Text and JSON timestamps                 |
| `with_targets` | `true`  | Text and JSON event targets              |
| `use_color`    | `true`  | Text output on an interactive terminal   |

Missing Serde fields use these defaults:

```rust
use solti_observe::{LoggerConfig, LoggerFormat, LoggerTimeZone};

let config: LoggerConfig = serde_json::from_str(r#"{
    "format": "json",
    "level": "taskvisor=debug,info",
    "with_targets": false
}"#).unwrap();

assert_eq!(config.format, LoggerFormat::Json);
assert_eq!(config.level.as_str(), "taskvisor=debug,info");
assert_eq!(config.timezone, LoggerTimeZone::Utc);
assert!(!config.with_targets);
assert!(config.use_color);
```

## Output formats

| Format     | Backend                          | Availability                |
|------------|----------------------------------|-----------------------------|
| `Text`     | `tracing_subscriber::fmt`        | Always                      |
| `Json`     | `tracing_subscriber::fmt::json`  | Always                      |
| `Journald` | `tracing_journald`               | Feature `journald`, Linux   |

Text output can use ANSI colors.
Colors require `use_color = true` and an interactive stdout.

JSON output always disables ANSI colors:

```rust,no_run
use solti_observe::{LoggerConfig, LoggerFormat, init_logger};

fn main() -> Result<(), solti_observe::LoggerError> {
    init_logger(&LoggerConfig {
        format: LoggerFormat::Json,
        with_targets: false,
        ..Default::default()
    })?;

    tracing::info!(component = "agent", "ready");
    Ok(())
}
```

Journald uses its native record format.
The timezone, target, and color fields do not configure the journald layer.

## Filtering

`LoggerLevel` accepts expressions supported by `tracing_subscriber::EnvFilter`.
It validates the expression and preserves the original string:

```rust
use solti_observe::LoggerLevel;

let level = LoggerLevel::new("solti_exec=trace,taskvisor=debug,info").unwrap();
assert_eq!(
    level.as_str(),
    "solti_exec=trace,taskvisor=debug,info",
);
```

Invalid expressions return `LoggerError::InvalidLevel`.

## Local timestamps

UTC is the default.
Text and JSON output can use the current local UTC offset:

```rust,no_run
use solti_observe::{LoggerConfig, LoggerTimeZone, init_logger};

fn main() -> Result<(), solti_observe::LoggerError> {
    init_logger(&LoggerConfig {
        timezone: LoggerTimeZone::Local,
        ..Default::default()
    })
}
```

Local offset detection happens before the subscriber is installed.
A detection failure returns `LoggerError::LocalOffsetUnavailable`.
The logger does not fall back to UTC.

## Timezone refresh task

The `timezone-sync` feature exposes a periodic embedded task:

```rust
#[cfg(feature = "timezone-sync")]
{
    use solti_observe::timezone_sync;

    let (manifest, task_ref) = timezone_sync();
    assert_eq!(manifest.slot().as_str(), "solti-observe-timezone-sync");
    let _ = task_ref;
}
```

| Setting         | Value                                      |
|-----------------|--------------------------------------------|
| Slot            | `solti-observe-timezone-sync`              |
| Workload        | Embedded                                   |
| Attempt timeout | 60 seconds                                 |
| Success period  | 1 hour                                     |
| Failure backoff | 5 seconds to 5 minutes, factor `2`         |
| Jitter          | Equal                                      |
| Admission       | `AdmissionPolicy::Replace`                 |

Each attempt detects the current system offset.
A successful attempt replaces the cached value.
Later attempts can observe offset changes, including DST transitions.
A detection failure returns a retryable task error.

## Feature flags

| Feature         | Default | Effect                                      |
|-----------------|---------|---------------------------------------------|
| `journald`      | Off     | Enables the journald format                 |
| `log-compat`    | Off     | Forwards `log` records into `tracing`       |
| `timezone-sync` | Off     | Exposes the supervised offset refresh task  |

## Examples

### Internal examples

These examples stay inside the `solti-observe` responsibility.
Text and JSON are separate executables because each process can install only one global subscriber.
The timezone example stops at the returned manifest and `TaskRef`; it does not start a supervisor.
Each example starts with a text flow diagram, then explains its configuration, emitted events, and result.

Start with human-readable text output:

```bash
cargo run -p solti-observe --example text_logging
```

| Example                                       | Features        | What it shows                                                      |
|-----------------------------------------------|-----------------|--------------------------------------------------------------------|
| [text_logging.rs](examples/text_logging.rs)   | default         | Text setup, filtering, targets, and structured fields.             |
| [json_logging.rs](examples/json_logging.rs)   | default         | Serde configuration, defaults, filtering, and JSON output.         |
| [timezone_sync.rs](examples/timezone_sync.rs) | `timezone-sync` | Embedded refresh manifest, `TaskRef`, schedule, and retry policy.  |

Run the remaining examples explicitly:

```bash
cargo run -p solti-observe --example json_logging
cargo run -p solti-observe --example timezone_sync --features timezone-sync
```

### Full examples

Application-level compositions live in the [`solti` examples](https://github.com/soltiHQ/sdk/tree/main/crates/solti/examples).
They combine component crates and own the complete binary lifecycle.

## Specific behavior

- Parsing `journald` with the feature on a non-Linux target returns `JournaldNotSupported`.
- Parsing `journald` without its feature returns `JournaldNotEnabled`.
- The local offset cache stores whole seconds in one process-global atomic value.
- `LoggerFormat` and `LoggerTimeZone` serialize as canonical lowercase strings.
- Text and JSON timestamps use the cached offset for `LoggerTimeZone::Local`.
- Format and timezone parsing trims whitespace and ignores ASCII case.
- A second global installation returns an initialization error.
- Timestamp formatting failure writes `<invalid-time>`.
- With `log-compat`, initialization failures use `LoggerInitFailed`.
- Without `log-compat`, that error is `AlreadyInitialized`.

## Errors

| Error                    | Cause                                               |
|--------------------------|-----------------------------------------------------|
| `InvalidFormat`          | Unknown format string                               |
| `JournaldNotEnabled`     | Journald requested without its feature              |
| `JournaldNotSupported`   | Journald requested outside Linux                    |
| `JournaldInitFailed`     | Connection to the system journal failed             |
| `AlreadyInitialized`     | Global subscriber already installed                 |
| `LoggerInitFailed`       | Subscriber or `log` compatibility setup failed      |
| `InvalidTimeZone`        | Unknown timezone string                             |
| `InvalidLevel`           | Invalid `EnvFilter` expression                      |
| `LocalOffsetUnavailable` | System local UTC offset could not be determined     |
