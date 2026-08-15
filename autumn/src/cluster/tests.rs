//! Deterministic two-node tests over the in-process loopback transport.
//!
//! These are the load-bearing tests for the cluster: a paused tokio runtime
//! plus an injected [`TickingClock`] means "advance three push intervals" is an
//! exact, reproducible statement rather than a sleep. Every assertion is about
//! *converged state* — member views, counter values, rejection counters — and
//! never about how many messages were sent.
//!
//! They live in-crate (not in `tests/`) because `LoopbackTransport`,
//! `ClusterNode` and the wire types are `pub(crate)`: the public integration
//! tests use only the public surface.

use std::sync::Arc;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use super::node::{ClusterNode, ClusterRuntimeConfig};
use super::transport::{LoopbackRouter, PeerTransport as _};
use super::wire::{self, ClusterMessage};
use super::{ClusterHandle, ClusterMemberStatus};
use crate::entropy::SeededEntropy;
use crate::time::TickingClock;

const CLUSTER: &str = "autumn";
const SECRET: &[u8] = b"a-shared-cluster-secret-value-32";
const OTHER_SECRET: &[u8] = b"a-different-cluster-secret-value";
const COUNTER: &str = "boids_sighted";

/// Fast but realistic: `suspicion` is 5x `push`, matching the shipped defaults
/// and satisfying the `>= 3x` validation rule.
const PUSH: Duration = Duration::from_millis(500);
const SUSPICION: Duration = Duration::from_millis(2_500);

/// A node plus the handles a test needs to drive and kill it.
struct TestNode {
    id: String,
    addr: String,
    handle: ClusterHandle,
    token: CancellationToken,
}

fn test_clock() -> TickingClock {
    TickingClock::starting_at(
        chrono::DateTime::<chrono::Utc>::from_timestamp(1_765_430_000, 0).unwrap_or_default(),
    )
}

/// Advance BOTH timelines: the injected clock (which the overlay and the
/// incarnation seed read) and tokio's virtual timer (which the node loops
/// sleep on). Advancing only one of them is the classic way to write a
/// deterministic test that proves nothing.
async fn advance_time(clock: &TickingClock, dur: Duration) {
    clock.advance(dur);
    tokio::time::sleep(dur).await;
}

/// Advance `rounds` push intervals, letting every loop run in between.
async fn settle(clock: &TickingClock, rounds: u32) {
    for _ in 0..rounds {
        advance_time(clock, PUSH).await;
    }
}

fn start_node(
    router: &LoopbackRouter,
    clock: &TickingClock,
    secret: &[u8],
    node_id: &str,
    seed: u64,
    seed_peers: Vec<String>,
) -> TestNode {
    let transport = router.endpoint();
    let addr = transport.local_addr().to_string();
    let token = CancellationToken::new();

    let handle = ClusterNode::start(
        ClusterRuntimeConfig {
            cluster_name: CLUSTER.to_owned(),
            secret: secret.to_vec(),
            node_id: Some(node_id.to_owned()),
            advertise_addr: None,
            seed_peers,
            push_interval: PUSH,
            suspicion_timeout: SUSPICION,
        },
        Arc::new(SeededEntropy::new(seed)),
        Arc::new(clock.clone()),
        token.clone(),
        transport,
    )
    .expect("a cluster node must start on a loopback transport");

    TestNode {
        id: node_id.to_owned(),
        addr,
        handle,
        token,
    }
}

/// Member ids as this node currently sees them, sorted for comparison.
fn member_ids(handle: &ClusterHandle) -> Vec<String> {
    let mut ids: Vec<String> = handle.members().into_iter().map(|m| m.id).collect();
    ids.sort();
    ids
}

