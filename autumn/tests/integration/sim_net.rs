//! Sim Phase 2 (issue #2967): the simulated network.
//!
//! A `SimNet` serves outbound `http_client::Client` calls from in-process
//! hosts. It adds seeded latency and drops, and it partitions hosts on demand.
//! Nothing reaches the real network.

use std::time::Duration;

use autumn_web::http_client::Client;
use autumn_web::prelude::*;
use autumn_web::sim::{NetFault, Sim, SimNet};
use autumn_web::sim_test;
use autumn_web::test::TestApp;

/// The remote service the app calls.
fn payments() -> axum::Router {
    axum::Router::new().route("/charge", axum::routing::get(|| async { "charged" }))
}

/// Calls the payments host and reports the result as text.
#[get("/pay")]
async fn pay(client: Client) -> String {
    match client.get("http://payments/charge").send().await {
        Ok(response) => format!("ok: {}", response.text()),
        Err(error) => format!("error: {error}"),
    }
}

/// Calls the payments host through a named client, which http mocks match.
#[get("/pay-named")]
async fn pay_named(client: Client) -> String {
    match client
        .named("payments")
        .get("http://payments/charge")
        .send()
        .await
    {
        Ok(response) => format!("ok: {}", response.text()),
        Err(error) => format!("error: {error}"),
    }
}

/// Calls a host the network does not know.
#[get("/elsewhere")]
async fn elsewhere(client: Client) -> String {
    match client.get("http://nowhere.invalid/x").send().await {
        Ok(response) => format!("ok: {}", response.text()),
        Err(error) => format!("error: {error}"),
    }
}

async fn call(sim: &Sim, path: &str) -> String {
    sim.client().get(path).send().await.text()
}

#[sim_test]
async fn sim_net_serves_a_registered_host(mut sim: Sim) {
    sim.net(SimNet::new().host("payments", payments()));
    sim.build(TestApp::new().routes(routes![pay]));
    assert_eq!(call(&sim, "/pay").await, "ok: charged");
}

#[sim_test]
async fn sim_net_unknown_host_fails_without_the_real_network(mut sim: Sim) {
    sim.net(SimNet::new());
    sim.build(TestApp::new().routes(routes![elsewhere]));
    let body = call(&sim, "/elsewhere").await;
    assert!(body.starts_with("error:"), "{body}");
    assert!(body.contains("nowhere.invalid"), "{body}");
}

#[sim_test]
async fn sim_net_adds_seeded_latency_in_virtual_time(mut sim: Sim) {
    let latency = Duration::from_millis(50);
    sim.net(
        SimNet::new()
            .host("payments", payments())
            .latency(latency, latency),
    );
    sim.build(TestApp::new().routes(routes![pay]));
    let start = tokio::time::Instant::now();
    assert_eq!(call(&sim, "/pay").await, "ok: charged");
    let elapsed = start.elapsed();
    assert!(
        elapsed >= latency && elapsed < latency * 2,
        "latency {elapsed:?}"
    );
}

#[sim_test]
async fn sim_net_partition_and_heal(mut sim: Sim) {
    let net = SimNet::new().host("payments", payments());
    sim.net(net.clone());
    sim.build(TestApp::new().routes(routes![pay]));

    net.partition("payments");
    let body = call(&sim, "/pay").await;
    assert!(body.contains("partition"), "{body}");

    net.heal("payments");
    assert_eq!(call(&sim, "/pay").await, "ok: charged");
    assert!(
        net.events()
            .iter()
            .any(|event| event.fault == NetFault::Partitioned),
        "the partition is in the event log"
    );
}

#[sim_test]
async fn sim_net_full_drop_rate_exhausts_the_retries(mut sim: Sim) {
    let net = SimNet::new().host("payments", payments()).drop_rate(1.0);
    sim.net(net.clone());
    sim.build(TestApp::new().routes(routes![pay]));
    let body = call(&sim, "/pay").await;
    assert!(body.contains("dropped"), "{body}");
    let events = net.events();
    assert!(events.len() > 1, "an idempotent GET is retried: {events:?}");
    assert!(events.iter().all(|event| event.fault == NetFault::Dropped));
}

