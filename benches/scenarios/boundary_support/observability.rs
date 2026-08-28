//! HTTP scrape assertions and untimed admission for observability fixtures.

use reqwest::{Client, StatusCode};
use solti_benches::fixtures::bounded;
use tokio::{sync::Notify, task::JoinHandle};

/// Completes one measured scrape without retrying a rejected request.
pub async fn scrape(client: Client, url: String, series: Option<usize>) {
    let response = bounded(client.get(url).send()).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = bounded(response.text()).await.unwrap();
    validate_exposition(&body, series);
}

/// Acquires a successful setup/recovery scrape outside the measured boundary.
///
/// Reading a response on the client is not a server-side slot-release barrier:
/// the transport can still own a response payload and its scrape permit.
/// Retry only capacity rejection, within one deadline covering every attempt.
pub async fn setup_scrape(client: Client, url: String) {
    bounded(async {
        loop {
            let response = client.get(&url).send().await.expect("setup scrape request");
            if response.status() == StatusCode::SERVICE_UNAVAILABLE {
                response.bytes().await.expect("setup rejection body");
                tokio::task::yield_now().await;
                continue;
            }
            assert_eq!(response.status(), StatusCode::OK, "setup scrape status");
            let body = response.text().await.expect("setup scrape exposition");
            validate_exposition(&body, None);
            return;
        }
    })
    .await;
}

/// Waits for physical collector entry while preserving an early scrape failure.
pub async fn wait_for_collector_entry(entered: &Notify, first: &mut JoinHandle<()>) {
    bounded(async {
        tokio::select! {
            () = entered.notified() => {}
            result = first => match result {
                Ok(()) => panic!("held setup scrape completed before collector entry"),
                Err(error) => panic!("held setup scrape failed before collector entry: {error}"),
            }
        }
    })
    .await;
}

fn validate_exposition(body: &str, series: Option<usize>) {
    assert!(body.contains("solti_bench_local_fixture 1"));
    assert!(body.ends_with('\n'));
    if let Some(series) = series {
        assert_eq!(
            body.lines()
                .filter(|line| line.starts_with("solti_bench_series{"))
                .count(),
            series
        );
    }
}
