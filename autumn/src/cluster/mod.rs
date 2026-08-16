//! Embedded, zero-dependency self-clustering control plane (issue #1762).
//!
//! Two instances of the same Autumn app, given a shared secret and a seed
//! address, discover each other over an authenticated TCP gossip transport and
//! converge on a **single replicated document** — a join-semilattice carrying
//! both the member table and the cluster-wide counters. There is no Redis, no
//! Postgres, no etcd and no `ZooKeeper` in the picture: the control plane is the
//! binary.
//!
//! # What ships in this slice
//!
//! - **Membership.** One periodic signed `StatePush` carries the whole document
//!   and doubles as the heartbeat. Replicated member status is `Alive`/`Left`
//!   only; liveness (`Alive` → `Suspect` → `Down`) is a *local* overlay driven
//!   by time-since-last-push read through the injected
//!   [`ClockSource`]. Views are therefore local and
//!   **eventually consistent** by construction.
//! - **Exactly one distributed primitive.** A cluster-wide grow-only counter
//!   ([`ClusterCounter`]), convergent (CRDT) because each node writes only its
//!   own `(node, boot)` cell and merge is per-cell max.
//!
//! The transport is **authenticated (HMAC-SHA256) but not encrypted**, and the
//! counter's lifetime is the cluster's process lifetime — see
//! `docs/guide/clustering.md` for the full failure-semantics contract.
//!
//! # Scope
//!
//! **Experimental, and a deliberate two-node slice.** Every push carries the
//! whole document to every known peer and there is no quorum anywhere: that is
//! a sound design at two nodes and an unproven one beyond. The public surface
//! is exactly [`ClusterHandle`], [`ClusterMemberInfo`], [`ClusterMemberStatus`],
//! [`ClusterCounter`], [`install_from_config`] and
//! [`ClusterConfig`]; everything protocol-shaped
//! (the wire envelope, the member records, the transport) is crate-private, so
//! the format stays free to change while the feature is unreleased.
//!
//! # Naming
//!
//! `[cluster]` here means *app nodes clustering with each other*. It is
//! unrelated to [`crate::sharding`]'s Redis-Cluster-style **database** shard
//! vocabulary.

// autumn-determinism-gate: production code in this module must read time and
// mint identifiers through the framework's injected seams (ClockSource /
// Entropy), never `Instant::now()` / `Utc::now()` / `SystemTime::now()` /
// `Uuid::new_v4()` directly. See CONTRIBUTING.md "Determinism seam gate"
// (issue #1797). Justify exceptions with
// #[allow(clippy::disallowed_methods, reason = "…")] at the narrowest scope.
#![cfg_attr(not(test), deny(clippy::disallowed_methods))]
// autumn-panic-gate: request-path module — production code path must be panic-free.
// See CONTRIBUTING.md "Request-path panic gate". Justify exceptions with
// #[allow(clippy::<lint>, reason = "…")] at the narrowest scope.
#![cfg_attr(
    not(test),
    deny(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::unreachable,
        clippy::todo,
        clippy::unimplemented,
        clippy::indexing_slicing,
        clippy::string_slice,
        clippy::arithmetic_side_effects,
    )
)]

pub(crate) mod counter;
pub(crate) mod membership;
pub(crate) mod node;
pub(crate) mod transport;
pub(crate) mod wire;

#[cfg(test)]
mod tests;

use std::fmt;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use secrecy::ExposeSecret as _;
use tokio_util::sync::CancellationToken;

use crate::config::ClusterConfig;
use crate::state::AppState;
use crate::time::ClockSource;
use crate::{AutumnError, AutumnResult};

pub use counter::ClusterCounter;
/// The window a departing node gets to put its final document and its `leave`
/// on the wire. Read by the app's shutdown sequence, which waits exactly this
/// long after cancelling the cluster so the notice is not cut off by process
/// exit.
pub(crate) use node::LEAVE_BUDGET;

