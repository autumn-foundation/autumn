//! Inbound `X-Request-Id` propagation tests (issue #3064).
//!
//! A trusted inbound request id must reach:
//! - the `X-Request-Id` response header,
//! - the log events of the request,
//! - the `x-request-id` header of an outbound `http_client` call.
//!
//! An untrusted peer or a malformed id gets a new id.

use std::collections::HashMap;

use autumn_web::config::AutumnConfig;
use autumn_web::log::capture::{LogBuffer, LogCaptureLayer};
use autumn_web::log::filter::ParameterFilter;
use autumn_web::test::TestApp;
use autumn_web::{get, routes};
use tracing_subscriber::layer::SubscriberExt as _;

const INBOUND: &str = "0f8fad5b-d9cb-469f-a165-70867728950e";

/// Start a downstream server. It returns the `x-request-id` it received.
async fn start_downstream() -> String {
    let app = axum::Router::new().route(
        "/echo",
        axum::routing::get(|headers: http::HeaderMap| async move {
            let ids: Vec<String> = headers
                .get_all("x-request-id")
                .iter()
                .map(|v| v.to_str().unwrap().to_owned())
                .collect();
            ids.join(",")
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    format!("http://{addr}/echo")
}

type Query = axum::extract::Query<HashMap<String, String>>;

/// Call the downstream server at `?url=` and return what it received.
#[get("/call-downstream")]
async fn call_downstream(axum::extract::Query(query): Query) -> String {
    tracing::info!("calling downstream");
    autumn_web::http_client::Client::new()
        .get(&query["url"])
        .send()
        .await
        .unwrap()
        .text()
}

/// Same call, but the caller sets its own `x-request-id`.
#[get("/call-downstream-own-id")]
async fn call_downstream_own_id(axum::extract::Query(query): Query) -> String {
    autumn_web::http_client::Client::new()
        .get(&query["url"])
        .header("x-request-id", "caller-set")
        .send()
        .await
        .unwrap()
        .text()
}

/// Install a thread-local subscriber with the framework's log capture layer.
fn capture_logs() -> (LogBuffer, tracing::subscriber::DefaultGuard) {
    let buffer = LogBuffer::new(1000, ParameterFilter::new(&[], &[]));
    let subscriber = tracing_subscriber::registry().with(LogCaptureLayer::new(buffer.clone()));
    let guard = tracing::subscriber::set_default(subscriber);
    tracing::callsite::rebuild_interest_cache();
    (buffer, guard)
}

fn trusting_config() -> AutumnConfig {
    let mut config = AutumnConfig {
        profile: Some("test".into()),
        ..Default::default()
    };
    config.security.trusted_proxies.trust_forwarded_headers = true;
    config
}

fn logged_request_ids(buffer: &LogBuffer) -> Vec<String> {
    buffer
        .snapshot(None, None)
        .into_iter()
        .filter_map(|entry| entry.request_id)
        .collect()
}

#[tokio::test(flavor = "current_thread")]
async fn trusted_inbound_id_reaches_response_logs_and_outbound_call() {
    let (logs, _guard) = capture_logs();
    let downstream = start_downstream().await;
    let client = TestApp::new()
        .config(trusting_config())
        .routes(routes![call_downstream])
        .build();

    let response = client
        .get(&format!("/call-downstream?url={downstream}"))
        .header("x-request-id", INBOUND)
        .send()
        .await;
    response.assert_ok();

    assert_eq!(response.header("x-request-id"), Some(INBOUND), "response");
    assert_eq!(response.text(), INBOUND, "outbound request");
    let ids = logged_request_ids(&logs);
    assert!(!ids.is_empty(), "no log event carried a request id");
    assert!(
        ids.iter().all(|id| id == INBOUND),
        "log events carried another id: {ids:?}"
    );
}

#[tokio::test(flavor = "current_thread")]
async fn simple_hex_form_is_kept_as_sent() {
    let (_logs, _guard) = capture_logs();
    let inbound = "0f8fad5bd9cb469fa16570867728950e";
    let client = TestApp::new()
        .config(trusting_config())
        .routes(routes![call_downstream])
        .build();

    let response = client
        .get("/actuator/health")
        .header("x-request-id", inbound)
        .send()
        .await;
    assert_eq!(response.header("x-request-id"), Some(inbound));
}

#[tokio::test(flavor = "current_thread")]
async fn untrusted_peer_gets_a_new_id() {
    let (_logs, _guard) = capture_logs();
    // Default config: no forwarded-header trust.
    let client = TestApp::new().routes(routes![call_downstream]).build();

    let response = client
        .get("/actuator/health")
        .header("x-request-id", INBOUND)
        .send()
        .await;
    let id = response.header("x-request-id").unwrap();
    assert_ne!(id, INBOUND);
    assert!(uuid::Uuid::parse_str(id).is_ok(), "new id is a UUID: {id}");
}

#[tokio::test(flavor = "current_thread")]
async fn malformed_inbound_ids_get_a_new_id() {
    let (_logs, _guard) = capture_logs();
    let client = TestApp::new()
        .config(trusting_config())
        .routes(routes![call_downstream])
        .build();

    let too_long = "a".repeat(200);
    for bad in [
        "not-a-uuid",
        "",
        too_long.as_str(),
        "{0f8fad5b-d9cb-469f-a165-70867728950e}",
        "urn:uuid:0f8fad5b-d9cb-469f-a165-70867728950e",
    ] {
        let response = client
            .get("/actuator/health")
            .header("x-request-id", bad)
            .send()
            .await;
        let id = response.header("x-request-id").unwrap();
        assert_ne!(id, bad);
        assert!(uuid::Uuid::parse_str(id).is_ok(), "new id is a UUID: {id}");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn caller_header_wins_over_the_forwarded_id() {
    let (_logs, _guard) = capture_logs();
    let downstream = start_downstream().await;
    let client = TestApp::new()
        .config(trusting_config())
        .routes(routes![call_downstream_own_id])
        .build();

    let response = client
        .get(&format!("/call-downstream-own-id?url={downstream}"))
        .header("x-request-id", INBOUND)
        .send()
        .await;
    response.assert_ok();
    assert_eq!(response.text(), "caller-set", "one header, the caller's");
}
