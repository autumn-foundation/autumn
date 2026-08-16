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

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use super::membership::{ClusterState, MemberRecord, TOMBSTONE_TIMEOUT_MULTIPLE};
use super::node::{ClusterNode, ClusterRuntimeConfig};
use super::transport::{IncomingFrames, LoopbackRouter, PeerTransport};
use super::wire::{self, ClusterMessage, RejectReason};
use super::{ClusterHandle, ClusterMemberStatus, ClusterMetrics, REJECT_REASONS};
use crate::entropy::{Entropy, SeededEntropy};
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

/// Assert that both nodes see exactly `expected`, printing BOTH views when they
/// do not.
///
/// The second half is the load-bearing one: an asymmetric view — A sees the
/// pair, B sees only itself — is a real convergence bug that an assertion on
/// one node alone reports as a pass.
fn assert_converged(a: &TestNode, b: &TestNode, expected: &[&str], why: &str) {
    let expected: Vec<String> = expected.iter().map(|id| (*id).to_owned()).collect();
    assert_eq!(
        member_ids(&a.handle),
        expected,
        "{why}; {} | {}",
        view_of(a),
        view_of(b)
    );
    assert_eq!(
        member_ids(&b.handle),
        expected,
        "…and both nodes must converge on the SAME view ({why}); {} | {}",
        view_of(a),
        view_of(b)
    );
}

/// State pushes this node has handed to the transport since it started.
fn pushes_sent(node: &TestNode) -> u64 {
    node.handle
        .inner
        .metrics
        .pushes_sent
        .load(std::sync::atomic::Ordering::Relaxed)
}

/// Outbound messages this node could not sign or frame at all.
fn pushes_unsendable(node: &TestNode) -> u64 {
    node.handle
        .inner
        .metrics
        .pushes_unsendable
        .load(std::sync::atomic::Ordering::Relaxed)
}

/// Grow `node`'s replicated document past [`wire::MAX_FRAME_BYTES`] the way a
/// real one grows: one never-pruned counter cell per `(node id, boot)`.
///
/// Synchronous on purpose — the document lock must not be held across an await.
fn grow_document_past_the_frame_cap(node: &TestNode, cells: u64) {
    let mut filler = super::counter::CounterShards::default();
    for boot in 0..cells {
        filler.increment_cell(
            &super::counter::cell_key("node-a-long-enough-id-to-model-a-real-one", boot),
            1,
        );
    }
    let mut state = node.handle.inner.lock_state();
    state
        .counters
        .entry(COUNTER.to_owned())
        .or_default()
        .merge(&filler);
    drop(state);
}

/// Every `autumn_cluster_*` family this node publishes right now.
fn collect_families(node: &TestNode) -> Vec<crate::actuator::MetricFamily> {
    use crate::actuator::MetricsSource as _;
    super::ClusterMetricsSource {
        handle: node.handle.clone(),
    }
    .collect()
}

/// One unlabelled family's single sample value, or `None` when the family is
/// missing (which the caller asserts on, rather than panicking here).
fn family_value(families: &[crate::actuator::MetricFamily], name: &str) -> Option<f64> {
    families
        .iter()
        .find(|family| family.name == name)
        .and_then(|family| family.samples.first())
        .map(|sample| sample.value)
}

/// `autumn_cluster_frames_rejected_total` as `(reason, value)` pairs, in the
/// order the family publishes them.
fn rejected_series(families: &[crate::actuator::MetricFamily]) -> Vec<(String, f64)> {
    families
        .iter()
        .find(|family| family.name == "autumn_cluster_frames_rejected_total")
        .map(|family| {
            family
                .samples
                .iter()
                .map(|sample| {
                    let reason = sample
                        .labels
                        .first()
                        .map_or_else(String::new, |(_, reason)| reason.clone());
                    (reason, sample.value)
                })
                .collect()
        })
        .unwrap_or_default()
}