/// Stable identity of one cluster member for the lifetime of a process.
///
/// Entropy-derived by default (never hostname-derived), overridable through
/// `[cluster] node_id`.
pub(crate) type NodeId = String;

/// Monotonically increasing per-boot generation counter used to order member
/// records and to refute a stale `Left`.
pub(crate) type Incarnation = u64;

/// Liveness of a member **as this node currently sees it**.
///
/// A view is local: two healthy nodes agree only *eventually*.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[non_exhaustive]
pub enum ClusterMemberStatus {
    /// A frame from this member arrived within the last two push intervals.
    Alive,
    /// Silence past two push intervals — a warning, not an eviction: the member
    /// stays in the view until the suspicion timeout elapses.
    Suspect,
}

impl fmt::Display for ClusterMemberStatus {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match *self {
            Self::Alive => f.write_str("alive"),
            Self::Suspect => f.write_str("suspect"),
        }
    }
}

/// One row of the local member view returned by [`ClusterHandle::members`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterMemberInfo {
    /// The member's node id.
    pub id: String,
    /// The address peers should dial to reach this member.
    pub addr: String,
    /// Liveness as seen locally right now.
    pub status: ClusterMemberStatus,
    /// The member's current incarnation.
    pub incarnation: u64,
}

/// Every [`wire::RejectReason`], in the order their per-reason counters are
/// stored in [`ClusterMetrics::rejected_by_reason`].
///
/// Exhaustive on purpose: the `reason` label on
/// `autumn_cluster_frames_rejected_total` is what turns "somebody is talking to
/// my port with the wrong secret" into an alert, so every series is published
/// from boot — a `rate()` over a label that only appears once the attack starts
/// is a `rate()` nobody wrote an alert for.
pub(crate) const REJECT_REASONS: [wire::RejectReason; 9] = [
    wire::RejectReason::Oversize,
    wire::RejectReason::Malformed,
    wire::RejectReason::Version,
    wire::RejectReason::KeyId,
    wire::RejectReason::Cluster,
    wire::RejectReason::Mac,
    wire::RejectReason::SelfOrigin,
    wire::RejectReason::Replay,
    wire::RejectReason::Payload,
];

/// Counters that make cluster behaviour observable from `/actuator/metrics`
/// and from tests, without asserting on message counts.
#[derive(Debug, Default)]
pub(crate) struct ClusterMetrics {
    /// Frames refused by the verifier, one counter per
    /// [`REJECT_REASONS`] entry and in that order.
    rejected_by_reason: [AtomicU64; REJECT_REASONS.len()],
    /// Remote documents successfully merged into the local one.
    pub(crate) merges_applied: AtomicU64,
    /// State pushes handed to the transport.
    pub(crate) pushes_sent: AtomicU64,
    /// Outbound messages that could not be signed or framed at all, so the
    /// transport never saw them.
    ///
    /// Almost always one thing: the replicated document has outgrown
    /// [`MAX_FRAME_BYTES`](wire::MAX_FRAME_BYTES), so the sender refuses to
    /// emit a frame its peer would be obliged to reject. That is silent
    /// otherwise — `frames_dropped` only sees drops the *transport* made and
    /// `frames_rejected` is inbound-only — and since the push is also the
    /// heartbeat, a node in that state keeps merging its peer's pushes while
    /// the peer watches it go `Suspect`, then `Down`. Anything above zero on
    /// this series is that failure and needs an operator.
    pub(crate) pushes_unsendable: AtomicU64,
    /// Monotonic milliseconds (since the injected clock's origin, plus one) at
    /// which the last unsendable-push warning was logged; `0` for "never".
    ///
    /// The offset by one is what lets `0` mean "never" without a second field:
    /// a node whose very first push is unsendable reads the clock at origin.
    pub(crate) unsendable_warned_at_ms: AtomicU64,
    /// State pushes accepted from peers.
    pub(crate) pushes_received: AtomicU64,
    /// Frames the transport could not queue (a peer's bounded writer queue was
    /// full, or that peer has no writer). Never an error: anti-entropy re-sends
    /// the whole document on the next interval.
    pub(crate) frames_dropped: AtomicU64,
    /// Inbound frames the transport's *framing* layer refused (a zero or
    /// oversize length prefix), mirrored from
    /// [`PeerTransport::framing_rejections`](transport::PeerTransport::framing_rejections).
    ///
    /// Kept apart from `rejected_by_reason` because the two are written
    /// differently — this one mirrors a monotonic counter the transport owns,
    /// that one is incremented here — and summed into the `oversize` series by
    /// [`rejections_by_reason`](Self::rejections_by_reason), so one series
    /// covers both the connection-fatal TCP rejections and the whole-buffer
    /// ones the verifier sees.
    pub(crate) framing_rejected: AtomicU64,
}

