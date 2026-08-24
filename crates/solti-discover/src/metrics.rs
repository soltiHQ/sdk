//! # Discovery metrics
//!
//! [`DiscoverMetricsBackend`] receives discovery lifecycle measurements.
//! The default backend discards them.
//!
//! ```text
//! sync attempt
//!      ├──► record_attempt
//!      ├──► record_success(duration)
//!      ├──► record_failure(duration, reason)
//!      └──► record_hold(seconds)
//! ```
//!
//! [`DiscoverFailReason`] keeps failure label cardinality bounded.
//! Transport error text is never used as a metric label.

use std::sync::Arc;

#[cfg(any(feature = "grpc", feature = "http"))]
use std::panic::{AssertUnwindSafe, catch_unwind};
#[cfg(any(feature = "grpc", feature = "http"))]
use std::sync::atomic::{AtomicBool, Ordering};

/// Canonical `outcome` label value for a successful sync attempt.
pub const OUTCOME_SUCCESS: &str = "success";
/// Canonical `outcome` label value for a failed sync attempt.
pub const OUTCOME_FAILURE: &str = "failure";

/// Canonical `reason` label for heartbeat failures.
///
/// The set remains bounded regardless of transport error text.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum DiscoverFailReason {
    /// Connection could not be established.
    Connect,
    /// Transport operation timed out.
    Timeout,
    /// Control plane returned a client-side failure.
    RejectedClient,
    /// Control plane returned a server-side failure.
    RejectedServer,
    /// Response body could not be decoded.
    Parse,
    /// Authentication was rejected.
    Auth,
    /// Failure has no more specific category.
    Other,
}

impl DiscoverFailReason {
    /// Returns the stable metric label.
    pub fn as_label(self) -> &'static str {
        match self {
            DiscoverFailReason::RejectedClient => "rejected_client",
            DiscoverFailReason::RejectedServer => "rejected_server",
            DiscoverFailReason::Connect => "connect",
            DiscoverFailReason::Timeout => "timeout",
            DiscoverFailReason::Parse => "parse",
            DiscoverFailReason::Other => "other",
            DiscoverFailReason::Auth => "auth",
        }
    }
}

/// Metrics backend for the discovery heartbeat task.
///
/// Every method has an empty default body.
/// Implementations can override only the required hooks.
///
/// SDK-owned discovery tasks catch unwinding backend panics, discard their opaque payloads, and
/// report the failure without unwinding. `DiscoverConfigBuilder::build` installs one sticky
/// boundary around the supplied handle when a transport feature is enabled. After its first
/// observed panic, later discovery updates are dropped without invoking the backend. Direct
/// application calls to these trait methods are not mediated by that boundary. The process panic
/// hook still runs before the unwind is caught. A process built with `panic = "abort"` cannot
/// isolate a backend panic.
/// If destroying a hostile payload itself panics, that replacement payload is intentionally
/// forgotten to prevent another unwind.
/// Calls that already entered the sticky boundary concurrently may still finish or panic. The
/// boundary prevents later invocations; it does not serialize healthy metrics callbacks.
/// Implementations must not panic; SDK containment is a defensive boundary.
pub trait DiscoverMetricsBackend: Send + Sync + std::fmt::Debug {
    /// Records one transport attempt.
    fn record_attempt(&self) {}

    /// Records a successful transport attempt in milliseconds.
    fn record_success(&self, _duration_ms: u64) {}

    /// Records a failed transport attempt in milliseconds.
    fn record_failure(&self, _duration_ms: u64, _reason: DiscoverFailReason) {}

    /// Records a clamped server-advised hold in seconds.
    fn record_hold(&self, _duration_s: u64) {}
}

/// No-op metrics backend.
#[derive(Debug, Default)]
pub struct NoOpDiscoverMetrics;

impl DiscoverMetricsBackend for NoOpDiscoverMetrics {}

/// Shared discovery metrics backend.
pub type DiscoverMetricsHandle = Arc<dyn DiscoverMetricsBackend>;

/// Creates a no-op metrics handle.
pub fn noop_discover_metrics() -> DiscoverMetricsHandle {
    Arc::new(NoOpDiscoverMetrics)
}

/// Installs one sticky panic boundary around an application discovery metrics backend.
#[cfg(any(feature = "grpc", feature = "http"))]
pub(crate) fn panic_contained_discover_metrics(
    metrics: DiscoverMetricsHandle,
) -> DiscoverMetricsHandle {
    Arc::new(PanicContainedDiscoverMetrics {
        metrics,
        disabled: AtomicBool::new(false),
    })
}

#[cfg(any(feature = "grpc", feature = "http"))]
struct PanicContainedDiscoverMetrics {
    metrics: DiscoverMetricsHandle,
    disabled: AtomicBool,
}