/// Twenty calls through a lossy, slow network, and the event log they leave.
async fn lossy_run(seed: u64) -> (Vec<String>, Vec<autumn_web::sim::NetEvent>) {
    let mut sim = Sim::from_seed(seed);
    let net = SimNet::new()
        .host("payments", payments())
        .drop_rate(0.3)
        .latency(Duration::from_millis(1), Duration::from_millis(40));
    sim.net(net.clone());
    sim.build(TestApp::new().routes(routes![pay]));
    let mut bodies = Vec::new();
    for _ in 0..20 {
        bodies.push(call(&sim, "/pay").await);
    }
    (bodies, net.events())
}

#[sim_test]
async fn sim_net_same_seed_replays_faults_and_latency(sim: Sim) {
    let first = lossy_run(sim.seed).await;
    let again = lossy_run(sim.seed).await;
    assert_eq!(first, again, "the same seed replays the network");
    assert!(
        first.1.iter().any(|event| event.fault == NetFault::Dropped),
        "a 30% drop rate drops some of 20+ attempts"
    );
    let other = lossy_run(sim.seed.wrapping_add(1)).await;
    assert_ne!(first.1, other.1, "another seed gives another network");
}

#[sim_test]
async fn sim_net_falls_back_to_http_mocks(mut sim: Sim) {
    sim.net(SimNet::new().latency(Duration::from_millis(5), Duration::from_millis(5)));
    let mut app = TestApp::new().routes(routes![pay_named]);
    let _mock = app
        .http_mock("payments")
        .get("/charge")
        .respond_with(200, serde_json::json!("mocked"));
    sim.build(app);
    assert_eq!(call(&sim, "/pay-named").await, "ok: \"mocked\"");
}

/// A client that a state initializer built and stored.
#[derive(Clone)]
struct StoredClient(Client);

#[get("/pay-stored")]
async fn pay_stored(State(state): State<AppState>) -> String {
    let client = state.extension::<StoredClient>().expect("stored").0.clone();
    match client.get("http://payments/charge").send().await {
        Ok(response) => format!("ok: {}", response.text()),
        Err(error) => format!("error: {error}"),
    }
}

#[sim_test]
async fn sim_net_reaches_clients_built_in_state_initializers(mut sim: Sim) {
    sim.net(SimNet::new().host("payments", payments()));
    sim.build(
        TestApp::new()
            .routes(routes![pay_stored])
            .state_initializer(|state| {
                state.insert_extension(StoredClient(Client::from_state(state)));
            }),
    );
    assert_eq!(call(&sim, "/pay-stored").await, "ok: charged");
}

#[sim_test]
async fn sim_net_latency_past_the_request_timeout_is_a_timeout(mut sim: Sim) {
    // The default request timeout is 30 s, below this 60 s latency.
    let net = SimNet::new()
        .host("payments", payments())
        .latency(Duration::from_secs(60), Duration::from_secs(60));
    sim.net(net.clone());
    sim.build(TestApp::new().routes(routes![pay]));
    let body = call(&sim, "/pay").await;
    assert!(body.contains("timed out"), "{body}");
    let events = net.events();
    assert!(!events.is_empty());
    for event in &events {
        assert_eq!(event.fault, NetFault::TimedOut, "{events:?}");
        assert_eq!(
            event.latency,
            Duration::from_secs(30),
            "the time that passed"
        );
    }
}

/// Calls a relative path through a named client, with no base URL.
#[get("/pay-relative")]
async fn pay_relative(client: Client) -> String {
    match client.named("payments").get("/charge").send().await {
        Ok(response) => format!("ok: {}", response.text()),
        Err(error) => format!("error: {error}"),
    }
}

#[sim_test]
async fn sim_net_relative_url_on_a_named_client_reaches_its_mock(mut sim: Sim) {
    sim.net(SimNet::new());
    let mut app = TestApp::new().routes(routes![pay_relative]);
    let _mock = app
        .http_mock("payments")
        .get("/charge")
        .respond_with(200, serde_json::json!("mocked"));
    sim.build(app);
    assert_eq!(call(&sim, "/pay-relative").await, "ok: \"mocked\"");
}