impl ClusterMetrics {
    /// Count one refused frame under its reason.
    ///
    /// A reason with no slot is impossible ([`REJECT_REASONS`] is exhaustive)
    /// and is dropped rather than panicked on: the receive loop must survive
    /// anything, including a future variant somebody forgot to add here.
    pub(crate) fn record_rejection(&self, reason: wire::RejectReason) {
        if let Some(counter) = REJECT_REASONS
            .iter()
            .position(|candidate| *candidate == reason)
            .and_then(|index| self.rejected_by_reason.get(index))
        {
            counter.fetch_add(1, Ordering::Relaxed);
        }
    }

    /// Every `(reason label, count)` pair, including the zeroes.
    ///
    /// The `oversize` pair carries the transport's framing rejections too: a
    /// bad length prefix closes the connection before the verifier ever sees
    /// the bytes, and an operator alerting on the series does not care which
    /// layer refused them.
    pub(crate) fn rejections_by_reason(&self) -> Vec<(&'static str, u64)> {
        let framing = self.framing_rejected.load(Ordering::Relaxed);
        REJECT_REASONS
            .iter()
            .enumerate()
            .filter_map(|(index, reason)| {
                self.rejected_by_reason.get(index).map(|counter| {
                    let mut count = counter.load(Ordering::Relaxed);
                    if *reason == wire::RejectReason::Oversize {
                        count = count.saturating_add(framing);
                    }
                    (reason.label(), count)
                })
            })
            .collect()
    }

    /// Total frames refused, for any reason and at either layer.
    #[cfg(test)]
    pub(crate) fn rejected_total(&self) -> u64 {
        self.rejections_by_reason()
            .into_iter()
            .map(|(_, count)| count)
            .fold(0, u64::saturating_add)
    }
}

/// Everything one cluster node owns, shared between the node's loops and every
/// [`ClusterHandle`] / [`ClusterCounter`] clone.
pub(crate) struct ClusterInner {
    pub(crate) node_id: NodeId,
    pub(crate) cluster_name: String,
    pub(crate) local_addr: SocketAddr,
    /// The address advertised to peers (defaults to `local_addr`).
    pub(crate) advertise_addr: String,
    pub(crate) secret: Vec<u8>,
    pub(crate) seed_peers: Vec<String>,
    pub(crate) push_interval: Duration,
    /// This process's incarnation; bumped when refuting a stale `Left`.
    pub(crate) incarnation: AtomicU64,
    /// The single replicated document (members + counters).
    pub(crate) state: Mutex<membership::ClusterState>,
    /// Local, never-replicated failure detector.
    pub(crate) overlay: Mutex<membership::LivenessOverlay>,
    /// Members whose `Left` tombstone the push loop has pruned and whose replay
    /// watermark the receive loop has not dropped yet.
    ///
    /// The one channel between the two loops that is not the document itself.
    /// Pruning happens in the push round; the [`wire::FrameVerifier`] that
    /// holds the watermarks is a local of the receive loop, and a prune must
    /// forget a node *whole* — see
    /// [`FrameVerifier::forget`](wire::FrameVerifier::forget). The receive loop
    /// drains this set before every verification, so the two never need a lock
    /// at the same time.
    pub(crate) pruned_senders: Mutex<std::collections::BTreeSet<NodeId>>,
    pub(crate) clock: Arc<dyn ClockSource>,
    /// Source of the per-node push jitter and of the default node id.
    pub(crate) entropy: Arc<dyn crate::entropy::Entropy>,
    pub(crate) transport: Arc<dyn transport::PeerTransport>,
    /// Child of the app's shutdown token; every spawned loop selects on it.
    pub(crate) shutdown: CancellationToken,
    /// Nudges the push loop when a local write happens.
    pub(crate) notify: tokio::sync::Notify,
    pub(crate) metrics: ClusterMetrics,
}