/// The reason series an untouched node publishes: every documented label, all
/// at zero.
fn zeroed_series() -> Vec<(String, f64)> {
    REJECT_REASONS
        .iter()
        .map(|reason| (reason.label().to_owned(), 0.0))
        .collect()
}

/// A [`PeerTransport`] that records every `retain_peers` call set and delegates
/// everything else.
///
/// Exists because the deterministic suite's loopback transport inherits the
/// trait's no-op `retain_peers`, which makes the production call site in
/// `push_round` invisible to every test — the writer-retirement unit test calls
/// the transport method directly and never goes through a node.
struct RetainSpy {
    inner: Arc<dyn PeerTransport>,
    retained: std::sync::Mutex<Vec<BTreeSet<String>>>,
}

impl RetainSpy {
    fn new(inner: Arc<dyn PeerTransport>) -> Self {
        Self {
            inner,
            retained: std::sync::Mutex::new(Vec::new()),
        }
    }

    /// The most recent target set the node asked the transport to retain.
    fn last_retained(&self) -> BTreeSet<String> {
        self.retained
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .last()
            .cloned()
            .unwrap_or_default()
    }
}

impl PeerTransport for RetainSpy {
    fn send(&self, to: &str, frame: Vec<u8>) {
        self.inner.send(to, frame);
    }

    fn take_incoming(&self) -> Option<IncomingFrames> {
        self.inner.take_incoming()
    }

    fn local_addr(&self) -> SocketAddr {
        self.inner.local_addr()
    }

    fn start(&self, shutdown: &CancellationToken, entropy: &Arc<dyn Entropy>) {
        self.inner.start(shutdown, entropy);
    }

    fn pending_frames(&self) -> usize {
        self.inner.pending_frames()
    }

    fn dropped_frames(&self) -> u64 {
        self.inner.dropped_frames()
    }

    fn framing_rejections(&self) -> u64 {
        self.inner.framing_rejections()
    }

    fn note_unauthenticated_frame(&self, from: &str) {
        self.inner.note_unauthenticated_frame(from);
    }

    fn retain_peers(&self, live: &BTreeSet<String>) {
        self.retained
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(live.clone());
        self.inner.retain_peers(live);
    }
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

