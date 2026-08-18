#![cfg(any(feature = "grpc", feature = "http"))]

use std::{
    fmt,
    sync::{Arc, Mutex},
};

use solti_api::ApiError;
use tracing::{
    Event, Metadata, Subscriber,
    field::{Field, Visit},
    span::{Attributes, Id, Record},
};

#[derive(Default)]
struct TraceCapture {
    fields: Mutex<Vec<String>>,
}

struct CaptureSubscriber(Arc<TraceCapture>);

impl Subscriber for CaptureSubscriber {
    fn enabled(&self, _metadata: &Metadata<'_>) -> bool {
        true
    }

    fn new_span(&self, _span: &Attributes<'_>) -> Id {
        Id::from_u64(1)
    }

    fn record(&self, _span: &Id, _values: &Record<'_>) {}

    fn record_follows_from(&self, _span: &Id, _follows: &Id) {}

    fn event(&self, event: &Event<'_>) {
        event.record(&mut CaptureVisitor(&self.0));
    }

    fn enter(&self, _span: &Id) {}

    fn exit(&self, _span: &Id) {}
}

struct CaptureVisitor<'a>(&'a TraceCapture);

impl Visit for CaptureVisitor<'_> {
    fn record_debug(&mut self, field: &Field, value: &dyn fmt::Debug) {
        self.0
            .fields
            .lock()
            .unwrap()
            .push(format!("{}={value:?}", field.name()));
    }
}

#[test]
fn internal_error_logging_does_not_record_diagnostic() {
    const SECRET: &str = "api-internal-credential-secret";
    const FORGED: &str = "forged-api-record";

    let capture = Arc::new(TraceCapture::default());
    let dispatch = tracing::Dispatch::new(CaptureSubscriber(Arc::clone(&capture)));
    let _guard = tracing::dispatcher::set_default(&dispatch);
    let diagnostic = format!("{SECRET}\n{FORGED}");

    #[cfg(feature = "grpc")]
    let _ = tonic::Status::from(ApiError::Internal(diagnostic.clone()));
    #[cfg(feature = "http")]
    let _ = axum::response::IntoResponse::into_response(ApiError::Internal(diagnostic));

    let fields = capture.fields.lock().unwrap().join(" ");
    assert!(fields.contains("api.internal_error"), "{fields}");
    assert!(fields.contains("error_kind=\"internal\""), "{fields}");
    #[cfg(feature = "grpc")]
    assert!(fields.contains("transport=\"grpc\""), "{fields}");
    #[cfg(feature = "http")]
    assert!(fields.contains("transport=\"http\""), "{fields}");
    assert!(!fields.contains(SECRET), "{fields}");
    assert!(!fields.contains(FORGED), "{fields}");
}