impl ClusterInner {
    /// Lock the replicated document, recovering from a poisoned mutex rather
    /// than panicking (the panic gate forbids `unwrap`).
    pub(crate) fn lock_state(&self) -> std::sync::MutexGuard<'_, membership::ClusterState> {
        self.state.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Lock the local liveness overlay, recovering from poisoning.
    pub(crate) fn lock_overlay(&self) -> std::sync::MutexGuard<'_, membership::LivenessOverlay> {
        self.overlay.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Hand the receive loop a set of pruned member ids to forget.
    pub(crate) fn note_pruned_senders(&self, pruned: impl IntoIterator<Item = NodeId>) {
        let mut queued = self
            .pruned_senders
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        queued.extend(pruned);
    }

    /// Take everything [`note_pruned_senders`](Self::note_pruned_senders) has
    /// queued since the last drain. Empty almost always, and cheap when it is.
    pub(crate) fn take_pruned_senders(&self) -> std::collections::BTreeSet<NodeId> {
        let mut queued = self
            .pruned_senders
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        std::mem::take(&mut queued)
    }
}

/// Handle onto the running cluster node.
///
/// Installed on [`AppState`] as an extension when `[cluster] enabled = true`;
/// `state.extension::<ClusterHandle>()` is `None` on every node where
/// clustering is off.
///
/// Cheap to clone — every clone addresses the same node. Experimental and
/// two-node-scoped; see the [module docs](self#scope).
#[derive(Clone)]
pub struct ClusterHandle {
    inner: Arc<ClusterInner>,
}

impl fmt::Debug for ClusterHandle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ClusterHandle")
            .field("node_id", &self.inner.node_id)
            .field("cluster_name", &self.inner.cluster_name)
            .field("local_addr", &self.inner.local_addr)
            .finish_non_exhaustive()
    }
}

impl ClusterHandle {
    pub(crate) const fn from_inner(inner: Arc<ClusterInner>) -> Self {
        Self { inner }
    }

    /// This node's id.
    #[must_use]
    pub fn node_id(&self) -> &str {
        &self.inner.node_id
    }

    /// The cluster this node belongs to (`[cluster] cluster_name`).
    ///
    /// Signed into every frame, so two clusters that share a secret still
    /// refuse each other's traffic.
    #[must_use]
    pub fn cluster_name(&self) -> &str {
        &self.inner.cluster_name
    }

    /// The address this node's cluster listener is actually bound to.
    ///
    /// With the default `bind_addr = "127.0.0.1:0"` this is the OS-assigned
    /// ephemeral port — the value to hand a second node as its seed peer.
    #[must_use]
    pub fn local_addr(&self) -> SocketAddr {
        self.inner.local_addr
    }

