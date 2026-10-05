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

/// Calls the upstream under a 5 s deadline on a route with no timeout layer,
/// so only the client can stop the call. `?` gives the error's status.
#[get("/scoped")]
async fn scoped(client: Client) -> AutumnResult<String> {
    let response = Deadline::after(ROUTE_TIMEOUT)
        .scope(client.get("http://upstream/work").send())
        .await?;
    Ok(response.status().as_u16().to_string())
}

/// Calls the upstream with a deadline that has passed.
#[get("/late")]
async fn late(client: Client) -> AutumnResult<String> {
    let response = Deadline::at(tokio::time::Instant::now())
        .scope(client.get("http://upstream/work").send())
        .await?;
    Ok(response.status().as_u16().to_string())
}

/// Calls the upstream with its own deadline header.
#[get("/own-header", timeout_ms = 5000)]
async fn own_header(client: Client) -> String {
    match client
        .get("http://upstream/work")
        .header(DEADLINE_HEADER, "42")
        .send()
        .await
    {
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
    assert_eq!(millis, 5000, "virtual time: no time passed");

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

/// The start of each attempt, relative to `begin`.
fn offsets(starts: &Starts, begin: tokio::time::Instant) -> Vec<Duration> {
    starts
        .lock()
        .unwrap()
        .iter()
        .map(|start| start.duration_since(begin))
        .collect()
}

#[sim_test]
async fn sim_deadline_scope_alone_stops_a_hanging_upstream(mut sim: Sim) {
    let starts = Starts::default();
    sim.net(SimNet::new().host("upstream", hanging(starts.clone())));
    sim.build(TestApp::new().routes(routes![scoped]));

    let begin = tokio::time::Instant::now();
    let response = sim.client().get("/scoped").send().await;
    let elapsed = begin.elapsed();
    response.assert_status(504);
    assert!(elapsed <= ROUTE_TIMEOUT + EPSILON, "took {elapsed:?}");

    // Let any late attempt start, then check that none did.
    sim.advance(Duration::from_secs(60)).await;
    assert_eq!(offsets(&starts, begin), vec![Duration::ZERO], "one attempt");
}

#[sim_test]
async fn sim_deadline_scope_alone_starts_no_attempt_after_the_deadline(mut sim: Sim) {
    let starts = Starts::default();
    sim.net(SimNet::new().host("upstream", slow_failing(starts.clone())));
    sim.build(TestApp::new().routes(routes![scoped]));

    let begin = tokio::time::Instant::now();
    let response = sim.client().get("/scoped").send().await;
    let elapsed = begin.elapsed();
    response.assert_status(504);
    assert!(elapsed <= ROUTE_TIMEOUT + EPSILON, "took {elapsed:?}");

    sim.advance(Duration::from_secs(60)).await;
    // Each attempt takes 2 s, plus a jittered backoff. With no deadline all
    // 4 attempts run, and the last one starts after 6 s.
    let offsets = offsets(&starts, begin);
    assert!(offsets.len() >= 2, "the client retries: {offsets:?}");
    assert_eq!(offsets[0], Duration::ZERO);
    for start in &offsets {
        assert!(
            *start < ROUTE_TIMEOUT,
            "an attempt started late: {offsets:?}"
        );
    }
}

#[sim_test]
async fn sim_deadline_that_has_passed_sends_nothing_and_is_a_504(mut sim: Sim) {
    let starts = Starts::default();
    sim.net(SimNet::new().host("upstream", hanging(starts.clone())));
    sim.build(TestApp::new().routes(routes![late]));
    sim.client().get("/late").send().await.assert_status(504);
    assert!(starts.lock().unwrap().is_empty(), "no attempt");
}

/// An upstream that asks for a 10 s wait.
fn throttling(starts: Starts) -> axum::Router {
    axum::Router::new().route(
        "/work",
        axum::routing::get(move || {
            starts.lock().unwrap().push(tokio::time::Instant::now());
            async {
                (
                    axum::http::StatusCode::TOO_MANY_REQUESTS,
                    [("retry-after", "10")],
                    "slow down",
                )
            }
        }),
    )
}

#[sim_test]
async fn sim_deadline_skips_a_retry_after_wait_past_the_deadline(mut sim: Sim) {
    let starts = Starts::default();
    sim.net(SimNet::new().host("upstream", throttling(starts.clone())));
    sim.build(TestApp::new().routes(routes![call]));

    let begin = tokio::time::Instant::now();
    let response = sim.client().get("/call").send().await;
    assert_eq!(begin.elapsed(), Duration::ZERO, "no wait");
    response.assert_status(200);
    assert_eq!(response.text(), "slow down", "the 429 comes back");
    assert_eq!(starts.lock().unwrap().len(), 1);
}

#[sim_test]
async fn sim_deadline_header_keeps_the_callers_value(mut sim: Sim) {
    sim.net(SimNet::new().host("upstream", echo_deadline()));
    sim.build(TestApp::new().routes(routes![own_header]));
    assert_eq!(sim.client().get("/own-header").send().await.text(), "42");
}

#[sim_test]
async fn sim_deadline_header_can_be_turned_off(mut sim: Sim) {
    let mut config = autumn_web::config::AutumnConfig::default();
    config.http.client.send_deadline_header = false;
    sim.net(SimNet::new().host("upstream", echo_deadline()));
    sim.build(TestApp::new().routes(routes![call]).config(config));
    assert_eq!(sim.client().get("/call").send().await.text(), "none");
}

/// Reports the deadline that a spawned task sees.
#[get("/spawned", timeout_ms = 5000)]
async fn spawned() -> String {
    let seen = tokio::spawn(async { Deadline::current().is_some() })
        .await
        .unwrap();
    format!("spawned task sees a deadline: {seen}")
}

#[sim_test]
async fn sim_deadline_does_not_follow_tokio_spawn(mut sim: Sim) {
    sim.build(TestApp::new().routes(routes![spawned]));
    assert_eq!(
        sim.client().get("/spawned").send().await.text(),
        "spawned task sees a deadline: false"
    );
}