#[cfg(any(feature = "grpc", feature = "http"))]
impl std::fmt::Debug for PanicContainedDiscoverMetrics {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PanicContainedDiscoverMetrics")
            .field("metrics", &"<backend>")
            .field("disabled", &self.disabled.load(Ordering::Acquire))
            .finish()
    }
}

#[cfg(any(feature = "grpc", feature = "http"))]
impl DiscoverMetricsBackend for PanicContainedDiscoverMetrics {
    fn record_attempt(&self) {
        self.invoke("record_attempt", || self.metrics.record_attempt());
    }

    fn record_success(&self, duration_ms: u64) {
        self.invoke("record_success", || {
            self.metrics.record_success(duration_ms);
        });
    }

    fn record_failure(&self, duration_ms: u64, reason: DiscoverFailReason) {
        self.invoke("record_failure", || {
            self.metrics.record_failure(duration_ms, reason);
        });
    }

    fn record_hold(&self, duration_s: u64) {
        self.invoke("record_hold", || {
            self.metrics.record_hold(duration_s);
        });
    }
}

#[cfg(any(feature = "grpc", feature = "http"))]
impl PanicContainedDiscoverMetrics {
    fn invoke(&self, callback: &'static str, invoke: impl FnOnce()) {
        if self.disabled.load(Ordering::Acquire) {
            return;
        }

        if let Err(payload) = catch_unwind(AssertUnwindSafe(invoke)) {
            let report = !self.disabled.swap(true, Ordering::AcqRel);
            dispose_panic_payload(payload);
            if report {
                report_without_unwind(|| {
                    tracing::error!(
                        event = "discovery.metrics_callback_panicked",
                        error_kind = "callback_panicked",
                        callback,
                        "discovery metrics callback panicked; disabling the installed backend"
                    );
                });
            }
        }
    }
}

#[cfg(any(feature = "grpc", feature = "http"))]
pub(crate) fn record_attempt(metrics: &DiscoverMetricsHandle) {
    invoke_metrics_callback("record_attempt", || metrics.record_attempt());
}

#[cfg(any(feature = "grpc", feature = "http"))]
pub(crate) fn record_success(metrics: &DiscoverMetricsHandle, duration_ms: u64) {
    invoke_metrics_callback("record_success", || metrics.record_success(duration_ms));
}

#[cfg(any(feature = "grpc", feature = "http"))]
pub(crate) fn record_failure(
    metrics: &DiscoverMetricsHandle,
    duration_ms: u64,
    reason: DiscoverFailReason,
) {
    invoke_metrics_callback("record_failure", || {
        metrics.record_failure(duration_ms, reason);
    });
}

#[cfg(any(feature = "grpc", feature = "http"))]
pub(crate) fn record_hold(metrics: &DiscoverMetricsHandle, duration_s: u64) {
    invoke_metrics_callback("record_hold", || metrics.record_hold(duration_s));
}

#[cfg(any(feature = "grpc", feature = "http"))]
fn invoke_metrics_callback(callback: &'static str, invoke: impl FnOnce()) {
    if let Err(payload) = catch_unwind(AssertUnwindSafe(invoke)) {
        dispose_panic_payload(payload);
        report_without_unwind(|| {
            tracing::error!(
                event = "discovery.metrics_callback_panicked",
                error_kind = "callback_panicked",
                callback,
                "discovery metrics callback panicked; dropping this update"
            );
        });
    }
}

#[cfg(any(feature = "grpc", feature = "http"))]
fn dispose_panic_payload(payload: Box<dyn std::any::Any + Send>) {
    if let Err(payload) = catch_unwind(AssertUnwindSafe(|| drop(payload))) {
        std::mem::forget(payload);
    }
}