    /// The **local** member view: replicated `Alive` records minus locally
    /// `Down`/`Left` peers.
    ///
    /// Eventually consistent: two nodes converge within a bounded number of
    /// push intervals, they are not instantaneously identical.
    #[must_use]
    pub fn members(&self) -> Vec<ClusterMemberInfo> {
        // Read the clock before taking either lock: a clock source may lock
        // internals of its own, and a view is cheap enough not to hold three
        // locks at once.
        let now = self.inner.clock.monotonic();
        let incarnation = self.inner.incarnation.load(Ordering::Relaxed);

        let state = self.inner.lock_state();
        let overlay = self.inner.lock_overlay();

        let mut view = Vec::with_capacity(state.members.len().saturating_add(1));
        // This node is always in its own view. A one-member view is healthy, and
        // a node has no silence to measure against itself.
        view.push(ClusterMemberInfo {
            id: self.inner.node_id.clone(),
            addr: self.inner.advertise_addr.clone(),
            status: ClusterMemberStatus::Alive,
            incarnation,
        });

        for (id, record) in &state.members {
            // Replicated `Left` is a tombstone, not a member; the local overlay
            // then removes whoever has gone silent past the suspicion timeout.
            // The view is exactly that intersection.
            if id == &self.inner.node_id || record.status != membership::MemberStatus::Alive {
                continue;
            }
            let liveness = overlay.liveness(id, now);
            if !liveness.in_view() {
                continue;
            }
            view.push(ClusterMemberInfo {
                id: id.clone(),
                addr: record.addr.clone(),
                status: if liveness == membership::Liveness::Suspect {
                    ClusterMemberStatus::Suspect
                } else {
                    ClusterMemberStatus::Alive
                },
                incarnation: record.incarnation,
            });
        }
        drop(overlay);
        drop(state);
        view
    }

    /// Handle onto the cluster-wide grow-only counter called `name`.
    #[must_use]
    pub fn counter(&self, name: &str) -> ClusterCounter {
        ClusterCounter::new(Arc::clone(&self.inner), name.to_owned())
    }

    /// This node's current incarnation.
    ///
    /// Test-only: the number is an implementation detail of refutation, and the
    /// operator-facing view of it is the `incarnation` field of each row in the
    /// `cluster:membership` health component.
    #[cfg(test)]
    pub(crate) fn incarnation(&self) -> u64 {
        self.inner.incarnation.load(Ordering::Relaxed)
    }

    /// Total frames refused by this node's verifier, for any reason.
    ///
    /// Test-only: production reads the same counters *with their `reason`
    /// label* through `autumn_cluster_frames_rejected_total`, which is the
    /// series an operator can actually act on.
    #[cfg(test)]
    pub(crate) fn frames_rejected_total(&self) -> u64 {
        self.inner.metrics.rejected_total()
    }
}

/// How many suspicion timeouts an inbound connection may stay silent before the
/// transport closes it. Well past the point where its peer is out of the view.
const INBOUND_IDLE_SUSPICION_MULTIPLE: u32 = 4;

/// Registration name of the cluster health component and of the cluster
/// metrics source. `curl /actuator/health | jq '.components["cluster:membership"]'`.
pub(crate) const MEMBERSHIP_COMPONENT: &str = "cluster:membership";

/// The `cluster:membership` health component.
///
/// Registered in [`IndicatorGroup::HealthOnly`](crate::actuator::IndicatorGroup)
/// and **always `UP`**: it reports the local view, it never grades it. A
/// one-member view is a healthy view — a node that reported `DOWN` because its
/// peer went away would hand an orchestrator a reason to restart the last
/// survivor, which is the one action guaranteed to make the outage total.
struct ClusterHealthIndicator {
    handle: ClusterHandle,
}

impl ClusterHealthIndicator {
    /// The details map, exactly as documented in `docs/guide/clustering.md`.
    fn snapshot(&self) -> crate::actuator::HealthCheckOutput {
        let members = self.handle.members();
        let rows: Vec<serde_json::Value> = members
            .iter()
            .map(|member| {
                serde_json::json!({
                    "id": member.id,
                    "addr": member.addr,
                    "status": member.status.to_string(),
                    "incarnation": member.incarnation,
                })
            })
            .collect();

        let details = std::collections::HashMap::from([
            (
                "node_id".to_owned(),
                serde_json::json!(self.handle.node_id()),
            ),
            (
                "cluster".to_owned(),
                serde_json::json!(self.handle.cluster_name()),
            ),
            (
                "local_addr".to_owned(),
                serde_json::json!(self.handle.local_addr().to_string()),
            ),
            ("member_count".to_owned(), serde_json::json!(members.len())),
            ("members".to_owned(), serde_json::Value::Array(rows)),
        ]);
        crate::actuator::HealthCheckOutput::up().with_details(details)
    }
}

