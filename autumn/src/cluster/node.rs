//! The running node: identity, the push loop, the receive loop, and departure.
//!
//! [`ClusterNode::start`] is deliberately **app-independent** — it takes a
//! runtime config, an [`Entropy`](crate::entropy::Entropy), a
//! [`ClockSource`](crate::time::ClockSource), a [`CancellationToken`] and a
//! transport, and nothing else. That is what makes two whole nodes constructible
//! inside one test process, which in turn is what makes the deterministic
//! two-node suite possible at all. No process globals, ever.
//!
//! # Loops
//!
//! - **Push loop.** Every `push_interval` (± an entropy-drawn jitter, so two
//!   identically-configured nodes never lock step) it signs and sends the whole
//!   document to every known peer. The push *is* the heartbeat.
//! - **Receive loop.** Decode → verify → merge → update the local liveness
//!   overlay. Malformed input is dropped and counted; the loop never exits on a
//!   bad frame.
//! - **Departure.** Both loops select on the cancellation token. The cancel arm
//!   sends a best-effort `Leave` over existing connections within a bounded
//!   budget (≤ 250 ms, inside the app's drain budget) and then exits. `Leave` is
//!   only the fast path — the suspicion timeout is the correctness path.
//!
//! RED PHASE (TDD): [`ClusterNode::start`] builds a real, inert node — identity,
//! document, overlay and handle are constructed; no loop is spawned yet.

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

use std::sync::Arc;
use std::sync::atomic::AtomicU64;
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use super::membership::{ClusterState, LivenessOverlay, MemberRecord};
use super::transport::PeerTransport;
use super::{ClusterHandle, ClusterInner, ClusterMetrics};
use crate::entropy::Entropy;
use crate::time::{ClockSource, clock_unix_secs};
use crate::{AutumnResult, cluster::NodeId};

/// Upper bound on the best-effort `Leave` broadcast during shutdown. Sized to
/// sit comfortably inside the app's drain budget: a clean departure must never
/// extend shutdown.
pub(crate) const LEAVE_BUDGET: Duration = Duration::from_millis(250);

/// Everything a node needs that does not come from the app.
///
/// Built from [`ClusterConfig`](crate::config::ClusterConfig) by
/// [`install_from_config`](super::install_from_config), or by hand in tests.
#[derive(Debug, Clone)]
pub(crate) struct ClusterRuntimeConfig {
    /// Cluster name; part of every MAC, so two clusters cannot cross-talk.
    pub(crate) cluster_name: String,
    /// Shared HMAC secret.
    pub(crate) secret: Vec<u8>,
    /// Explicit node id override; entropy-derived when absent.
    pub(crate) node_id: Option<String>,
    /// Address advertised to peers; the bound address when absent.
    pub(crate) advertise_addr: Option<String>,
    /// Addresses to dial on startup.
    pub(crate) seed_peers: Vec<String>,
    /// Base interval between state pushes.
    pub(crate) push_interval: Duration,
    /// How long without a push before a peer becomes `Suspect`.
    pub(crate) suspicion_timeout: Duration,
}

/// Constructor for a running cluster node.
pub(crate) struct ClusterNode;

impl ClusterNode {
    /// Start a node on `transport` and return its handle.
    ///
    /// # Errors
    ///
    /// Returns an error when the node cannot be constructed on the given
    /// transport.
    pub(crate) fn start(
        config: ClusterRuntimeConfig,
        entropy: Arc<dyn Entropy>,
        clock: Arc<dyn ClockSource>,
        shutdown: CancellationToken,
        transport: Arc<dyn PeerTransport>,
    ) -> AutumnResult<ClusterHandle> {
        let node_id = resolve_node_id(config.node_id.as_deref(), entropy.as_ref());
        let local_addr = transport.local_addr();
        let advertise_addr = config
            .advertise_addr
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map_or_else(|| local_addr.to_string(), ToOwned::to_owned);

        // Clock-seeded so a restart with a configured static node id always
        // comes back with a HIGHER incarnation than the record peers remember.
        let incarnation = clock_unix_secs(clock.as_ref());

        let mut state = ClusterState::default();
        state.members.insert(
            node_id.clone(),
            MemberRecord::alive(advertise_addr.clone(), incarnation),
        );

        let inner = Arc::new(ClusterInner {
            node_id,
            cluster_name: config.cluster_name,
            local_addr,
            advertise_addr,
            secret: config.secret,
            seed_peers: config.seed_peers,
            push_interval: config.push_interval,
            suspicion_timeout: config.suspicion_timeout,
            incarnation: AtomicU64::new(incarnation),
            state: std::sync::Mutex::new(state),
            overlay: std::sync::Mutex::new(LivenessOverlay::new(
                config.push_interval,
                config.suspicion_timeout,
            )),
            clock,
            entropy,
            transport,
            shutdown,
            notify: tokio::sync::Notify::new(),
            metrics: ClusterMetrics::default(),
        });

        // RED-PHASE STUB: the green phase spawns the push loop, the receive
        // loop and (for the TCP transport) the accept loop here — each on
        // `inner.shutdown.child_token()`, each `tokio::select!`ing on
        // `token.cancelled()`, with the cancel arm sending a bounded
        // `LEAVE_BUDGET` departure notice before it returns.
        Ok(ClusterHandle::from_inner(inner))
    }
}

/// The node's stable identity: the configured override when it is non-empty,
/// otherwise an entropy-derived id.
///
/// Never hostname-derived: hostnames collide across containers and are not a
/// uniqueness guarantee.
pub(crate) fn resolve_node_id(configured: Option<&str>, entropy: &dyn Entropy) -> NodeId {
    match configured.map(str::trim).filter(|s| !s.is_empty()) {
        Some(id) => id.to_owned(),
        None => format!("node-{}", entropy.uuid_v4().simple()),
    }
}
