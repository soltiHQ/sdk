//! # Discovery retryability
//!
//! Every discovery failure is classified before it reaches Taskvisor.
//! Retryable failures may use the unchanged desired config again.
//! Permanent failures require a config or protocol change.
//!
//! This example shows:
//!
//! - configuration and authentication failures;
//! - control-plane rejection;
//! - HTTP client and transient statuses;
//! - gRPC client and transient statuses;
//! - the stable `Retryability` decision consumed by the embedded task.
//!
//! Run with `cargo run -p solti-discover --example retryability --features http,grpc`.

use solti_discover::{DiscoverError, Retryability};

const FLOW: &str = r#"
solti-discover: failure classification

  configuration / transport / protocol failure
                       │
                       ▼
         DiscoverError::retryability()
              ┌────────┴─────────┐
              ▼                  ▼
         Retryable           Permanent
              ▼                  ▼
      TaskError::Fail     TaskError::Fatal
              ▼                  ▼
     Taskvisor backoff       task stops

  A rejected response is retryable.
  Authentication and permanent client mistakes stop the task.
"#;

fn main() {
    println!("{FLOW}");
    println!("[purpose] Make the retry decision explicit for representative discovery failures.");

    let cases = [
        (
            "invalid config",
            DiscoverError::InvalidConfig("endpoint is empty".into()),
            Retryability::Permanent,
        ),
        (
            "authentication rejected",
            DiscoverError::AuthFailed {
                reason: "token denied".into(),
            },
            Retryability::Permanent,
        ),
        (
            "protocol rejected with hold",
            DiscoverError::Rejected {
                reason: "temporarily overloaded".into(),
                retry_after_s: Some(30),
            },
            Retryability::Retryable,
        ),
        (
            "HTTP invalid request",
            DiscoverError::HttpStatus {
                code: 400,
                body: "bad request".into(),
            },
            Retryability::Permanent,
        ),
        (
            "HTTP throttled",
            DiscoverError::HttpStatus {
                code: 429,
                body: "slow down".into(),
            },
            Retryability::Retryable,
        ),
        (
            "gRPC invalid argument",
            DiscoverError::from(tonic::Status::invalid_argument("bad request")),
            Retryability::Permanent,
        ),
        (
            "gRPC unavailable",
            DiscoverError::from(tonic::Status::unavailable("try another endpoint")),
            Retryability::Retryable,
        ),
    ];

    println!("[classification]");
    for (name, error, expected) in cases {
        let observed = error.retryability();
        println!("      {name:<29} -> {observed:?}: {error}");
        assert_eq!(observed, expected);
    }

    println!(
        "\nResult: transient transport and server conditions are retryable; invalid intent, authentication, and permanent client failures stop unchanged retries."
    );
}