impl crate::actuator::HealthIndicator for ClusterHealthIndicator {
    fn check(&self) -> futures::future::BoxFuture<'_, crate::actuator::HealthCheckOutput> {
        // Reading the view is two uncontended mutex acquisitions and a clone;
        // there is nothing to await, so the check can never trip the registry's
        // per-indicator timeout.
        Box::pin(std::future::ready(self.snapshot()))
    }

    fn group(&self) -> crate::actuator::IndicatorGroup {
        crate::actuator::IndicatorGroup::HealthOnly
    }
}

/// The `autumn_cluster_*` families on `/actuator/metrics` and
/// `/actuator/prometheus`, per the table in `docs/guide/clustering.md`.
struct ClusterMetricsSource {
    handle: ClusterHandle,
}

impl crate::actuator::MetricsSource for ClusterMetricsSource {
    #[allow(
        clippy::cast_precision_loss,
        reason = "Prometheus values are f64 by definition; these counters would \
                  have to pass 2^53 frames to lose a unit"
    )]
    fn collect(&self) -> Vec<crate::actuator::MetricFamily> {
        use crate::actuator::{MetricFamily, MetricKind, MetricSample};

        let metrics = &self.handle.inner.metrics;
        let unlabelled = |value: u64| {
            vec![MetricSample {
                labels: Vec::new(),
                value: value as f64,
            }]
        };

        vec![
            MetricFamily {
                name: "autumn_cluster_members".to_owned(),
                help: "Members in this node's local cluster view.".to_owned(),
                kind: MetricKind::Gauge,
                samples: unlabelled(self.handle.members().len() as u64),
            },
            MetricFamily {
                name: "autumn_cluster_pushes_sent_total".to_owned(),
                help: "State pushes handed to the cluster transport.".to_owned(),
                kind: MetricKind::Counter,
                samples: unlabelled(metrics.pushes_sent.load(Ordering::Relaxed)),
            },
            MetricFamily {
                name: "autumn_cluster_pushes_unsendable_total".to_owned(),
                help: "Outbound cluster messages that could not be signed or framed \
                       (almost always a document past the 64 KiB frame cap)."
                    .to_owned(),
                kind: MetricKind::Counter,
                samples: unlabelled(metrics.pushes_unsendable.load(Ordering::Relaxed)),
            },
            MetricFamily {
                name: "autumn_cluster_pushes_received_total".to_owned(),
                help: "State pushes accepted from peers after verification.".to_owned(),
                kind: MetricKind::Counter,
                samples: unlabelled(metrics.pushes_received.load(Ordering::Relaxed)),
            },
            MetricFamily {
                name: "autumn_cluster_merges_applied_total".to_owned(),
                help: "Merges that changed this node's replicated document.".to_owned(),
                kind: MetricKind::Counter,
                samples: unlabelled(metrics.merges_applied.load(Ordering::Relaxed)),
            },
            MetricFamily {
                name: "autumn_cluster_frames_dropped_total".to_owned(),
                help: "Frames the transport could not queue for a peer.".to_owned(),
                kind: MetricKind::Counter,
                samples: unlabelled(metrics.frames_dropped.load(Ordering::Relaxed)),
            },
            MetricFamily {
                name: "autumn_cluster_frames_rejected_total".to_owned(),
                help: "Inbound frames refused by the verifier, by reason.".to_owned(),
                kind: MetricKind::Counter,
                samples: metrics
                    .rejections_by_reason()
                    .into_iter()
                    .map(|(reason, count)| MetricSample {
                        labels: vec![("reason".to_owned(), reason.to_owned())],
                        value: count as f64,
                    })
                    .collect(),
            },
        ]
    }
}

