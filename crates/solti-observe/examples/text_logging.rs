//! # Text logging
//!
//! A binary builds one logger configuration and installs it once.
//! Tracing events then pass through the configured filter and text formatter.
//!
//! This example shows:
//!
//! - programmatic logger configuration;
//! - UTC RFC 3339 timestamps;
//! - deterministic output without ANSI colors;
//! - target and structured field rendering;
//! - a trace event rejected by a debug filter.
//!
//! Run with `cargo run -p solti-observe --example text_logging`.

use solti_observe::{LoggerConfig, LoggerFormat, LoggerLevel, LoggerTimeZone, init_logger};

type ExampleResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

const FLOW: &str = r#"
solti-observe: text logging

  LoggerConfig
      ├── format: text
      ├── level: debug
      ├── timezone: UTC
      ├── targets: enabled
      └── color: disabled
                ▼
            init_logger()
                ├──► validate EnvFilter
                ├──► build RFC 3339 text layer
                └──► install global subscriber
  tracing events ──────────────┤
      ├── trace ──► filtered   │
      ├── debug ───────────────┤
      └── info ────────────────┤
                               ▼
                         readable log lines

  Initialization changes process-global state and can succeed only once.
"#;

fn main() -> ExampleResult {
    println!("{FLOW}");
    println!(
        "[purpose] Install predictable human-readable logging and show which events survive filtering."
    );

    let config = LoggerConfig {
        format: LoggerFormat::Text,
        level: LoggerLevel::new("debug")?,
        timezone: LoggerTimeZone::Utc,
        with_targets: true,
        use_color: false,
    };
    println!(
        "[config] format={}, level={}, timezone={}, targets=true, color=false.",
        config.format,
        config.level.as_str(),
        config.timezone,
    );

    init_logger(&config)?;
    println!("[installation] The process-global text subscriber is active.");
    println!("[events] Emit trace, debug, and info; the debug filter removes trace.");

    tracing::trace!(
        target: "example::runner",
        event = "runner.attempt",
        task_name = "resize-cover",
        "not exported"
    );
    tracing::debug!(
        target: "example::runner",
        event = "runner.attempt",
        task_name = "resize-cover",
        attempt = 2,
        "runner attempt started"
    );
    tracing::info!(
        target: "example::runner",
        event = "runner.attempt",
        task_name = "resize-cover",
        outcome = "success",
        "runner attempt completed"
    );

    println!("\nResult: debug and info are visible with timestamps, targets, and event fields.");
    Ok(())
}