#[cfg(any(feature = "grpc", feature = "http"))]
fn report_without_unwind(report: impl FnOnce()) {
    if let Err(payload) = catch_unwind(AssertUnwindSafe(report)) {
        dispose_panic_payload(payload);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discover_fail_reason_as_label_maps_all_variants() {
        assert_eq!(DiscoverFailReason::Connect.as_label(), "connect");
        assert_eq!(DiscoverFailReason::Timeout.as_label(), "timeout");
        assert_eq!(
            DiscoverFailReason::RejectedClient.as_label(),
            "rejected_client"
        );
        assert_eq!(
            DiscoverFailReason::RejectedServer.as_label(),
            "rejected_server"
        );
        assert_eq!(DiscoverFailReason::Parse.as_label(), "parse");
        assert_eq!(DiscoverFailReason::Auth.as_label(), "auth");
        assert_eq!(DiscoverFailReason::Other.as_label(), "other");
    }

    #[cfg(feature = "http")]
    mod callback_tests {
        use std::sync::{
            Arc,
            atomic::{AtomicUsize, Ordering},
        };
        use std::{env, process::Command};

        use super::*;

        #[test]
        fn config_installed_metrics_sticky_disable_hostile_callbacks_in_subprocess() {
            const CHILD_ENV: &str = "SOLTI_DISCOVER_HOSTILE_METRICS_CHILD";

            if env::var_os(CHILD_ENV).is_some() {
                run_hostile_metrics_child();
                return;
            }

            let test_name = std::thread::current()
                .name()
                .expect("the Rust test harness must name the current test")
                .to_owned();
            let output = Command::new(env::current_exe().expect("the test executable must exist"))
                .arg("--exact")
                .arg(test_name)
                .arg("--nocapture")
                .env(CHILD_ENV, "1")
                .output()
                .expect("the hostile-metrics child test must start");

            assert!(
                output.status.success(),
                "hostile-metrics child failed with {}\nstdout:\n{}\nstderr:\n{}",
                output.status,
                String::from_utf8_lossy(&output.stdout),
                String::from_utf8_lossy(&output.stderr),
            );
        }

        fn run_hostile_metrics_child() {
            let reports = Arc::new(AtomicUsize::new(0));
            let retained = Arc::new(());
            tracing::subscriber::set_global_default(HostileSubscriber {
                reports: Arc::clone(&reports),
                retained: Arc::clone(&retained),
            })
            .expect("the isolated child must install its tracing subscriber once");
            let calls = Arc::new(AtomicUsize::new(0));
            let raw: DiscoverMetricsHandle = Arc::new(PanickingMetrics {
                calls: Arc::clone(&calls),
                retained: Arc::clone(&retained),
            });
            let metrics = crate::DiscoverConfig::builder(
                solti_model::AgentId::new("agent-1").unwrap(),
                "agent-1",
                crate::AgentEndpoint::new(
                    "http://127.0.0.1:8085",
                    crate::AgentEndpointType::Http,
                    1,
                )
                .unwrap(),
                crate::ControlPlaneEndpoint::new(
                    "http://127.0.0.1:9000",
                    crate::DiscoveryTransport::Http,
                )
                .unwrap(),
                solti_model::AgentCapabilities::default(),
                1_000,
                "hostile-metrics@1",
            )
            .with_metrics(raw)
            .build()
            .unwrap()
            .metrics;

            record_attempt(&metrics);
            record_success(&metrics, 1);
            record_failure(&metrics, 2, DiscoverFailReason::Timeout);
            record_hold(&metrics, 3);
            let retained_after_first_failure = Arc::strong_count(&retained);

            for _ in 0..1_024 {
                record_attempt(&metrics);
                record_success(&metrics, 1);
                record_failure(&metrics, 2, DiscoverFailReason::Timeout);
                record_hold(&metrics, 3);
            }

            assert_eq!(calls.load(Ordering::SeqCst), 1);
            assert_eq!(reports.load(Ordering::SeqCst), 1);
            assert_eq!(Arc::strong_count(&retained), retained_after_first_failure);
        }

        #[derive(Debug)]
        struct PanickingMetrics {
            calls: Arc<AtomicUsize>,
            retained: Arc<()>,
        }

        impl DiscoverMetricsBackend for PanickingMetrics {
            fn record_attempt(&self) {
                self.panic();
            }

            fn record_success(&self, _duration_ms: u64) {
                self.panic();
            }

            fn record_failure(&self, _duration_ms: u64, _reason: DiscoverFailReason) {
                self.panic();
            }

            fn record_hold(&self, _duration_s: u64) {
                self.panic();
            }
        }

        impl PanickingMetrics {
            fn panic(&self) {
                self.calls.fetch_add(1, Ordering::SeqCst);
                std::panic::panic_any(DestructorPanickingPayload(Arc::clone(&self.retained)));
            }
        }

        struct DestructorPanickingPayload(Arc<()>);

        impl Drop for DestructorPanickingPayload {
            fn drop(&mut self) {
                let _ = Arc::strong_count(&self.0);
                panic!("panic payload destructor");
            }
        }

        struct HostileSubscriber {
            reports: Arc<AtomicUsize>,
            retained: Arc<()>,
        }

        impl tracing::Subscriber for HostileSubscriber {
            fn enabled(&self, _metadata: &tracing::Metadata<'_>) -> bool {
                true
            }

            fn new_span(&self, _span: &tracing::span::Attributes<'_>) -> tracing::span::Id {
                tracing::span::Id::from_u64(1)
            }

            fn record(&self, _span: &tracing::span::Id, _values: &tracing::span::Record<'_>) {}

            fn record_follows_from(&self, _span: &tracing::span::Id, _follows: &tracing::span::Id) {
            }

            fn event(&self, _event: &tracing::Event<'_>) {
                self.reports.fetch_add(1, Ordering::SeqCst);
                std::panic::panic_any(DestructorPanickingPayload(Arc::clone(&self.retained)));
            }

            fn enter(&self, _span: &tracing::span::Id) {}

            fn exit(&self, _span: &tracing::span::Id) {}
        }
    }
}