    assert_converged(
        &a,
        &b,
        &["node-a", "node-b"],
        "seeding B at A's address must give A a two-member view",
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
    assert_converged(
        &a,
        &b,
        &["node-a", "node-b"],
        "a peer signing with a different secret must never enter the view",
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

/// A departure must carry the departing node's FINAL document, not just the
/// notice that it left.
///
/// An increment that lands between the last push round and cancellation exists
/// only on the departing node. If the leave carries no state, that increment
/// dies with the process while the survivor keeps a total that has silently
/// gone backwards — and a rolling restart re-learns the smaller number.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn loopback_departure_carries_the_final_document() {
    let router = LoopbackRouter::new();
    let clock = test_clock();

    let a = start_node(&router, &clock, SECRET, "node-a", 1, Vec::new());
    let b = start_node(&router, &clock, SECRET, "node-b", 2, vec![a.addr.clone()]);
    settle(&clock, 6).await;

    // Written on B and cancelled in the same breath: no push round in between,
    // so the only vehicle left for these cells is the departure itself.
    b.handle.counter(COUNTER).increment_by(7);
    b.token.cancel();

    advance_time(&clock, Duration::from_millis(250)).await;
    settle(&clock, 1).await;

    assert_eq!(
        a.handle.counter(COUNTER).get(),
        7,
        "the survivor must end up with the increments the departing node held \
         at cancellation — a counter that reads 0 here is a write accepted and \
         then thrown away; {} | {}",
        view_of(&a),
        view_of(&b)
    );
    assert_eq!(
        member_ids(&a.handle),
        vec!["node-a".to_owned()],
        "…and the departure must still converge the view to one member; {}",
        view_of(&a)
    );

    a.token.cancel();
}

/// A write storm must not turn the push loop into request-rate gossip.
///
/// Every increment leaves a notify permit behind, so an unthrottled loop
/// clones, signs and sends the whole document once per write. This pins the
/// cadence floor: the first nudge after a quiet gap still pushes promptly, and
/// the rest of the storm collapses into it.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn loopback_write_storm_is_bounded_by_the_push_cadence_floor() {
    let router = LoopbackRouter::new();
    let clock = test_clock();

    let a = start_node(&router, &clock, SECRET, "node-a", 1, Vec::new());
    let b = start_node(&router, &clock, SECRET, "node-b", 2, vec![a.addr.clone()]);
    settle(&clock, 6).await;

    // Past the floor (a quarter of PUSH) but well short of a whole interval,
    // so the next push can only come from a nudge.
    advance_time(&clock, PUSH.checked_div(2).unwrap_or(PUSH)).await;

    let before = pushes_sent(&a);
    // `yield_now` rather than a sleep: the loop gets to run between writes (so
    // the permits really are consumed one at a time) while the clock stands
    // still, which is exactly the "increments faster than the interval" shape.
    for _ in 0..50 {
        a.handle.counter(COUNTER).increment();
        tokio::task::yield_now().await;
    }
    let storm_pushes = pushes_sent(&a).saturating_sub(before);

    assert!(
        storm_pushes >= 1,
        "the first nudge after a quiet gap must still push promptly — a floor \
         that swallows it would make every write wait out the interval; {}",
        view_of(&a)
    );
    assert!(
        storm_pushes <= 4,
        "50 increments inside one floor window must collapse into a handful of \
         pushes, not 50: observed {storm_pushes} pushes; {}",
        view_of(&a)
    );

    // …and the storm is still delivered: bounding the cadence must not lose a
    // single increment.
    settle(&clock, 4).await;
    assert_eq!(
        b.handle.counter(COUNTER).get(),
        50,
        "every increment in the storm must reach B once the pushes resume; {} | {}",
        view_of(&a),
        view_of(&b)
    );

    a.token.cancel();
    b.token.cancel();
}

/// The metrics contract, which the guide publishes as a table an operator
/// writes alerts against: every family is emitted from boot (including the
/// zeroes), the members gauge really is the local view's size, and every
/// `reason` label of the rejection family exists before anything is rejected —
/// a `rate()` over a label that only appears once an attack starts is a
/// `rate()` nobody wrote an alert for.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn metrics_source_emits_every_cluster_family() {
    use crate::actuator::MetricKind;

    let router = LoopbackRouter::new();
    let clock = test_clock();
    let a = start_node(&router, &clock, SECRET, "node-a", 1, Vec::new());
    let b = start_node(&router, &clock, SECRET, "node-b", 2, vec![a.addr.clone()]);
    settle(&clock, 6).await;

    let families = collect_families(&a);
    let names: Vec<&str> = families.iter().map(|family| family.name.as_str()).collect();

    assert_eq!(
        names,
        vec![
            "autumn_cluster_members",
            "autumn_cluster_pushes_sent_total",
            "autumn_cluster_pushes_unsendable_total",
            "autumn_cluster_pushes_received_total",
            "autumn_cluster_merges_applied_total",
            "autumn_cluster_frames_dropped_total",
            "autumn_cluster_frames_rejected_total",
        ],
        "the seven documented families must all be emitted, under exactly the \
         names docs/guide/clustering.md tells operators to alert on"
    );
    assert_eq!(
        family_value(&families, "autumn_cluster_pushes_unsendable_total"),
        Some(0.0),
        "a healthy node publishes the unsendable series at zero — an alert on a \
         label that only appears once the document outgrows the frame cap is an \
         alert nobody wrote"
    );