/// Install the cluster control plane from `[cluster]` configuration.
///
/// Mirrors [`crate::alerts::install_from_config`]: a no-op when the section is
/// disabled, a hard boot error when it is enabled but cannot start, and on
/// success a [`ClusterHandle`] inserted as an [`AppState`] extension, plus the
/// `cluster:membership` health component and the `autumn_cluster_*` metric
/// families.
///
/// # Errors
///
/// Returns an error when the section is enabled but
///
/// - a [`ClusterHandle`] is already installed on `state`. One node per app: a
///   second install would run a second set of loops behind an actuator that
///   still reports the first;
/// - the section violates any [`ClusterConfig::validate`] rule. The installer
///   runs that validation itself, before it binds anything: it is public and
///   reachable with a hand-built section that never went through the config
///   layer;
/// - the shared secret is absent or shorter than 16 bytes — checked again here
///   and a third time inside the node, because falling back to an empty HMAC
///   key would authenticate every peer on the port;
/// - the cluster listener cannot bind `bind_addr`, or the node cannot start. A
///   node that cannot join must not boot pretending it did;
/// - the app has already registered a health indicator or a metrics source
///   called `cluster:membership`. The cluster's own actuator surface would be
///   shadowed by an unrelated component, leaving the membership view and the
///   rejection counters invisible; the just-started node is cancelled and the
///   boot fails rather than running unobservably.
pub fn install_from_config(
    state: &AppState,
    config: &ClusterConfig,
    shutdown: &CancellationToken,
) -> AutumnResult<()> {
    if !config.enabled {
        return Ok(());
    }

    // One node per app. A second install would bind a second listener, start a
    // second set of loops, and replace the extension — leaving the actuator
    // reporting the first node (its health component and metrics source are
    // registered under a name that is now taken, and those registrations fail)
    // while application code reads the second. Two nodes in one *process* are
    // fine and the tests do exactly that; they have one `AppState` each.
    if state.extension::<ClusterHandle>().is_some() {
        return Err(AutumnError::internal_server_error_msg(
            "cluster: a ClusterHandle is already installed on this AppState — \
             install_from_config must be called once per app, and a second node \
             needs its own AppState",
        ));
    }

    // The whole `[cluster]` rule set, re-checked here and before anything is
    // bound. `AutumnConfig::validate` already runs it at boot, but this
    // installer is public and reachable with a hand-built section that never
    // went through the config layer — and every rule it checks (a dialable
    // advertised address, a suspicion timeout that cannot flap, a `node_id`
    // free of the cell-key separator) is one this node would otherwise carry
    // into the cluster.
    config.validate().map_err(|error| {
        AutumnError::internal_server_error_msg(format!("cluster: invalid configuration: {error}"))
    })?;

    // Deliberately not `unwrap_or_default()`: an absent secret is a
    // configuration error, never an empty key.
    let Some(secret) = config.secret.as_ref() else {
        return Err(AutumnError::internal_server_error_msg(
            "cluster.secret is required when cluster.enabled = true: the cluster transport is \
             authenticated (HMAC-SHA256) and has no unauthenticated mode — set it with \
             AUTUMN_CLUSTER__SECRET",
        ));
    };
    let secret = secret.expose_secret().as_bytes().to_vec();

    let suspicion_timeout = Duration::from_millis(config.suspicion_timeout_ms);
    // A peer silent for longer than its suspicion timeout is already out of the
    // view, so closing its idle socket costs nothing a live peer would miss —
    // while an unauthenticated socket that never completes a frame stops being
    // free to hold. The floor keeps a fast cluster from recycling connections.
    let transport = transport::TcpPeerTransport::bind(&config.bind_addr)?
        .with_inbound_idle_timeout(
            suspicion_timeout
                .saturating_mul(INBOUND_IDLE_SUSPICION_MULTIPLE)
                .max(transport::DEFAULT_INBOUND_IDLE_TIMEOUT),
        );

    let runtime = node::ClusterRuntimeConfig {
        cluster_name: config.cluster_name.clone(),
        secret,
        node_id: config.node_id.clone(),
        advertise_addr: config.advertise_addr.clone(),
        seed_peers: config.seed_peers.clone(),
        push_interval: Duration::from_millis(config.push_interval_ms),
        suspicion_timeout,
    };

    // A short secret is refused inside `ClusterNode::start`, before any frame is
    // signed — same rule as `ClusterConfig::validate`, enforced at the seam that
    // actually holds the key.
    let node_shutdown = shutdown.child_token();
    let handle = node::ClusterNode::start(
        runtime,
        state.entropy_arc(),
        state.clock_arc(),
        node_shutdown.clone(),
        Arc::new(transport),
    )?;

    // Either registration failing means the app already owns the
    // `cluster:membership` name — the duplicate-install guard above has already
    // taken the "installer called twice" case. That is a hard error, not a
    // warning: the node would bind, gossip and resolve through the extension
    // while `/actuator/health` and `/actuator/metrics` described somebody
    // else's component, so an operator watching the membership view or
    // `autumn_cluster_frames_rejected_total` would see nothing at all while
    // this node was partitioned or under attack. Booting is refused instead,
    // and the node that was just started is cancelled on the way out so a
    // refused install leaves no loops and no listener behind.
    let registered = state
        .health_indicator_registry()
        .register(
            MEMBERSHIP_COMPONENT,
            crate::actuator::IndicatorGroup::HealthOnly,
            Arc::new(ClusterHealthIndicator {
                handle: handle.clone(),
            }),
        )
        .and_then(|()| {
            state.metrics_source_registry().register(
                MEMBERSHIP_COMPONENT,
                Arc::new(ClusterMetricsSource {
                    handle: handle.clone(),
                }),
            )
        });
    if let Err(error) = registered {
        node_shutdown.cancel();
        return Err(AutumnError::internal_server_error_msg(format!(
            "cluster: {error} — the {MEMBERSHIP_COMPONENT} name is reserved for the cluster's \
             own health component and metrics source; rename the app's registration, because a \
             cluster whose membership and rejection counters are invisible cannot be operated"
        )));
    }

    state.insert_extension(handle);
    Ok(())
}

