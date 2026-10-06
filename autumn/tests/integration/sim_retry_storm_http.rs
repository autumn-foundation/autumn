//! Retry storm on the outbound HTTP client (issue #3054).
//!
//! N callers hit one failing upstream at the same virtual instant. Each
//! caller retries. With un-jittered backoff, all retries arrive at the same
//! instants (+100, +300, +700 ms) and load the upstream again at once. With
//! full jitter, the retries spread out.
//!
//! The upstream is a `SimNet` host. It records the virtual instant of each
//! request. The paused clock makes "same instant" exact, so the test is
//! deterministic for a seed.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use autumn_web::http_client::Client;
use autumn_web::prelude::*;
use autumn_web::sim::{Sim, SimNet};
use autumn_web::sim_test;
use autumn_web::test::TestApp;
use axum::http::{HeaderMap, StatusCode};

/// Number of callers that fail at the same instant.
const CALLERS: usize = 12;

/// One request as the upstream saw it.
#[derive(Clone, Debug)]
struct Hit {
    at: tokio::time::Instant,
    idempotency_key: Option<String>,
}

type Hits = Arc<Mutex<Vec<Hit>>>;

fn record(hits: &Hits, headers: &HeaderMap) {
    hits.lock().unwrap().push(Hit {
        at: tokio::time::Instant::now(),
        idempotency_key: headers
            .get("idempotency-key")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned),
    });
}

/// An upstream that always answers `503` with no `Retry-After`.
fn always_down(hits: Hits) -> axum::Router {
    let get_hits = hits.clone();
    axum::Router::new().route(
        "/x",
        axum::routing::get(move |headers: HeaderMap| {
            let hits = get_hits.clone();
            async move {
                record(&hits, &headers);
                StatusCode::SERVICE_UNAVAILABLE
            }
        })
        .post(move |headers: HeaderMap| {
            let hits = hits.clone();
            async move {
                record(&hits, &headers);
                StatusCode::SERVICE_UNAVAILABLE
            }
        }),
    )
}

/// An upstream that always answers `429` with `Retry-After: 1`.
fn always_limited(hits: Hits) -> axum::Router {
    axum::Router::new().route(
        "/x",
        axum::routing::get(move |headers: HeaderMap| {
            let hits = hits.clone();
            async move {
                record(&hits, &headers);
                (
                    StatusCode::TOO_MANY_REQUESTS,
                    [("retry-after", "1")],
                    "slow down",
                )
            }
        }),
    )
}

/// An upstream that answers `503` with `Retry-After: <secs>` once, then `200`.
fn down_once_with_retry_after(hits: Hits, secs: &'static str) -> axum::Router {
    axum::Router::new().route(
        "/x",
        axum::routing::get(move |headers: HeaderMap| {
            let hits = hits.clone();
            async move {
                record(&hits, &headers);
                if hits.lock().unwrap().len() == 1 {
                    (
                        StatusCode::SERVICE_UNAVAILABLE,
                        [("retry-after", secs)],
                        "down",
                    )
                        .into_response()
                } else {
                    (StatusCode::OK, "up").into_response()
                }
            }
        }),
    )
}

#[get("/call")]
async fn call_get(client: Client) -> String {
    match client.get("http://upstream/x").send().await {
        Ok(response) => response.status().as_u16().to_string(),
        Err(error) => format!("error: {error}"),
    }
}

#[get("/call-post")]
async fn call_post(client: Client) -> String {
    match client.post("http://upstream/x").retries(3).send().await {
        Ok(response) => response.status().as_u16().to_string(),
        Err(error) => format!("error: {error}"),
    }
}

#[get("/call-post-opt-in")]
async fn call_post_opt_in(client: Client) -> String {
    match client
        .post("http://upstream/x")
        .retries(3)
        .retry_non_idempotent()
        .send()
        .await
    {
        Ok(response) => response.status().as_u16().to_string(),
        Err(error) => format!("error: {error}"),
    }
}

#[get("/call-post-own-key")]
async fn call_post_own_key(client: Client) -> String {
    match client
        .post("http://upstream/x")
        .header("Idempotency-Key", "order-42")
        .retries(1)
        .retry_non_idempotent()
        .send()
        .await
    {
        Ok(response) => response.status().as_u16().to_string(),
        Err(error) => format!("error: {error}"),
    }
}

/// Run `CALLERS` concurrent calls to `path`, all at one virtual instant.
/// Every call must end on `status`.
async fn storm(sim: &Sim, path: &str, status: &str) {
    let calls = (0..CALLERS).map(|_| async move { sim.client().get(path).send().await.text() });
    for body in futures::future::join_all(calls).await {
        assert_eq!(body, status, "every call ends on the upstream's {status}");
    }
}

/// Fail when too many retries share one instant. Before #3054 the burst is
/// `CALLERS`. With full jitter a burst above `CALLERS / 2` is very unlikely
/// for any seed, so a seed sweep does not make this test flaky.
fn assert_spread(hits: &[Hit], seed: u64) {
    let burst = largest_retry_burst(hits);
    assert!(
        burst <= CALLERS / 2,
        "{burst} of {CALLERS} callers retried at the same instant (seed={seed:#x})",
    );
}

/// The largest number of hits that share one virtual instant, first
/// attempts excluded.
fn largest_retry_burst(hits: &[Hit]) -> usize {
    let first = hits
        .iter()
        .map(|hit| hit.at)
        .min()
        .expect("at least one hit");
    let mut per_instant: HashMap<tokio::time::Instant, usize> = HashMap::new();
    for hit in hits.iter().filter(|hit| hit.at > first) {
        *per_instant.entry(hit.at).or_default() += 1;
    }
    per_instant.into_values().max().unwrap_or(0)
}