    let members = families
        .iter()
        .find(|family| family.name == "autumn_cluster_members")
        .expect("the members family is in the list asserted above");
    let expected = f64::from(u32::try_from(a.handle.members().len()).unwrap_or(u32::MAX));
    assert_eq!(
        members.kind,
        MetricKind::Gauge,
        "the view's size is a gauge"
    );
    assert_eq!(
        members.samples.iter().map(|s| s.value).collect::<Vec<_>>(),
        vec![expected],
        "the members gauge must track the local view exactly; {} | {}",
        view_of(&a),
        view_of(&b)
    );
    assert!(
        expected > 1.0,
        "sanity: the gauge must be read against a converged two-member view, \
         or a broken gauge reading 1 would pass; {}",
        view_of(&a)
    );

    assert_eq!(
        rejected_series(&families),
        zeroed_series(),
        "every reason label must be published from boot, and on a healthy \
         cluster every one of them must read ZERO — asserting only the labels \
         leaves the values, which are the thing an alert fires on, unpinned"
    );

    a.token.cancel();
    b.token.cancel();
}

/// The per-reason mapping itself, in isolation: each [`RejectReason`] owns one
/// series, and the transport's framing rejections fold into `oversize`.
///
/// The guide sells both to operators — "a steady `frames_rejected_total`
/// `{reason="mac"}` means somebody is talking to your port with the wrong
/// secret", and "`reason="oversize"` covers the connection-fatal step-1
/// rejections too" — and both are one off-by-one away from silently attributing
/// an attack to the wrong series.
#[test]
fn rejection_counters_are_per_reason_and_fold_framing_into_oversize() {
    let metrics = ClusterMetrics::default();

    assert_eq!(
        metrics.rejections_by_reason(),
        REJECT_REASONS
            .iter()
            .map(|reason| (reason.label(), 0))
            .collect::<Vec<_>>(),
        "a fresh node must publish every documented series at zero"
    );

    // One frame per reason: every series must move by exactly one, which no
    // "always increment slot 0" or off-by-one indexing can satisfy.
    for reason in REJECT_REASONS {
        metrics.record_rejection(reason);
    }
    assert_eq!(
        metrics.rejections_by_reason(),
        REJECT_REASONS
            .iter()
            .map(|reason| (reason.label(), 1))
            .collect::<Vec<_>>(),
        "each reason must increment its OWN series, never a shared or shifted one"
    );

    // A second wrong-secret frame moves only `mac`.
    metrics.record_rejection(RejectReason::Mac);
    // …and the framing layer's connection-fatal rejections fold into
    // `oversize`, the series the guide says covers them.
    metrics.framing_rejected.store(3, Ordering::Relaxed);

    let expected: Vec<(&str, u64)> = REJECT_REASONS
        .iter()
        .map(|reason| match *reason {
            RejectReason::Oversize => (reason.label(), 4),
            RejectReason::Mac => (reason.label(), 2),
            other => (other.label(), 1),
        })
        .collect();
    assert_eq!(
        metrics.rejections_by_reason(),
        expected,
        "the transport's framing rejections must land in `oversize` (not \
         `malformed`, and not in a series of their own), while every other \
         reason keeps its own count"
    );
    assert_eq!(
        metrics.rejected_total(),
        13,
        "the unlabelled aggregate must be the sum over every reason INCLUDING \
         the folded framing rejections"
    );
}