/// A one-line description of a node's view, for failure messages.
fn view_of(node: &TestNode) -> String {
    format!(
        "{} sees {:?} (counter {} = {})",
        node.id,
        node.handle.members(),
        COUNTER,
        node.handle.counter(COUNTER).get()
    )
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn loopback_two_nodes_converge_to_two_member_view() {
    let router = LoopbackRouter::new();
    let clock = test_clock();

    let a = start_node(&router, &clock, SECRET, "node-a", 1, Vec::new());
    let b = start_node(&router, &clock, SECRET, "node-b", 2, vec![a.addr.clone()]);

    settle(&clock, 6).await;

    assert_eq!(
        member_ids(&a.handle),
        vec!["node-a".to_owned(), "node-b".to_owned()],
        "seeding B at A's address must give A a two-member view; {} | {}",
        view_of(&a),
        view_of(&b)
    );
    assert_eq!(
        member_ids(&b.handle),
        member_ids(&a.handle),
        "both nodes must converge on the SAME view — an asymmetric view is the \
         bug this asserts against; {} | {}",
        view_of(&a),
        view_of(&b)
    );
    assert!(
        a.handle
            .members()
            .iter()
            .all(|m| m.status == ClusterMemberStatus::Alive),
        "a freshly converged view must be entirely Alive; {}",
        view_of(&a)
    );

    a.token.cancel();
    b.token.cancel();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn loopback_increment_on_a_reads_on_b() {
    let router = LoopbackRouter::new();
    let clock = test_clock();

    let a = start_node(&router, &clock, SECRET, "node-a", 1, Vec::new());
    let b = start_node(&router, &clock, SECRET, "node-b", 2, vec![a.addr.clone()]);
    settle(&clock, 6).await;

    a.handle.counter(COUNTER).increment();

    assert_eq!(
        a.handle.counter(COUNTER).get(),
        1,
        "a local increment must be visible immediately on the writer; {}",
        view_of(&a)
    );

    settle(&clock, 4).await;

    assert_eq!(
        b.handle.counter(COUNTER).get(),
        1,
        "an increment on A must be readable on B after a few push intervals; {} | {}",
        view_of(&a),
        view_of(&b)
    );

    a.token.cancel();
    b.token.cancel();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn loopback_concurrent_increments_converge() {
    let router = LoopbackRouter::new();
    let clock = test_clock();

    let a = start_node(&router, &clock, SECRET, "node-a", 1, Vec::new());
    let b = start_node(&router, &clock, SECRET, "node-b", 2, vec![a.addr.clone()]);
    settle(&clock, 6).await;

    // Interleave writes on both nodes across several push rounds.
    for _ in 0..3 {
        a.handle.counter(COUNTER).increment();
        b.handle.counter(COUNTER).increment();
        advance_time(&clock, PUSH).await;
    }
    a.handle.counter(COUNTER).increment_by(4);

    settle(&clock, 6).await;

    assert_eq!(
        a.handle.counter(COUNTER).get(),
        10,
        "3 + 3 interleaved increments plus 4 on A must total 10 on A; {} | {}",
        view_of(&a),
        view_of(&b)
    );
    assert_eq!(
        b.handle.counter(COUNTER).get(),
        a.handle.counter(COUNTER).get(),
        "both nodes must converge on the identical total; {} | {}",
        view_of(&a),
        view_of(&b)
    );

    a.token.cancel();
    b.token.cancel();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn loopback_wrong_secret_peer_never_joins() {
    let router = LoopbackRouter::new();
    let clock = test_clock();

    let a = start_node(&router, &clock, SECRET, "node-a", 1, Vec::new());
    let b = start_node(&router, &clock, SECRET, "node-b", 2, vec![a.addr.clone()]);
    settle(&clock, 6).await;

    // A third node with the wrong secret, aimed at both real members.
    let intruder = start_node(
        &router,
        &clock,
        OTHER_SECRET,
        "node-intruder",
        3,
        vec![a.addr.clone(), b.addr.clone()],
    );

    settle(&clock, 10).await;

    assert!(
        a.handle.frames_rejected_total() > 0,
        "A must actually have seen and REFUSED the intruder's frames — a view \
         that stays at two because nothing arrived proves nothing; {}",
        view_of(&a)
    );
    assert_eq!(
        member_ids(&a.handle),
        vec!["node-a".to_owned(), "node-b".to_owned()],
        "a peer signing with a different secret must never enter the view; {}",
        view_of(&a)
    );
    assert_eq!(
        member_ids(&b.handle),
        vec!["node-a".to_owned(), "node-b".to_owned()],
        "…on either node; {}",
        view_of(&b)
    );

    a.token.cancel();
    b.token.cancel();
    intruder.token.cancel();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn loopback_clean_leave_converges_to_one_member_view() {
    let router = LoopbackRouter::new();
    let clock = test_clock();

    let a = start_node(&router, &clock, SECRET, "node-a", 1, Vec::new());
    let b = start_node(&router, &clock, SECRET, "node-b", 2, vec![a.addr.clone()]);
    settle(&clock, 6).await;
    a.handle.counter(COUNTER).increment();
    settle(&clock, 2).await;

    // Clean shutdown: B's leave must reach A well inside the 250 ms budget,
    // long before the suspicion timeout would have evicted it.
    b.token.cancel();
    advance_time(&clock, Duration::from_millis(250)).await;
    settle(&clock, 1).await;

    assert_eq!(
        member_ids(&a.handle),
        vec!["node-a".to_owned()],
        "a clean leave must converge A to a one-member view well before the \
         suspicion timeout ({}ms); {}",
        SUSPICION.as_millis(),
        view_of(&a)
    );

    // The survivor keeps serving the primitive.
    a.handle.counter(COUNTER).increment();
    assert_eq!(
        a.handle.counter(COUNTER).get(),
        2,
        "the surviving node must keep incrementing and reading its counter; {}",
        view_of(&a)
    );

    a.token.cancel();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn loopback_kill_without_leave_converges_after_suspicion_timeout() {
    let router = LoopbackRouter::new();
    let clock = test_clock();

    let a = start_node(&router, &clock, SECRET, "node-a", 1, Vec::new());
    let b = start_node(&router, &clock, SECRET, "node-b", 2, vec![a.addr.clone()]);
    settle(&clock, 6).await;
    a.handle.counter(COUNTER).increment();
    settle(&clock, 2).await;

    // Hard kill: unplug B from the router FIRST, so no leave can be delivered.
    router.disconnect(&b.addr);
    b.token.cancel();

    // Two push intervals of silence: B is Suspect, but Suspect stays in view.
    advance_time(&clock, PUSH.saturating_mul(2)).await;
    assert_eq!(
        member_ids(&a.handle),
        vec!["node-a".to_owned(), "node-b".to_owned()],
        "a silent peer must stay in the view until the suspicion timeout — \
         suspicion is the correctness path, not an instant eviction; {}",
        view_of(&a)
    );

    // Past the suspicion timeout it drops out.
    advance_time(&clock, SUSPICION).await;
    settle(&clock, 1).await;
    assert_eq!(
        member_ids(&a.handle),
        vec!["node-a".to_owned()],
        "past the suspicion timeout the killed peer must leave the view; {}",
        view_of(&a)
    );

    a.handle.counter(COUNTER).increment();
    assert_eq!(
        a.handle.counter(COUNTER).get(),
        2,
        "the survivor must keep serving the counter after a peer is killed; {}",
        view_of(&a)
    );

    a.token.cancel();
}

#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn loopback_replayed_leave_is_refuted_by_live_node() {
    let router = LoopbackRouter::new();
    let clock = test_clock();

    let a = start_node(&router, &clock, SECRET, "node-a", 1, Vec::new());
    let b = start_node(&router, &clock, SECRET, "node-b", 2, vec![a.addr.clone()]);
    settle(&clock, 6).await;

    let captured_incarnation = a.handle.incarnation();

    // Forge the frame an attacker would have captured: a correctly signed
    // `leave` from A at the incarnation it is running right now, replayed at a
    // sequence high enough to clear B's watermark.
    let frame = wire::sign_envelope(
        SECRET,
        CLUSTER,
        &a.id,
        captured_incarnation,
        u64::MAX / 2,
        &ClusterMessage::Leave,
    )
    .as_ref()
    .and_then(wire::encode_frame)
    .unwrap_or_default();
    assert!(
        !frame.is_empty(),
        "the test must be able to forge a signed leave frame to replay"
    );

    let delivered = router.deliver(&a.addr, &b.addr, frame);
    assert!(
        delivered,
        "the forged frame must reach node B to prove anything"
    );

    settle(&clock, 6).await;

    assert!(
        a.handle.incarnation() > captured_incarnation,
        "A must refute the replayed leave by bumping its incarnation \
         (was {captured_incarnation}, now {}); {}",
        a.handle.incarnation(),
        view_of(&a)
    );
    assert_eq!(
        member_ids(&b.handle),
        vec!["node-a".to_owned(), "node-b".to_owned()],
        "a replayed leave must not evict a live node: B's view must return to \
         two members; {} | {}",
        view_of(&a),
        view_of(&b)
    );

    a.token.cancel();
    b.token.cancel();
}
