//! Deadline propagation to outbound HTTP (issue #3058).
//!
//! A route with a 5 s timeout calls an upstream through `SimNet`. The client
//! must stop at the inbound deadline. It must not start an attempt after it.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use autumn_web::deadline::{DEADLINE_HEADER, Deadline};
use autumn_web::http_client::Client;
use autumn_web::prelude::*;
use autumn_web::sim::{Sim, SimNet};
use autumn_web::sim_test;
use autumn_web::test::TestApp;

const ROUTE_TIMEOUT: Duration = Duration::from_secs(5);
/// Tolerance for the end time. Virtual time is exact, so this is small.
const EPSILON: Duration = Duration::from_millis(10);

/// The time each attempt reached the upstream.
type Starts = Arc<Mutex<Vec<tokio::time::Instant>>>;

/// An upstream that records each attempt and never answers.
fn hanging(starts: Starts) -> axum::Router {
    axum::Router::new().route(
        "/work",
        axum::routing::get(move || {
            starts.lock().unwrap().push(tokio::time::Instant::now());
            std::future::pending::<&'static str>()
        }),
    )
}

/// An upstream that records each attempt and fails after 2 s.
fn slow_failing(starts: Starts) -> axum::Router {
    axum::Router::new().route(
        "/work",
        axum::routing::get(move || {
            starts.lock().unwrap().push(tokio::time::Instant::now());
            async {
                tokio::time::sleep(Duration::from_secs(2)).await;
                axum::http::StatusCode::SERVICE_UNAVAILABLE
            }
        }),
    )
}

/// An upstream that returns the deadline header it got, or "none".
fn echo_deadline() -> axum::Router {
    axum::Router::new().route(
        "/work",
        axum::routing::get(|headers: axum::http::HeaderMap| async move {
            headers
                .get(DEADLINE_HEADER)
                .and_then(|value| value.to_str().ok())
                .unwrap_or("none")
                .to_owned()
        }),
    )
}

/// Calls the upstream. A client error becomes a `502` with the error text,
/// so the test can see that the client stopped, not the timeout layer.
#[get("/call", timeout_ms = 5000)]
async fn call(client: Client) -> (axum::http::StatusCode, String) {
    match client.get("http://upstream/work").send().await {
        Ok(response) => (axum::http::StatusCode::OK, response.text()),
        Err(error) => (axum::http::StatusCode::BAD_GATEWAY, error.to_string()),
    }
}

/// The same call on a route with no timeout.
#[get("/call-unbounded")]
async fn call_unbounded(client: Client) -> String {
    match client.get("http://upstream/work").send().await {
        Ok(response) => response.text(),
        Err(error) => format!("error: {error}"),
    }
}

/// Reports the time left that the handler sees, in milliseconds.
#[get("/remaining", timeout_ms = 5000)]
async fn remaining() -> String {
    Deadline::current().map_or_else(
        || "none".to_owned(),
        |deadline| deadline.remaining().as_millis().to_string(),
    )
}

#[sim_test]
async fn sim_deadline_stops_a_hanging_upstream_at_the_route_timeout(mut sim: Sim) {
    let starts = Starts::default();
    sim.net(SimNet::new().host("upstream", hanging(starts.clone())));
    sim.build(TestApp::new().routes(routes![call]));

    let begin = tokio::time::Instant::now();
    let response = sim.client().get("/call").send().await;
    let elapsed = begin.elapsed();

    assert!(elapsed <= ROUTE_TIMEOUT + EPSILON, "took {elapsed:?}");
    assert_eq!(
        response.status.as_u16(),
        502,
        "the client must fail first: {}",
        response.text()
    );
    let starts = starts.lock().unwrap().clone();
    assert!(!starts.is_empty(), "the upstream got no attempt");
    for start in &starts {
        assert!(
            *start < begin + ROUTE_TIMEOUT,
            "an attempt started after the deadline"
        );
    }
}

#[sim_test]
async fn sim_deadline_retries_only_inside_the_route_timeout(mut sim: Sim) {
    let starts = Starts::default();
    sim.net(SimNet::new().host("upstream", slow_failing(starts.clone())));
    sim.build(TestApp::new().routes(routes![call]));

    let begin = tokio::time::Instant::now();
    let response = sim.client().get("/call").send().await;
    let elapsed = begin.elapsed();

    assert!(elapsed <= ROUTE_TIMEOUT + EPSILON, "took {elapsed:?}");
    assert_eq!(response.status.as_u16(), 502, "{}", response.text());
    let starts = starts.lock().unwrap().clone();
    assert!(starts.len() >= 2, "the client retries: {starts:?}");
    for start in &starts {
        assert!(*start < begin + ROUTE_TIMEOUT, "an attempt started late");
    }
}

#[sim_test]
async fn sim_deadline_sends_the_remaining_time_downstream(mut sim: Sim) {
    sim.net(SimNet::new().host("upstream", echo_deadline()));
    sim.build(TestApp::new().routes(routes![call, call_unbounded]));

    let response = sim.client().get("/call").send().await;
    let millis: u64 = response
        .text()
        .parse()
        .unwrap_or_else(|_| panic!("not a number: {}", response.text()));
    assert!(millis > 0 && millis <= 5000, "{millis}");

    let unbounded = sim.client().get("/call-unbounded").send().await;
    assert_eq!(unbounded.text(), "none", "no deadline, no header");
}

#[sim_test]
async fn sim_deadline_is_visible_to_the_handler(mut sim: Sim) {
    sim.build(TestApp::new().routes(routes![remaining]));
    let body = sim.client().get("/remaining").send().await.text();
    let millis: u64 = body.parse().unwrap_or_else(|_| panic!("{body}"));
    assert!(millis > 4_900 && millis <= 5_000, "{millis}");
}

/// A route that never answers inside its 100 ms timeout.
#[get("/stuck", timeout_ms = 100)]
async fn stuck() -> &'static str {
    std::future::pending().await
}

/// The `Retry-After` values of 20 timed-out requests on a fresh sim.
async fn retry_after_values(seed: u64) -> Vec<String> {
    let mut sim = Sim::from_seed(seed);
    sim.build(TestApp::new().routes(routes![stuck]));
    let mut values = Vec::new();
    for _ in 0..20 {
        let response = sim.client().get("/stuck").send().await;
        response.assert_status(503);
        values.push(
            response
                .header("retry-after")
                .expect("Retry-After")
                .to_owned(),
        );
    }
    values
}

#[sim_test]
async fn sim_timeout_retry_after_is_jittered_and_replays(sim: Sim) {
    let first = retry_after_values(sim.seed).await;
    assert_eq!(first, retry_after_values(sim.seed).await, "same seed");
    let distinct: std::collections::BTreeSet<_> = first.iter().collect();
    assert!(distinct.len() > 1, "no jitter: {first:?}");
}