/// The same mapping, end to end through a running node: a forged frame per
/// representative reason must move exactly that reason's published series.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn forged_frames_are_attributed_to_their_own_reason_series() {
    let router = LoopbackRouter::new();
    let clock = test_clock();
    let a = start_node(&router, &clock, SECRET, "node-a", 1, Vec::new());
    // An endpoint with no node behind it: purely a return address the router
    // will accept frames from.
    let hostile = router.endpoint();
    let hostile_addr = hostile.local_addr().to_string();
    settle(&clock, 2).await;

    assert_eq!(
        rejected_series(&collect_families(&a)),
        zeroed_series(),
        "sanity: nothing has been rejected yet, or the deltas below prove nothing"
    );

    let forge = |secret: &[u8], cluster: &str, sender: &str, incarnation: u64, seq: u64| {
        wire::sign_envelope(
            secret,
            cluster,
            sender,
            incarnation,
            seq,
            &ClusterMessage::Leave,
        )
        .as_ref()
        .and_then(wire::encode_frame)
        .unwrap_or_default()
    };

    // Wrong secret → `mac`. Wrong cluster name → `cluster`. A frame delivered
    // twice → `replay` on the second.
    let wrong_secret = forge(OTHER_SECRET, CLUSTER, "node-intruder", 9, 1);
    let wrong_cluster = forge(SECRET, "some-other-cluster", "node-stranger", 9, 1);
    let replayed = forge(SECRET, CLUSTER, "node-ghost", 9, 1);

    for frame in [
        wrong_secret,
        wrong_cluster,
        replayed.clone(),
        replayed.clone(),
    ] {
        assert!(
            router.deliver(&hostile_addr, &a.addr, frame),
            "every forged frame must reach node A, or this test proves nothing"
        );
        settle(&clock, 1).await;
    }

    let expected: Vec<(String, f64)> = REJECT_REASONS
        .iter()
        .map(|reason| {
            let value = match *reason {
                RejectReason::Mac | RejectReason::Cluster | RejectReason::Replay => 1.0,
                _ => 0.0,
            };
            (reason.label().to_owned(), value)
        })
        .collect();
    assert_eq!(
        rejected_series(&collect_families(&a)),
        expected,
        "a wrong-secret frame must move ONLY `mac`, a foreign cluster name only \
         `cluster`, and a replayed frame only `replay` — an operator alerting \
         on `mac` must not be looking at a series somebody else's traffic \
         increments; {}",
        view_of(&a)
    );

    a.token.cancel();
}

/// A document that no longer fits one frame must be **observable**.
///
/// The push is also the heartbeat, so a node whose document has outgrown the
/// 64 KiB cap keeps merging its peer's pushes and looks perfectly healthy to
/// itself while the peer sees pure silence and evicts it. `frames_dropped`
/// cannot see it (the transport never receives the frame) and `frames_rejected`
/// is inbound-only, so without a series of its own the failure is invisible.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn an_oversized_document_is_counted_as_unsendable() {
    /// Enough `(node, boot)` cells that the serialized document is comfortably
    /// past `MAX_FRAME_BYTES`, built the way a real one grows: cells are never
    /// pruned, and a node with no configured `node_id` mints a fresh member id
    /// per restart.
    const FILLER_CELLS: u64 = 2_000;

    let router = LoopbackRouter::new();
    let clock = test_clock();
    let a = start_node(&router, &clock, SECRET, "node-a", 1, Vec::new());
    let b = start_node(&router, &clock, SECRET, "node-b", 2, vec![a.addr.clone()]);
    settle(&clock, 6).await;

    assert_eq!(
        pushes_unsendable(&a),
        0,
        "sanity: a healthy node must be able to send; {}",
        view_of(&a)
    );

    grow_document_past_the_frame_cap(&a, FILLER_CELLS);

    let sent_before = pushes_sent(&a);
    settle(&clock, 3).await;

    assert!(
        pushes_unsendable(&a) > 0,
        "a document past the frame cap must increment the unsendable counter — \
         silently dropping every heartbeat is the one failure an operator has no \
         other way to see; {}",
        view_of(&a)
    );
    assert_eq!(
        pushes_sent(&a),
        sent_before,
        "…and it must NOT be counted as a push that was sent; {}",
        view_of(&a)
    );
    assert_eq!(
        family_value(
            &collect_families(&a),
            "autumn_cluster_pushes_unsendable_total"
        ),
        Some(f64::from(
            u32::try_from(pushes_unsendable(&a)).unwrap_or(u32::MAX)
        )),
        "the counter must be published as autumn_cluster_pushes_unsendable_total"
    );

    a.token.cancel();
    b.token.cancel();
}

