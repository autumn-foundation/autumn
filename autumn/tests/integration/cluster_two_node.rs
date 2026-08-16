//! Two-node cluster tests over the public surface (issue #1762).
//!
//! Layering (see `docs/guide/clustering.md`):
//!
//! - **L2** — exactly one real-socket existence proof per behaviour:
//!   `TcpPeerTransport` bound on `127.0.0.1:0`, the second node seeded from the
//!   first node's observed `local_addr()`. Assertions are about converged
//!   state, polled with the house `poll_until` convention; never about message
//!   counts and never behind a fixed sleep.
//! - **L3** — one full-app vertical: two in-process apps, `[cluster]` config
//!   structs built directly (never environment mutation), health observed over
//!   real HTTP with the no-`reqwest` `TcpStream` helper.
//! - A guard proving a disabled `[cluster]` section installs literally nothing.
//!
//! The tight-timing coverage (suspicion thresholds, refutation, replay) lives
//! in the deterministic virtual-clock suite in `src/cluster/tests.rs`; these
//! socket tests deliberately keep loose intervals so they cannot flake on a
//! slow Windows runner.

use std::net::SocketAddr;
use std::time::Duration;

use autumn_web::cluster::{ClusterHandle, install_from_config};
use autumn_web::config::{AutumnConfig, ClusterConfig};
use autumn_web::test::TestApp;
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::{TcpListener, TcpStream};
use tokio_util::sync::CancellationToken;

const SECRET: &str = "a-shared-cluster-secret-value-32";
const COUNTER: &str = "boids_sighted";

/// Generous outer bound for every convergence poll. Real convergence takes a
/// few push intervals (200 ms each below); this is a ceiling, not a wait.
const CONVERGE_TIMEOUT: Duration = Duration::from_secs(5);

