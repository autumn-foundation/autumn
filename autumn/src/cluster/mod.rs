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
//!   [`ClockSource`](crate::time::ClockSource). Views are therefore local and
//!   **eventually consistent** by construction.
//! - **Exactly one distributed primitive.** A cluster-wide grow-only counter
//!   ([`ClusterCounter`]), convergent (CRDT) because each node writes only its
//!   own `(node, boot)` cell and merge is per-cell max.
//!
//! The transport is **authenticated (HMAC-SHA256) but not encrypted**, and the
//! counter's lifetime is the cluster's process lifetime — see
//! `docs/guide/clustering.md` for the full failure-semantics contract.
//!
//! # Naming
//!
//! `[cluster]` here means *app nodes clustering with each other*. It is
//! unrelated to [`crate::sharding`]'s Redis-Cluster-style **database** shard
//! vocabulary.
//!
//! # Status of this file
//!
//! RED PHASE (TDD): the type surface below is final-shaped but the bodies are
//! inert. Every entry point compiles, runs, and returns an empty/zero answer so
//! the red tests fail by *assertion*, never by panic.

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

/// Counters that make cluster behaviour observable from `/actuator/metrics`
/// and from tests, without asserting on message counts.
#[derive(Debug, Default)]
pub(crate) struct ClusterMetrics {
    /// Frames refused by the verifier for any reason (bad MAC, wrong cluster,
    /// replay, oversized, malformed, self-origin).
    pub(crate) frames_rejected: AtomicU64,
    /// Remote documents successfully merged into the local one.
    pub(crate) merges_applied: AtomicU64,
    /// State pushes handed to the transport.
    pub(crate) pushes_sent: AtomicU64,
    /// State pushes accepted from peers.
    pub(crate) pushes_received: AtomicU64,
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
    pub(crate) suspicion_timeout: Duration,
    /// This process's incarnation; bumped when refuting a stale `Left`.
    pub(crate) incarnation: AtomicU64,
    /// The single replicated document (members + counters).
    pub(crate) state: Mutex<membership::ClusterState>,
    /// Local, never-replicated failure detector.
    pub(crate) overlay: Mutex<membership::LivenessOverlay>,
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
}

/// Handle onto the running cluster node.
///
/// Installed on [`AppState`] as an extension when `[cluster] enabled = true`;
/// `state.extension::<ClusterHandle>()` is `None` on every node where
/// clustering is off.
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
        // RED-PHASE STUB: the real view folds the replicated member table
        // through the local liveness overlay.
        Vec::new()
    }

    /// Handle onto the cluster-wide grow-only counter called `name`.
    #[must_use]
    pub fn counter(&self, name: &str) -> ClusterCounter {
        ClusterCounter::new(Arc::clone(&self.inner), name.to_owned())
    }

    /// This node's current incarnation.
    pub(crate) fn incarnation(&self) -> u64 {
        self.inner.incarnation.load(Ordering::Relaxed)
    }

    /// Total frames refused by this node's verifier, for any reason.
    pub(crate) fn frames_rejected_total(&self) -> u64 {
        self.inner.metrics.frames_rejected.load(Ordering::Relaxed)
    }
}

/// Install the cluster control plane from `[cluster]` configuration.
///
/// Mirrors [`crate::alerts::install_from_config`]: a no-op when the section is
/// disabled, a hard boot error when it is enabled but cannot bind, and on
/// success a [`ClusterHandle`] inserted as an [`AppState`] extension.
///
/// # Errors
///
/// Returns an error when the cluster listener cannot bind `bind_addr`. A node
/// that cannot join must not boot pretending it did.
pub fn install_from_config(
    state: &AppState,
    config: &ClusterConfig,
    shutdown: &CancellationToken,
) -> AutumnResult<()> {
    if !config.enabled {
        return Ok(());
    }

    let secret = config
        .secret
        .as_ref()
        .map(|s| s.expose_secret().as_bytes().to_vec())
        .unwrap_or_default();

    let transport = transport::TcpPeerTransport::bind(&config.bind_addr)?;

    let runtime = node::ClusterRuntimeConfig {
        cluster_name: config.cluster_name.clone(),
        secret,
        node_id: config.node_id.clone(),
        advertise_addr: config.advertise_addr.clone(),
        seed_peers: config.seed_peers.clone(),
        push_interval: Duration::from_millis(config.push_interval_ms),
        suspicion_timeout: Duration::from_millis(config.suspicion_timeout_ms),
    };

    let handle = node::ClusterNode::start(
        runtime,
        state.entropy_arc(),
        state.clock_arc(),
        shutdown.child_token(),
        Arc::new(transport),
    )?;

    // RED-PHASE STUB: the green phase also registers the `cluster:membership`
    // health indicator (HealthOnly — a one-member view is UP, never DOWN) and
    // the cluster metrics source here.
    state.insert_extension(handle);
    Ok(())
}

/// Build the boot error used when the cluster listener cannot be bound.
pub(crate) fn bind_error(addr: &str, err: &std::io::Error) -> AutumnError {
    AutumnError::internal_server_error_msg(format!(
        "cluster: failed to bind the cluster listener on {addr}: {err}"
    ))
}