/// Pruning a tombstone must forget the departed node **whole** — its replay
/// watermark included — or a node that comes back at a LOWER incarnation is
/// partitioned for good.
///
/// After the tombstone window the survivor's document holds no record of the
/// departed node, and nothing pushes to its address any more, so refutation
/// (the documented recovery for a clock that stepped backwards) has nothing to
/// trigger on. If the watermark outlives the record, every frame the returning
/// node sends is dropped as a replay, both nodes serve a one-member view, and
/// only an operator restarting one of them can break the deadlock.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn loopback_pruned_member_rejoins_at_a_lower_incarnation() {
    let router = LoopbackRouter::new();
    let clock = test_clock();

    let a = start_node(&router, &clock, SECRET, "node-a", 1, Vec::new());
    let b = start_node(&router, &clock, SECRET, "node-b", 2, vec![a.addr.clone()]);
    settle(&clock, 6).await;
    assert_converged(
        &a,
        &b,
        &["node-a", "node-b"],
        "the pair must converge before B departs, or the prune below proves nothing",
    );
    let departed_incarnation = b.handle.incarnation();

    // A clean departure, then the whole tombstone window: ten suspicion
    // timeouts after A first observed the leave, A prunes the record.
    b.token.cancel();
    advance_time(&clock, Duration::from_millis(250)).await;
    let window_rounds = u32::try_from(
        SUSPICION
            .saturating_mul(TOMBSTONE_TIMEOUT_MULTIPLE)
            .as_millis()
            .saturating_div(PUSH.as_millis().max(1))
            .saturating_add(2),
    )
    .unwrap_or(u32::MAX);
    settle(&clock, window_rounds).await;

    let still_held = {
        let state = a.handle.inner.lock_state();
        state.members.contains_key(&b.id)
    };
    assert!(
        !still_held,
        "sanity: A must have pruned B's tombstone after ten suspicion timeouts, \
         or the rejoin below is testing the pre-prune path instead; {}",
        view_of(&a)
    );

    // B comes back on the same node id behind a clock that stepped backwards:
    // a strictly LOWER incarnation than the one A last accepted.
    let rejoin_incarnation = 1;
    assert!(
        rejoin_incarnation < departed_incarnation,
        "the rejoin must really be lower than the watermark A recorded \
         ({departed_incarnation}), or this test asserts nothing"
    );
    let mut returning = ClusterState::default();
    returning.members.insert(
        b.id.clone(),
        MemberRecord::alive(b.addr.clone(), rejoin_incarnation),
    );
    let frame = wire::sign_envelope(
        SECRET,
        CLUSTER,
        &b.id,
        rejoin_incarnation,
        0,
        &ClusterMessage::StatePush { state: returning },
    )
    .as_ref()
    .and_then(wire::encode_frame)
    .unwrap_or_default();
    assert!(
        router.deliver(&b.addr, &a.addr, frame),
        "the returning node's push must reach A to prove anything"
    );

    settle(&clock, 1).await;

    assert_eq!(
        member_ids(&a.handle),
        vec!["node-a".to_owned(), "node-b".to_owned()],
        "a pruned member is a forgotten member: its next push must be judged as \
         a FRESH sender, whatever incarnation it carries. Holding the watermark \
         past the tombstone leaves the returning node replay-dropped with no \
         record left to refute — a permanent partition; {}",
        view_of(&a)
    );

    a.token.cancel();
}