/// Poll `condition` every 10 ms until it returns `true` or `timeout` elapses.
///
/// Replaces fixed `tokio::time::sleep` calls that are fragile on slow CI
/// runners (especially Windows): the condition is checked as soon as the
/// background work finishes rather than waiting a worst-case wall-clock time.
async fn poll_until<F, Fut>(timeout: Duration, mut condition: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + timeout;
    loop {
        if condition().await {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            // Let the assertion that follows produce the real failure message.
            return;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

/// An enabled `[cluster]` section on an ephemeral loopback port.
///
/// Intervals are short enough to converge quickly and still satisfy the
/// `suspicion_timeout_ms >= 3 x push_interval_ms` validation rule.
fn cluster_config(seed_peers: Vec<String>) -> ClusterConfig {
    ClusterConfig {
        enabled: true,
        secret: Some(secrecy::SecretString::from(SECRET.to_owned())),
        bind_addr: "127.0.0.1:0".to_owned(),
        seed_peers,
        push_interval_ms: 200,
        suspicion_timeout_ms: 1_000,
        ..ClusterConfig::default()
    }
}

fn app_config(cluster: ClusterConfig) -> AutumnConfig {
    AutumnConfig {
        cluster,
        // `/actuator/health` renders a component's `details` map only when
        // `health.detailed` is on — the `dev` profile turns it on, the bare
        // `AutumnConfig::default()` these tests build does not. Without this
        // the membership component still appears (status `UP`) but with no
        // details at all, and every assertion below would be reading `null`.
        health: autumn_web::config::HealthConfig {
            detailed: true,
            ..autumn_web::config::HealthConfig::default()
        },
        ..AutumnConfig::default()
    }
}

fn member_ids(handle: &ClusterHandle) -> Vec<String> {
    let mut ids: Vec<String> = handle.members().into_iter().map(|m| m.id).collect();
    ids.sort();
    ids
}

/// Poll until both nodes report a two-member view, then assert it.
///
/// Asserting on both is the point: an asymmetric view — A sees the pair, B sees
/// only itself — is a real convergence bug that a one-sided assertion passes.
async fn assert_converged(a: &ClusterHandle, b: &ClusterHandle) {
    let (left, right) = (a.clone(), b.clone());
    poll_until(CONVERGE_TIMEOUT, || {
        let (left, right) = (left.clone(), right.clone());
        async move { left.members().len() == 2 && right.members().len() == 2 }
    })
    .await;

    assert_eq!(
        member_ids(a).len(),
        2,
        "node A must converge on a two-member view; {} | {}",
        describe("A", a),
        describe("B", b)
    );
    assert_eq!(
        member_ids(b),
        member_ids(a),
        "both nodes must converge on the same view; {} | {}",
        describe("A", a),
        describe("B", b)
    );
}

fn describe(label: &str, handle: &ClusterHandle) -> String {
    format!(
        "{label}: members={:?} {COUNTER}={}",
        handle.members(),
        handle.counter(COUNTER).get()
    )
}

/// Serve an app router on an ephemeral loopback port; returns its address.
async fn serve(router: axum::Router, shutdown: CancellationToken) -> SocketAddr {
    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind an ephemeral HTTP port");
    let addr = listener.local_addr().expect("local_addr");
    tokio::spawn(async move {
        axum::serve(listener, router)
            .with_graceful_shutdown(shutdown.cancelled_owned())
            .await
            .ok();
    });
    addr
}

/// Hand-written HTTP/1.1 GET over raw TCP — `reqwest` is deliberately not a
/// dev-dependency of this workspace.
async fn http_get(addr: SocketAddr, path: &str) -> String {
    let Ok(mut stream) = TcpStream::connect(addr).await else {
        return String::new();
    };
    let request = format!("GET {path} HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n");
    if stream.write_all(request.as_bytes()).await.is_err() {
        return String::new();
    }
    let mut buf = Vec::new();
    if stream.read_to_end(&mut buf).await.is_err() {
        return String::new();
    }
    String::from_utf8_lossy(&buf).into_owned()
}

/// The cluster membership facts a `/actuator/health` body reports, as
/// `(details.member_count, details.members.len())`.
///
/// Both halves are asserted together on purpose: the guide defines
/// `member_count` as the number and `members` as the array of
/// `{id, addr, status, incarnation}` rows, and a count that disagrees with the
/// array it summarises is exactly the bug this pair catches. `(0, 0)` when the
/// component is absent, so a missing indicator fails as a mismatch rather than
/// as a panic.
fn membership(health: &serde_json::Value) -> (u64, usize) {
    let details = &health["components"]["cluster:membership"]["details"];
    (
        details["member_count"].as_u64().unwrap_or(0),
        details["members"].as_array().map_or(0, Vec::len),
    )
}

/// Assert the `cluster:membership` details map is **exactly** the object the
/// guide publishes: `{node_id, cluster, local_addr, member_count, members}`
/// with member rows of `{id, addr, status, incarnation}`.
///
/// `ClusterHealthIndicator::snapshot`'s rustdoc promises "the details map,
/// exactly as documented", and operators write `jq` against that shape — but
/// `member_count` and the array length are all any other assertion here reads,
/// so a renamed row key or a change in the `status` casing would sail through
/// the whole suite and break every dashboard built on the guide.
fn assert_documented_details_shape(health: &serde_json::Value, expected_node_id: &str) {
    let details = &health["components"]["cluster:membership"]["details"];
    let object = details.as_object().unwrap_or_else(|| {
        panic!("cluster:membership must publish a details object; got {health}")
    });

    let mut keys: Vec<&str> = object.keys().map(String::as_str).collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        vec![
            "cluster",
            "local_addr",
            "member_count",
            "members",
            "node_id"
        ],
        "the details map must carry exactly the documented keys; got {details}"
    );
    assert_eq!(
        details["node_id"].as_str(),
        Some(expected_node_id),
        "node_id must name this node; got {details}"
    );
    assert!(
        details["cluster"].as_str().is_some_and(|s| !s.is_empty()),
        "cluster must be the cluster name as a string; got {details}"
    );
    assert!(
        details["local_addr"]
            .as_str()
            .is_some_and(|addr| addr.parse::<SocketAddr>().is_ok()),
        "local_addr must be a dialable host:port string; got {details}"
    );

    let rows = details["members"]
        .as_array()
        .unwrap_or_else(|| panic!("members must be an array of rows; got {details}"));
    assert!(
        !rows.is_empty(),
        "a node is always in its own view, so there is always at least one row; got {details}"
    );
    for row in rows {
        let row_object = row
            .as_object()
            .unwrap_or_else(|| panic!("each member row must be an object; got {details}"));
        let mut row_keys: Vec<&str> = row_object.keys().map(String::as_str).collect();
        row_keys.sort_unstable();
        assert_eq!(
            row_keys,
            vec!["addr", "id", "incarnation", "status"],
            "each member row must carry exactly {{id, addr, status, incarnation}}; got {details}"
        );
        assert!(
            row["id"].as_str().is_some_and(|id| !id.is_empty()),
            "a row's id must be a non-empty string; got {details}"
        );
        assert!(
            row["addr"].is_string(),
            "a row's addr must be a string; got {details}"
        );
        assert!(
            matches!(row["status"].as_str(), Some("alive" | "suspect")),
            "a row's status must be the documented lowercase \"alive\"/\"suspect\" \
             — a member this node considers down is not in the view at all; got {details}"
        );
        assert!(
            row["incarnation"].as_u64().is_some(),
            "a row's incarnation must be a NUMBER, not a string; got {details}"
        );
    }
    assert_eq!(
        details["member_count"].as_u64(),
        u64::try_from(rows.len()).ok(),
        "member_count must be the number the members array summarises; got {details}"
    );
}

/// The JSON body of an HTTP/1.1 response, or `Null` when there isn't one.
fn json_body(response: &str) -> serde_json::Value {
    response
        .split_once("\r\n\r\n")
        .map(|(_, body)| body)
        .and_then(|body| serde_json::from_str(body).ok())
        .unwrap_or(serde_json::Value::Null)
}

// ── L2: real sockets ─────────────────────────────────────────────────────────

/// AC1 + AC3 + AC4 + AC6 over real sockets: two nodes with a shared secret and
/// no external datastore discover each other and replicate the counter.
#[tokio::test(flavor = "multi_thread")]
async fn tcp_two_nodes_converge_and_counter_replicates() {
    let shutdown = CancellationToken::new();

    let app_a = TestApp::new()
        .config(app_config(cluster_config(Vec::new())))
        .build();
    let config_a = cluster_config(Vec::new());
    install_from_config(app_a.state(), &config_a, &shutdown).expect("node A must install");
    let handle_a = app_a
        .state()
        .extension::<ClusterHandle>()
        .expect("an enabled [cluster] section must install a ClusterHandle on node A");

    // Seed B from A's OBSERVED bound address — no hardcoded ports anywhere.
    let seed = handle_a.local_addr().to_string();
    let config_b = cluster_config(vec![seed.clone()]);
    let app_b = TestApp::new().config(app_config(config_b.clone())).build();
    install_from_config(app_b.state(), &config_b, &shutdown).expect("node B must install");
    let handle_b = app_b
        .state()
        .extension::<ClusterHandle>()
        .expect("an enabled [cluster] section must install a ClusterHandle on node B");

    assert_converged(&handle_a, &handle_b).await;
    assert!(
        member_ids(&handle_a).contains(&handle_b.node_id().to_owned()),
        "A's view must name B itself, not merely count two rows (seeded at \
         {seed}); {} | {}",
        describe("A", &handle_a),
        describe("B", &handle_b)
    );

    // AC3: a write on A is observable on B.
    handle_a.counter(COUNTER).increment_by(3);
    let b = handle_b.clone();
    poll_until(CONVERGE_TIMEOUT, || {
        let b = b.clone();
        async move { b.counter(COUNTER).get() == 3 }
    })
    .await;

    assert_eq!(
        handle_b.counter(COUNTER).get(),
        3,
        "three increments on A must be readable on B; {} | {}",
        describe("A", &handle_a),
        describe("B", &handle_b)
    );

    shutdown.cancel();
}

/// AC5 + AC6 over real sockets: when one node goes away the survivor keeps
/// serving and converges to a one-member view.
#[tokio::test(flavor = "multi_thread")]
async fn tcp_survivor_converges_after_peer_cancelled() {
    let shutdown_a = CancellationToken::new();
    let shutdown_b = CancellationToken::new();

    let config_a = cluster_config(Vec::new());
    let app_a = TestApp::new().config(app_config(config_a.clone())).build();
    install_from_config(app_a.state(), &config_a, &shutdown_a).expect("node A must install");
    let handle_a = app_a
        .state()
        .extension::<ClusterHandle>()
        .expect("node A must expose a ClusterHandle");

    let config_b = cluster_config(vec![handle_a.local_addr().to_string()]);
    let app_b = TestApp::new().config(app_config(config_b.clone())).build();
    install_from_config(app_b.state(), &config_b, &shutdown_b).expect("node B must install");
    let handle_b = app_b
        .state()
        .extension::<ClusterHandle>()
        .expect("node B must expose a ClusterHandle");

    // Converged before the kill, or the test proves nothing.
    assert_converged(&handle_a, &handle_b).await;

    handle_a.counter(COUNTER).increment();
    shutdown_b.cancel();

    let a = handle_a.clone();
    poll_until(CONVERGE_TIMEOUT, || {
        let a = a.clone();
        async move { a.members().len() == 1 }
    })
    .await;

    assert_eq!(
        member_ids(&handle_a).len(),
        1,
        "the survivor must converge to a one-member view; {}",
        describe("A", &handle_a)
    );

    handle_a.counter(COUNTER).increment();
    assert_eq!(
        handle_a.counter(COUNTER).get(),
        2,
        "the survivor must keep serving the counter after its peer left; {}",
        describe("A", &handle_a)
    );

    shutdown_a.cancel();
    drop(handle_b);
}

/// The clean-leave FAST path over real sockets: cancellation must converge the
/// survivor long before the suspicion timeout could have.
///
/// The bug this pins is a shutdown-ordering one, invisible to every other test
/// here because they all give the suspicion timeout room to finish the job:
/// when the per-peer writer tasks are cancelled by the same token that triggers
/// the departure, the writers are gone before the `Leave` they exist to carry
/// is queued, and the clean path silently degrades into the timeout. So the
/// suspicion timeout is set to **six seconds** and convergence is required
/// inside a fraction of it — the assertion is the *margin*, not the outcome.
#[tokio::test(flavor = "multi_thread")]
async fn tcp_clean_leave_converges_before_the_suspicion_timeout() {
    /// Long enough that reaching it would take the test far past its poll
    /// budget, so a convergence observed below can only have come from a
    /// delivered `Leave`.
    const SUSPICION: Duration = Duration::from_secs(6);
    /// A generous fraction of that: a leave crosses loopback in microseconds.
    const LEAVE_BUDGET: Duration = Duration::from_secs(2);

    let shutdown_a = CancellationToken::new();
    let shutdown_b = CancellationToken::new();

    let config_a = ClusterConfig {
        suspicion_timeout_ms: 6_000,
        ..cluster_config(Vec::new())
    };
    let app_a = TestApp::new().config(app_config(config_a.clone())).build();
    install_from_config(app_a.state(), &config_a, &shutdown_a).expect("node A must install");
    let handle_a = app_a
        .state()
        .extension::<ClusterHandle>()
        .expect("node A must expose a ClusterHandle");

    let config_b = ClusterConfig {
        suspicion_timeout_ms: 6_000,
        ..cluster_config(vec![handle_a.local_addr().to_string()])
    };
    let app_b = TestApp::new().config(app_config(config_b.clone())).build();
    install_from_config(app_b.state(), &config_b, &shutdown_b).expect("node B must install");
    let handle_b = app_b
        .state()
        .extension::<ClusterHandle>()
        .expect("node B must expose a ClusterHandle");

    // Converged before the leave, or this test proves nothing.
    assert_converged(&handle_a, &handle_b).await;

    let departed_at = tokio::time::Instant::now();
    shutdown_b.cancel();

    let a = handle_a.clone();
    poll_until(LEAVE_BUDGET, || {
        let a = a.clone();
        async move { a.members().len() == 1 }
    })
    .await;
    let converged_after = departed_at.elapsed();

    assert_eq!(
        member_ids(&handle_a).len(),
        1,
        "a clean cancellation must deliver B's leave and converge A within \
         {}ms — the suspicion timeout is {}ms away, so falling back to it means \
         the leave never reached the wire; {}",
        LEAVE_BUDGET.as_millis(),
        SUSPICION.as_millis(),
        describe("A", &handle_a)
    );
    assert!(
        converged_after < SUSPICION,
        "convergence took {}ms, at or past the {}ms suspicion timeout: that is \
         the slow path, not the leave",
        converged_after.as_millis(),
        SUSPICION.as_millis()
    );

    shutdown_a.cancel();
    drop(handle_b);
}

// ── L3: the full-app vertical ────────────────────────────────────────────────

/// The whole stack, end to end: two in-process apps built from `[cluster]`
/// config structs, the membership visible on `/actuator/health` over real HTTP,
/// the counter written on A and read on B, and A's departure leaving B UP with
/// a one-member view (never DOWN — a liveness probe must not be able to kill
/// the survivor).
#[tokio::test(flavor = "multi_thread")]
async fn full_app_two_nodes_health_and_counter_via_http() {
    let shutdown_cluster_a = CancellationToken::new();
    let shutdown_cluster_b = CancellationToken::new();
    let http_shutdown = CancellationToken::new();

    let config_a = cluster_config(Vec::new());
    let app_a = TestApp::new().config(app_config(config_a.clone())).build();
    let state_a = app_a.state().clone();
    install_from_config(&state_a, &config_a, &shutdown_cluster_a).expect("node A must install");
    let handle_a = state_a
        .extension::<ClusterHandle>()
        .expect("node A must expose a ClusterHandle");

    let config_b = cluster_config(vec![handle_a.local_addr().to_string()]);
    let app_b = TestApp::new().config(app_config(config_b.clone())).build();
    let state_b = app_b.state().clone();
    install_from_config(&state_b, &config_b, &shutdown_cluster_b).expect("node B must install");
    let handle_b = state_b
        .extension::<ClusterHandle>()
        .expect("node B must expose a ClusterHandle");

    let http_a = serve(app_a.into_router(), http_shutdown.clone()).await;
    let http_b = serve(app_b.into_router(), http_shutdown.clone()).await;

    // AC1: the two-member view is a curl-able fact on BOTH nodes.
    poll_until(CONVERGE_TIMEOUT, || async move {
        let a = json_body(&http_get(http_a, "/actuator/health").await);
        let b = json_body(&http_get(http_b, "/actuator/health").await);
        membership(&a) == (2, 2) && membership(&b) == (2, 2)
    })
    .await;

    let health_a = json_body(&http_get(http_a, "/actuator/health").await);
    let health_b = json_body(&http_get(http_b, "/actuator/health").await);
    assert_eq!(
        membership(&health_a),
        (2, 2),
        "/actuator/health on node A must report a two-member cluster, count and rows \
         agreeing; got {health_a}"
    );
    assert_eq!(
        membership(&health_b),
        (2, 2),
        "/actuator/health on node B must report a two-member cluster, count and rows \
         agreeing; got {health_b}"
    );
    assert_eq!(
        health_a["components"]["cluster:membership"]["status"], "UP",
        "the cluster:membership indicator must report UP; got {health_a}"
    );
    // …and the whole documented JSON shape, not just the two numbers above.
    assert_documented_details_shape(&health_a, handle_a.node_id());
    assert_documented_details_shape(&health_b, handle_b.node_id());

    // AC3: written on A through the extension handle, read on B.
    handle_a.counter(COUNTER).increment();
    let b = handle_b.clone();
    poll_until(CONVERGE_TIMEOUT, || {
        let b = b.clone();
        async move { b.counter(COUNTER).get() == 1 }
    })
    .await;
    assert_eq!(
        handle_b.counter(COUNTER).get(),
        1,
        "the increment on node A must be readable on node B; {} | {}",
        describe("A", &handle_a),
        describe("B", &handle_b)
    );

    // AC5: A departs; B stays UP and converges to one member.
    shutdown_cluster_a.cancel();
    poll_until(CONVERGE_TIMEOUT, || async move {
        membership(&json_body(&http_get(http_b, "/actuator/health").await)) == (1, 1)
    })
    .await;

    let survivor = json_body(&http_get(http_b, "/actuator/health").await);
    assert_eq!(
        membership(&survivor),
        (1, 1),
        "node B must converge to a one-member view after A leaves; got {survivor}"
    );
    assert_eq!(
        survivor["components"]["cluster:membership"]["status"], "UP",
        "a one-member view is HEALTHY: reporting DOWN would let a liveness probe \
         restart the last surviving node; got {survivor}"
    );
    // The shape must survive the departure too: a one-member view is the shape
    // an operator reads most often, on exactly the node they are worried about.
    assert_documented_details_shape(&survivor, handle_b.node_id());
    assert_eq!(
        handle_b.counter(COUNTER).get(),
        1,
        "the survivor must keep serving the counter; {}",
        describe("B", &handle_b)
    );

    shutdown_cluster_b.cancel();
    http_shutdown.cancel();
}

// ── Guard: the installer validates what it is handed ─────────────────────────

/// `install_from_config` is public, so it is reachable with a `[cluster]`
/// section that never went through `AutumnConfig::validate` — a hand-built
/// struct, exactly as these tests build one. It must therefore run the same
/// rules itself, before it binds anything, and fail closed on a missing secret
/// instead of authenticating every peer on the port with an empty key.
#[tokio::test]
async fn install_rejects_an_invalid_or_secretless_section() {
    let app = TestApp::new()
        .config(app_config(ClusterConfig::default()))
        .build();
    let shutdown = CancellationToken::new();

    let secretless = ClusterConfig {
        secret: None,
        ..cluster_config(Vec::new())
    };
    // A section that would flap: one delayed push evicts a healthy peer.
    let flapping = ClusterConfig {
        suspicion_timeout_ms: 300,
        ..cluster_config(Vec::new())
    };
    // A seed peer nobody can dial.
    let unseedable = cluster_config(vec!["127.0.0.1:0".to_owned()]);

    for (label, config) in [
        ("an enabled section with no secret", secretless),
        ("a suspicion timeout below 3x the push interval", flapping),
        ("a seed peer on port 0", unseedable),
    ] {
        let result = install_from_config(app.state(), &config, &shutdown);
        assert!(
            result.is_err(),
            "{label} must be refused by the installer, not carried into the \
             cluster; got {result:?}"
        );
    }

    assert!(
        app.state().extension::<ClusterHandle>().is_none(),
        "a refused install must leave no ClusterHandle behind"
    );

    shutdown.cancel();
}

/// One node per app.
///
/// A second install on the same state binds a second listener and starts a
/// second set of loops, while the health component and the metrics source keep
/// the registrations of the first — so the actuator would describe node 1 while
/// `state.extension::<ClusterHandle>()` handed application code node 2. Two
/// nodes in one *process* stay legal (the tests above do exactly that); they
/// have an `AppState` each.
#[tokio::test]
async fn install_refuses_a_second_node_on_one_state() {
    let config = cluster_config(Vec::new());
    let app = TestApp::new().config(app_config(config.clone())).build();
    let shutdown = CancellationToken::new();

    install_from_config(app.state(), &config, &shutdown).expect("the first install must succeed");
    let first = app
        .state()
        .extension::<ClusterHandle>()
        .expect("the first install must leave a handle")
        .node_id()
        .to_owned();

    let second = install_from_config(app.state(), &config, &shutdown);
    assert!(
        second.is_err(),
        "installing a second cluster node on one AppState must fail loudly; got {second:?}"
    );
    assert_eq!(
        app.state()
            .extension::<ClusterHandle>()
            .map(|handle| handle.node_id().to_owned()),
        Some(first),
        "the refused install must not replace the running node's handle"
    );

    shutdown.cancel();
}

/// The cluster's actuator surface is not optional.
///
/// If the app has already claimed `cluster:membership` for a health indicator
/// **or** a metrics source of its own, the cluster's registration is rejected.
/// Warn and carry on would leave the node binding, gossiping and resolving
/// through the extension while `/actuator/health` and `/actuator/metrics`
/// described somebody else's component — so an operator watching the membership
/// view or `autumn_cluster_frames_rejected_total` would see nothing at all
/// while this node was partitioned or under attack. Boot must fail instead, no
/// node may be left running behind the failure, and — the half-install case —
/// the registry the app did *not* claim must come out of it untouched.
///
/// Neither registry can unregister, so the cluster has to look before it leaps:
/// registering the health indicator and only then colliding on the metrics
/// source would strand an always-`UP` indicator describing a cancelled node,
/// squatting on the very name the operator's retry needs.
#[tokio::test]
async fn install_refuses_a_collision_on_the_membership_component_name() {
    struct Impostor;
    impl autumn_web::actuator::HealthIndicator for Impostor {
        fn check(&self) -> futures::future::BoxFuture<'_, autumn_web::actuator::HealthCheckOutput> {
            Box::pin(std::future::ready(
                autumn_web::actuator::HealthCheckOutput::up(),
            ))
        }
    }
    impl autumn_web::actuator::MetricsSource for Impostor {
        fn collect(&self) -> Vec<autumn_web::actuator::MetricFamily> {
            Vec::new()
        }
    }

    // `true` = the app claims the health name (the cluster collides on its
    // first registration), `false` = it claims the metrics name (the cluster
    // collides on its second — the case that used to leave residue behind).
    for app_claims_health in [true, false] {
        // A port we know is free, so a leaked node is observable as a port that
        // never comes back.
        let probe = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind a probe port");
        let bind_addr = probe.local_addr().expect("probe local_addr");
        drop(probe);

        let config = ClusterConfig {
            bind_addr: bind_addr.to_string(),
            ..cluster_config(Vec::new())
        };
        let app = TestApp::new().config(app_config(config.clone())).build();
        let shutdown = CancellationToken::new();

        if app_claims_health {
            app.state()
                .health_indicator_registry()
                .register(
                    "cluster:membership",
                    autumn_web::actuator::IndicatorGroup::Readiness,
                    std::sync::Arc::new(Impostor),
                )
                .expect("the app must be able to register its own indicator first");
        } else {
            app.state()
                .metrics_source_registry()
                .register("cluster:membership", std::sync::Arc::new(Impostor))
                .expect("the app must be able to register its own source first");
        }

        let result = install_from_config(app.state(), &config, &shutdown);
        assert!(
            result.is_err(),
            "installing over an app-registered cluster:membership component \
             (health = {app_claims_health}) must fail the boot, not warn and \
             run unobservably; got {result:?}"
        );
        let message = result
            .err()
            .map(|error| error.to_string())
            .unwrap_or_default();
        assert!(
            message.contains("cluster:membership"),
            "the error must name the colliding registration so an operator can \
             rename theirs; got {message:?}"
        );
        assert!(
            app.state().extension::<ClusterHandle>().is_none(),
            "a refused install must leave no ClusterHandle behind"
        );

        // No residue: after the refusal exactly the app's own registration is
        // there and nothing of the cluster's, so the operator's retry — after
        // renaming their component — finds the name free in both registries.
        assert_eq!(
            app.state()
                .health_indicator_registry()
                .contains("cluster:membership"),
            app_claims_health,
            "a refused install must leave the health registry exactly as it \
             found it: registering the indicator and then failing on the \
             metrics name strands an always-UP component describing a \
             cancelled node, and blocks the retry that fixes the collision"
        );
        assert_eq!(
            app.state()
                .metrics_source_registry()
                .contains("cluster:membership"),
            !app_claims_health,
            "a refused install must leave the metrics registry exactly as it \
             found it"
        );

        // …and no node may be left running: the listener has to give the port
        // back (it is never started at all now that both names are checked
        // first, which is the same observation from the operator's side).
        poll_until(CONVERGE_TIMEOUT, || async move {
            TcpListener::bind(bind_addr).await.is_ok()
        })
        .await;
        assert!(
            TcpListener::bind(bind_addr).await.is_ok(),
            "a refused install must leave nothing on {bind_addr} — a listener \
             still holding it means a whole node is gossiping behind a boot \
             that reported failure"
        );

        shutdown.cancel();
    }
}

// ── Guard: off means off ─────────────────────────────────────────────────────

/// AC4, from the other direction: with the default (disabled) `[cluster]`
/// section nothing is bound, nothing is spawned, no extension is installed and
/// no health indicator appears. Classified as a guard, not TDD red evidence.
#[tokio::test]
async fn disabled_cluster_installs_nothing() {
    let config = ClusterConfig::default();
    assert!(
        !config.enabled,
        "[cluster] must be off by default — an opt-in networked subsystem that \
         defaults on is a security bug"
    );

    let app = TestApp::new().config(app_config(config.clone())).build();
    let shutdown = CancellationToken::new();
    install_from_config(app.state(), &config, &shutdown)
        .expect("a disabled [cluster] section must install cleanly");

    assert!(
        app.state().extension::<ClusterHandle>().is_none(),
        "a disabled cluster must not install a ClusterHandle extension"
    );

    let response = app.get("/actuator/health").send().await;
    response.assert_ok();
    response.assert_json::<serde_json::Value, _>(|value| {
        assert!(
            value["components"]["cluster:membership"].is_null(),
            "a disabled cluster must not register a health indicator; got {value}"
        );
    });
}