/// The lowest percentage of a base interval a jittered draw can return.
const JITTER_FLOOR_PERCENT: u64 = 80;

/// Width of the jitter window in percentage points: `80..=120`, i.e. ±20%.
const JITTER_SPAN_PERCENT: u64 = 41;

/// `base` ± 20%, drawn from the injected [`Entropy`](crate::entropy::Entropy).
///
/// *Separation*, in the flocking sense: two identically configured nodes must
/// not fly in lockstep and collide on the wire. The same draw shapes the
/// transport's reconnect backoff, so two nodes that lost each other do not
/// resynchronize into a dial storm.
///
/// Never returns zero — a zero-length sleep would spin the loop that awaits it.
pub(crate) fn jittered(base: Duration, entropy: &dyn crate::entropy::Entropy) -> Duration {
    let base_ms = u64::try_from(base.as_millis()).unwrap_or(u64::MAX);
    let draw = entropy
        .next_u64()
        .checked_rem(JITTER_SPAN_PERCENT)
        .unwrap_or(0);
    let percent = JITTER_FLOOR_PERCENT.saturating_add(draw);
    let millis = base_ms
        .saturating_mul(percent)
        .checked_div(100)
        .unwrap_or(base_ms);
    Duration::from_millis(millis.max(1))
}

/// Build the boot error used when the cluster listener cannot be bound.
pub(crate) fn bind_error(addr: &str, err: &std::io::Error) -> AutumnError {
    AutumnError::internal_server_error_msg(format!(
        "cluster: failed to bind the cluster listener on {addr}: {err}"
    ))
}