/// The production wiring of `retain_peers`: an address that leaves the target
/// set must be retired **through the push loop**, not just when a test calls
/// the transport method by hand.
///
/// Deleting the call from `push_round` leaves the whole suite green otherwise,
/// while a member that returns at a new address keeps its old address's writer
/// task and bounded queue alive for the life of the process.
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn push_round_retires_writers_for_addresses_that_left_the_target_set() {
    const OLD_ADDR: &str = "127.0.0.1:47901";
    const NEW_ADDR: &str = "127.0.0.1:47902";

    let router = LoopbackRouter::new();
    let clock = test_clock();
    let spy = Arc::new(RetainSpy::new(router.endpoint()));
    let token = CancellationToken::new();

    let handle = ClusterNode::start(
        ClusterRuntimeConfig {
            cluster_name: CLUSTER.to_owned(),
            secret: SECRET.to_vec(),
            node_id: Some("node-a".to_owned()),
            advertise_addr: None,
            seed_peers: Vec::new(),
            push_interval: PUSH,
            suspicion_timeout: SUSPICION,
        },
        Arc::new(SeededEntropy::new(11)),
        Arc::new(clock.clone()),
        token.clone(),
        Arc::clone(&spy) as Arc<dyn PeerTransport>,
    )
    .expect("a cluster node must start on a spying transport");

    // Teach the node a peer at one address…
    handle
        .inner
        .lock_state()
        .members
        .insert("node-x".to_owned(), MemberRecord::alive(OLD_ADDR, 1));
    settle(&clock, 2).await;
    assert!(
        spy.last_retained().contains(OLD_ADDR),
        "a known member's address must be in the retained set; observed {:?}",
        spy.last_retained()
    );

    // …then move it, exactly as a merge of a higher-incarnation record does.
    handle
        .inner
        .lock_state()
        .members
        .insert("node-x".to_owned(), MemberRecord::alive(NEW_ADDR, 2));
    settle(&clock, 2).await;

    let retained = spy.last_retained();
    assert!(
        retained.contains(NEW_ADDR),
        "the member's new address must become a target; observed {retained:?}"
    );
    assert!(
        !retained.contains(OLD_ADDR),
        "the abandoned address must be retired through the push round, or its \
         writer task and queue outlive the address forever; observed {retained:?}"
    );

    token.cancel();
}

/// `Suspect` must be visible on the surface the guide documents: two missed
/// pushes are "worth showing an operator", and the row an operator reads is
/// `ClusterHandle::members()` (and the health rows built from it).
#[tokio::test(flavor = "current_thread", start_paused = true)]
async fn loopback_silent_peer_reads_as_suspect_before_it_drops_out() {
    let router = LoopbackRouter::new();
    let clock = test_clock();

    let a = start_node(&router, &clock, SECRET, "node-a", 1, Vec::new());
    let b = start_node(&router, &clock, SECRET, "node-b", 2, vec![a.addr.clone()]);
    settle(&clock, 6).await;
    assert_converged(
        &a,
        &b,
        &["node-a", "node-b"],
        "the pair must be Alive before B goes silent",
    );

    // Unplug B and stop short of the suspicion timeout: past 2x push (1000ms),
    // well inside the 2500ms eviction.
    router.disconnect(&b.addr);
    advance_time(&clock, PUSH.saturating_mul(3)).await;

    let view = a.handle.members();
    let peer = view.iter().find(|member| member.id == b.id);
    assert_eq!(
        peer.map(|member| member.status),
        Some(ClusterMemberStatus::Suspect),
        "a peer silent past two push intervals must be reported Suspect — not \
         quietly Alive, and not evicted; observed {view:?}"
    );
    assert_eq!(
        view.iter()
            .find(|member| member.id == a.id)
            .map(|member| member.status),
        Some(ClusterMemberStatus::Alive),
        "…while this node itself stays Alive in its own view; observed {view:?}"
    );
    assert_eq!(
        member_ids(&a.handle),
        vec!["node-a".to_owned(), "node-b".to_owned()],
        "a Suspect member is a warning, not an eviction: it must still be in \
         the view; {}",
        view_of(&a)
    );

    a.token.cancel();
    b.token.cancel();
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
    assert_converged(
        &a,
        &b,
        &["node-a", "node-b"],
        "a replayed leave must not evict a live node: B's view must return to \
         two members",
    );

    a.token.cancel();
    b.token.cancel();
}