/// AC1 (#3054): N callers that fail together must not retry together.
///
/// Before #3054 every retry lands on one of three instants, so the largest
/// burst is `CALLERS`. Full jitter spreads each retry over `[0, ceiling]` ms.
#[sim_test]
async fn sim_http_client_retries_spread_out(mut sim: Sim) {
    let hits: Hits = Arc::default();
    sim.net(SimNet::new().host("upstream", always_down(hits.clone())));
    sim.build(TestApp::new().routes(routes![call_get]));

    storm(&sim, "/call", "503").await;

    let hits = hits.lock().unwrap().clone();
    assert_eq!(hits.len(), CALLERS * 4, "1 attempt + 3 retries per caller");
    assert_spread(&hits, sim.seed);
}

/// The same `Retry-After` hint must not bring all callers back at once:
/// the jittered backoff stays in the wait.
#[sim_test]
async fn sim_http_client_429_retries_spread_out(mut sim: Sim) {
    let hits: Hits = Arc::default();
    sim.net(SimNet::new().host("upstream", always_limited(hits.clone())));
    sim.build(TestApp::new().routes(routes![call_get]));

    storm(&sim, "/call", "429").await;

    let hits = hits.lock().unwrap().clone();
    assert_eq!(hits.len(), CALLERS * 4, "1 attempt + 3 retries per caller");
    assert_spread(&hits, sim.seed);
    let first = hits.iter().map(|hit| hit.at).min().expect("hits");
    let earliest_retry = hits
        .iter()
        .map(|hit| hit.at)
        .filter(|at| *at > first)
        .min()
        .expect("retries");
    assert!(
        earliest_retry - first >= Duration::from_secs(1),
        "no retry comes before the 1 s hint"
    );
}

/// AC4 (#3054): a `503` with `Retry-After: 2` waits at least 2 s.
#[sim_test]
async fn sim_http_client_honours_retry_after_on_503(mut sim: Sim) {
    let hits: Hits = Arc::default();
    sim.net(SimNet::new().host("upstream", down_once_with_retry_after(hits.clone(), "2")));
    sim.build(TestApp::new().routes(routes![call_get]));

    assert_eq!(sim.client().get("/call").send().await.text(), "200");

    let hits = hits.lock().unwrap().clone();
    assert_eq!(hits.len(), 2);
    let waited = hits[1].at - hits[0].at;
    assert!(waited >= Duration::from_secs(2), "waited only {waited:?}");
}

/// A large `Retry-After` is cut to `backoff + 5 s`.
#[sim_test]
async fn sim_http_client_clamps_a_large_retry_after(mut sim: Sim) {
    let hits: Hits = Arc::default();
    sim.net(SimNet::new().host("upstream", down_once_with_retry_after(hits.clone(), "3600")));
    sim.build(TestApp::new().routes(routes![call_get]));

    assert_eq!(sim.client().get("/call").send().await.text(), "200");

    let hits = hits.lock().unwrap().clone();
    assert_eq!(hits.len(), 2);
    let waited = hits[1].at - hits[0].at;
    // First retry: backoff <= 100 ms, so the wait is <= 5.1 s.
    assert!(
        waited >= Duration::from_secs(5) && waited <= Duration::from_millis(5_100),
        "waited {waited:?}"
    );
}

/// AC3 (#3054): `retries(3)` on POST does not retry without the opt-in.
#[sim_test]
async fn sim_http_client_post_retries_need_opt_in(mut sim: Sim) {
    let hits: Hits = Arc::default();
    sim.net(SimNet::new().host("upstream", always_down(hits.clone())));
    sim.build(TestApp::new().routes(routes![call_post]));

    assert_eq!(sim.client().get("/call-post").send().await.text(), "503");
    assert_eq!(hits.lock().unwrap().len(), 1, "POST must not retry");
}

/// With the opt-in, POST retries and every attempt carries one
/// `Idempotency-Key`.
#[sim_test]
async fn sim_http_client_opted_in_post_retries_with_one_key(mut sim: Sim) {
    let hits: Hits = Arc::default();
    sim.net(SimNet::new().host("upstream", always_down(hits.clone())));
    sim.build(TestApp::new().routes(routes![call_post_opt_in]));

    assert_eq!(
        sim.client().get("/call-post-opt-in").send().await.text(),
        "503"
    );
    let hits = hits.lock().unwrap().clone();
    assert_eq!(hits.len(), 4, "1 attempt + 3 retries");
    let key = hits[0].idempotency_key.clone().expect("a key is sent");
    assert!(uuid::Uuid::parse_str(&key).is_ok(), "key {key} is a UUID");
    assert!(
        hits.iter()
            .all(|hit| hit.idempotency_key.as_ref() == Some(&key))
    );
}

/// A key the caller set is kept.
#[sim_test]
async fn sim_http_client_keeps_the_callers_idempotency_key(mut sim: Sim) {
    let hits: Hits = Arc::default();
    sim.net(SimNet::new().host("upstream", always_down(hits.clone())));
    sim.build(TestApp::new().routes(routes![call_post_own_key]));

    assert_eq!(
        sim.client().get("/call-post-own-key").send().await.text(),
        "503"
    );
    let hits = hits.lock().unwrap().clone();
    assert_eq!(hits.len(), 2);
    assert!(
        hits.iter()
            .all(|hit| hit.idempotency_key.as_deref() == Some("order-42"))
    );
}
