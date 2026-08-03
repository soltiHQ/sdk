//! # Supervised timezone refresh
//!
//! Local timestamps use one process-global UTC-offset cache.
//! The optional timezone task refreshes that cache under Taskvisor supervision.
//!
//! This example shows:
//!
//! - the returned desired-state manifest;
//! - the matching Taskvisor task;
//! - periodic success scheduling;
//! - retry backoff after offset-detection failures;
//! - the boundary before submission and execution.
//!
//! Run with `cargo run -p solti-observe --example timezone_sync --features timezone-sync`.

use solti_model::TaskWorkload;
use solti_observe::timezone_sync;

type ExampleResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

const SLOT: &str = "solti-observe-timezone-sync";

const FLOW: &str = r#"
solti-observe: supervised timezone refresh

  timezone_sync()
      ├──► TaskManifest (desired lifecycle) ─┐
      └──► TaskRef (refresh implementation) ─┤
                                             ▼
                              solti-core embedded-task API
                                             │ submit
                                             ▼
                                     Taskvisor attempt
                                             ├── success ──► update offset cache
                                             │                    └──► run again in 1 hour
                                             └── failure ──► retry with backoff

  Text and JSON local-time formatters read the same offset cache.
  timezone_sync() constructs the task; it does not execute an attempt.
"#;

fn main() -> ExampleResult {
    println!("{FLOW}");
    println!(
        "[purpose] Keep local log timestamps aligned with system offset changes under supervision."
    );

    let (manifest, task_ref) = timezone_sync();
    let spec = manifest.spec();
    let TaskWorkload::Embedded(embedded) = spec.workload() else {
        return Err("timezone sync must produce an Embedded workload".into());
    };

    println!(
        "[manifest] name={}, slot={}, workload=Embedded.",
        manifest.name(),
        manifest.slot(),
    );
    println!("[manifest] revision={}.", embedded.revision());
    println!(
        "[schedule] timeout={} ms, restart={:?}.",
        spec.timeout().as_millis(),
        spec.restart(),
    );
    println!(
        "[retry] first={} ms, max={} ms, factor={}, jitter={:?}.",
        spec.backoff().first_ms,
        spec.backoff().max_ms,
        spec.backoff().factor,
        spec.backoff().jitter,
    );
    println!("[task] TaskRef name={}.", task_ref.name());

    assert_eq!(manifest.name().as_str(), SLOT);
    assert_eq!(manifest.slot().as_str(), SLOT);
    assert_eq!(task_ref.name(), SLOT);
    assert_eq!(spec.timeout().as_millis(), 60_000);

    println!("[runtime] No offset was detected; this example did not submit the TaskRef.");
    println!(
        "\nResult: the manifest and TaskRef are ready for the solti-core embedded-task boundary."
    );
    Ok(())
}
