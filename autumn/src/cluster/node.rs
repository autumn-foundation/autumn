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
//!   sends this node's final document (its own record marked `Left`, carrying
//!   every counter cell written since the last push) followed by a `Leave`,
//!   over existing connections and within a bounded budget (≤ 250 ms, inside
//!   the app's drain budget), and then exits. That whole path is only the fast
//!   one — the suspicion timeout is the correctness path.

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
// `pub` throughout this file is crate-visible only: the enclosing `cluster`
// submodule is itself `pub(crate)`, so nothing here escapes the crate
// (clippy::redundant_pub_crate).

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use tokio_util::sync::CancellationToken;

use super::counter::CELL_SEPARATOR;
use super::membership::{ClusterState, LivenessOverlay, MemberRecord};
use super::transport::{IncomingFrames, PeerTransport};
use super::wire::{ClusterMessage, Envelope, FrameVerifier};
use super::{ClusterHandle, ClusterInner, ClusterMetrics, Incarnation, jittered, wire};
use crate::config::MIN_CLUSTER_SECRET_LEN;
use crate::entropy::Entropy;
use crate::time::{ClockSource, MonotonicInstant, clock_unix_duration};
use crate::{AutumnError, AutumnResult, cluster::NodeId};

/// Upper bound on the best-effort `Leave` broadcast during shutdown. Sized to
/// sit comfortably inside the app's drain budget: a clean departure must never
/// extend shutdown.
pub const LEAVE_BUDGET: Duration = Duration::from_millis(250);

/// How often the departure flush re-checks the transport's outbound queues.
/// Only ever polled inside [`LEAVE_BUDGET`].
const LEAVE_FLUSH_POLL: Duration = Duration::from_millis(5);

/// Fraction of the push interval that bounds notify-driven pushes: at most one
/// prompt push per `push_interval / PUSH_FLOOR_DIVISOR`.
const PUSH_FLOOR_DIVISOR: u32 = 4;

/// Lower bound on that fraction, so a short `push_interval` still spaces prompt
/// pushes by something a write-heavy handler cannot outrun. Clamped back to the
/// interval itself when the interval is shorter still: a prompt push must never
/// be rarer than the periodic one.
const PUSH_FLOOR_MIN: Duration = Duration::from_millis(50);

/// Minimum gap between two "this node cannot send" warnings.
///
/// The failure is permanent once it starts (the document only grows), and the
/// push loop retries every interval, so an unthrottled warning would be a log
/// flood at gossip rate. One line per minute is enough to be noticed and few
/// enough to be kept.
const UNSENDABLE_WARN_INTERVAL: Duration = Duration::from_secs(60);

/// Everything a node needs that does not come from the app.
///
/// Built from [`ClusterConfig`](crate::config::ClusterConfig) by
/// [`install_from_config`](super::install_from_config), or by hand in tests.
///
/// [`Debug`] is hand-written and **omits the secret**: the key is copied in
/// here out of a [`SecretString`](secrecy::SecretString) as a plain
/// `Vec<u8>`, and a derived `Debug` would print it in full the first time
/// somebody added `?config` to a boot log line.
#[derive(Clone)]
pub struct ClusterRuntimeConfig {
    /// Cluster name; part of every MAC, so two clusters cannot cross-talk.
    pub cluster_name: String,
    /// Shared HMAC secret.
    pub secret: Vec<u8>,
    /// Explicit node id override; entropy-derived when absent.
    pub node_id: Option<String>,
    /// Address advertised to peers; the bound address when absent.
    pub advertise_addr: Option<String>,
    /// Addresses to dial on startup.
    pub seed_peers: Vec<String>,
    /// Base interval between state pushes.
    pub push_interval: Duration,
    /// How long without a push before a peer becomes `Suspect`.
    pub suspicion_timeout: Duration,
}

impl std::fmt::Debug for ClusterRuntimeConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never `secret`, not even its length: a key that reaches a log line is
        // a key that has to be rotated.
        f.debug_struct("ClusterRuntimeConfig")
            .field("cluster_name", &self.cluster_name)
            .field("node_id", &self.node_id)
            .field("advertise_addr", &self.advertise_addr)
            .field("seed_peers", &self.seed_peers)
            .field("push_interval", &self.push_interval)
            .field("suspicion_timeout", &self.suspicion_timeout)
            .finish_non_exhaustive()
    }
}

/// Constructor for a running cluster node.
pub struct ClusterNode;

impl ClusterNode {
    /// Start a node on `transport` and return its handle.
    ///
    /// Spawns three things, each on a child of `shutdown`: the transport's own
    /// I/O ([`PeerTransport::start`] — the accept loop and per-peer writers for
    /// TCP, nothing at all for an in-process transport), the jittered push loop,
    /// and the receive pipeline.
    ///
    /// # Errors
    ///
    /// Returns an error when the node cannot be constructed:
    ///
    /// - called outside a Tokio runtime, so no loop could be spawned — a node
    ///   that silently never gossips is worse than one that refuses to boot;
    /// - the shared secret is absent or shorter than
    ///   `MIN_CLUSTER_SECRET_LEN`. This mirrors
    ///   [`ClusterConfig::validate`](crate::config::ClusterConfig::validate) as
    ///   defence in depth, so a hand-built config can never authenticate with an
    ///   empty key;
    /// - the cluster name or node id is empty, or contains the `#` reserved as
    ///   the counter cell-key separator, which would make cell keys ambiguous;
    /// - the push interval is zero, which would spin the push loop;
    /// - the transport's inbound stream has already been taken.
    pub fn start(
        config: ClusterRuntimeConfig,
        entropy: Arc<dyn Entropy>,
        clock: Arc<dyn ClockSource>,
        shutdown: CancellationToken,
        transport: Arc<dyn PeerTransport>,
    ) -> AutumnResult<ClusterHandle> {
        // Captured rather than used through `tokio::spawn`, which panics outside
        // a runtime — the panic gate forbids that, and a caller deserves an
        // error it can attribute.
        let runtime = tokio::runtime::Handle::try_current().map_err(|err| {
            boot_error(format!(
                "ClusterNode::start must be called from within a Tokio runtime: {err}"
            ))
        })?;

        let node_id = resolve_node_id(config.node_id.as_deref(), entropy.as_ref());
        validate(&config, &node_id)?;

        let incoming = transport.take_incoming().ok_or_else(|| {
            boot_error("the transport's inbound frame stream has already been taken")
        })?;

        let local_addr = transport.local_addr();
        let advertise_addr = config
            .advertise_addr
            .as_deref()
            .map(str::trim)
            .filter(|s| !s.is_empty())
            // No `advertise_addr`: advertise the address actually bound, so a
            // `bind_addr` with port 0 publishes its RESOLVED ephemeral port
            // rather than a port nobody can dial.
            .map_or_else(|| local_addr.to_string(), ToOwned::to_owned);

        // Clock-seeded from Unix MILLISECONDS so a restart with a configured
        // static node id always comes back with a strictly higher incarnation
        // than the record peers remember. Second granularity is not enough: two
        // boots inside one second would mint a byte-identical self-record, and
        // refutation's exact-echo rule would then read a leftover record from
        // the dead boot as this boot's own echo.
        let incarnation = seed_incarnation(clock.as_ref());

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
            incarnation: AtomicU64::new(incarnation),
            state: std::sync::Mutex::new(state),
            overlay: std::sync::Mutex::new(LivenessOverlay::new(
                config.push_interval,
                config.suspicion_timeout,
            )),
            pruned_senders: std::sync::Mutex::new(BTreeSet::new()),
            clock,
            entropy,
            transport,
            shutdown,
            notify: tokio::sync::Notify::new(),
            metrics: ClusterMetrics::default(),
        });

        inner.transport.start(&inner.shutdown, &inner.entropy);
        runtime.spawn(push_loop(Arc::clone(&inner), inner.shutdown.child_token()));
        runtime.spawn(receive_loop(
            Arc::clone(&inner),
            incoming,
            inner.shutdown.child_token(),
        ));

        Ok(ClusterHandle::from_inner(inner))
    }
}

/// A boot error from the cluster node, in the shape the installer propagates.
fn boot_error(message: impl std::fmt::Display) -> AutumnError {
    AutumnError::internal_server_error_msg(format!("cluster: {message}"))
}

/// The invariants the loops below assume, checked once at boot.
///
/// Deliberately duplicated from
/// [`ClusterConfig::validate`](crate::config::ClusterConfig::validate): this
/// constructor is reachable with a hand-built config that never went through the
/// config layer, and a missing secret must fail closed rather than authenticate
/// every peer with an empty key.
fn validate(config: &ClusterRuntimeConfig, node_id: &str) -> AutumnResult<()> {
    let secret_len = config.secret.len();
    if secret_len < MIN_CLUSTER_SECRET_LEN {
        return Err(boot_error(format!(
            "the shared secret must be at least {MIN_CLUSTER_SECRET_LEN} bytes, got \
             {secret_len}: there is no unauthenticated mode, and an absent secret must \
             never fall back to an empty key"
        )));
    }
    validate_ident("cluster_name", &config.cluster_name)?;
    validate_ident("node_id", node_id)?;
    if config.push_interval.is_zero() {
        return Err(boot_error(
            "push_interval must be greater than zero: a zero interval spins the push loop",
        ));
    }
    Ok(())
}

/// A cluster/node identifier must be non-empty and free of the cell separator.
fn validate_ident(field: &str, value: &str) -> AutumnResult<()> {
    if value.trim().is_empty() {
        return Err(boot_error(format!("{field} must not be empty")));
    }
    if value.contains(CELL_SEPARATOR) {
        return Err(boot_error(format!(
            "{field} must not contain {CELL_SEPARATOR:?} ({value:?}): it separates the node id \
             from the incarnation in every counter cell key"
        )));
    }
    Ok(())
}

/// This boot's incarnation: Unix milliseconds through the injected clock,
/// saturating at `0` for a pre-epoch clock and at [`u64::MAX`] beyond range.
///
/// Milliseconds, not seconds, and the granularity is load-bearing: two boots
/// that mint the same incarnation produce a byte-identical self-record, which
/// [`ClusterState::refute`] is obliged to read as this boot's own echo rather
/// than as the dead boot's leftover — see [`super::membership`].
///
/// **The one caveat**: that argument is probabilistic, not absolute. A clock
/// that is frozen, clamped (a pre-epoch reading saturates to `0`), or stepped
/// backwards onto the exact millisecond a previous boot read will mint an equal
/// incarnation, and the two boots then share a counter cell and a replay
/// watermark until an operator intervenes. Milliseconds make that a
/// coincidence rather than a routine same-second restart; nothing here makes it
/// impossible.
fn seed_incarnation(clock: &dyn ClockSource) -> Incarnation {
    u64::try_from(clock_unix_duration(clock).as_millis()).unwrap_or(u64::MAX)
}

// ── The push loop ────────────────────────────────────────────────────────────

/// Push the whole document to every known peer, forever, on a jittered
/// interval — and depart cleanly when cancelled.
///
/// A local write nudges [`ClusterInner::notify`], so an increment does not wait
/// out the interval it landed in. That nudge is **rate-limited**: every
/// increment leaves a permit behind, so an unthrottled loop would clone, sign
/// and send the whole document once per write — gossip at request rate, which
/// is the one workload most likely to be pushing the document toward the 64 KiB
/// frame cap in the first place. A notified push therefore waits out the
/// remainder of [`push_floor`] before it runs, which bounds prompt pushes to
/// one per floor while keeping propagation latency at the floor rather than at
/// the full interval. The first nudge after a quiet period is never delayed.
async fn push_loop(inner: Arc<ClusterInner>, shutdown: CancellationToken) {
    let mut seq: u64 = 0;
    let mut published = inner.incarnation.load(Ordering::Relaxed);
    let floor = push_floor(inner.push_interval);
    let mut last_push: Option<MonotonicInstant> = None;

    loop {
        let interval = jittered(inner.push_interval, inner.entropy.as_ref());
        tokio::select! {
            () = shutdown.cancelled() => {
                depart(&inner, seq).await;
                return;
            }
            () = inner.notify.notified() => {
                let remaining = last_push
                    .and_then(|last| {
                        let since = inner.clock.monotonic().saturating_duration_since(last);
                        floor.checked_sub(since)
                    })
                    .filter(|remaining| !remaining.is_zero());
                if let Some(remaining) = remaining {
                    tokio::select! {
                        () = tokio::time::sleep(remaining) => {}
                        () = shutdown.cancelled() => {
                            depart(&inner, seq).await;
                            return;
                        }
                    }
                }
            }
            () = tokio::time::sleep(interval) => {}
        }
        push_round(&inner, &mut seq, &mut published);
        last_push = Some(inner.clock.monotonic());
    }
}

/// The minimum spacing between notify-driven pushes: a quarter of the push
/// interval, never under [`PUSH_FLOOR_MIN`] and never over the interval itself.
fn push_floor(push_interval: Duration) -> Duration {
    push_interval
        .checked_div(PUSH_FLOOR_DIVISOR)
        .unwrap_or(push_interval)
        .max(PUSH_FLOOR_MIN)
        .min(push_interval)
}

/// Sign and send one state push to every known peer.
fn push_round(inner: &Arc<ClusterInner>, seq: &mut u64, published: &mut Incarnation) {
    let incarnation = inner.incarnation.load(Ordering::Relaxed);
    if incarnation > *published {
        // A receiver adopts a higher incarnation and RESETS its sequence
        // watermark, so restarting the sequence here is what lets a refutation
        // (or a fresh boot) be heard instead of being replay-dropped.
        *seq = 0;
        *published = incarnation;
    }

    let now = inner.clock.monotonic();
    let (document, targets) = {
        let mut state = inner.lock_state();
        // Locks are always taken state-then-overlay, never the other way round.
        let mut overlay = inner.lock_overlay();
        // Housekeeping, in the one order that gives every record an exit:
        // convert, stamp, prune. A member this node has not heard from for a
        // whole tombstone window becomes a `Left` record at its current
        // incarnation — the only way an `Alive` record ever leaves the
        // document, since pruning collects tombstones alone (see
        // `ClusterState::convert_down_members`). Converting first means the
        // record is stamped on this same round and prunes one window later
        // rather than two.
        let converted = state.convert_down_members(&inner.node_id, &mut overlay, now);
        // Stamp before pruning: a tombstone learned since the last round ages
        // from this observation, not from the departed peer's last receipt
        // (which pruning forgets — see `ClusterState::observe_tombstones`).
        state.observe_tombstones(&mut overlay, now);
        let pruned = state.prune_tombstones(&mut overlay, now);
        drop(overlay);
        if !converted.is_empty() {
            tracing::debug!(
                converted = converted.len(),
                "cluster: members Down for a whole tombstone window recorded as Left"
            );
        }
        if !pruned.is_empty() {
            tracing::debug!(
                pruned = pruned.len(),
                "cluster: pruned expired Left tombstones"
            );
            // Pruning forgets a node WHOLE. The receive loop owns the replay
            // watermarks, so it is handed the ids here: leaving a pruned
            // sender's watermark behind permanently partitions that node if it
            // comes back at a lower incarnation, because the document no longer
            // holds a record it could refute.
            inner.note_pruned_senders(pruned);
        }
        publish_self(&mut state, inner, incarnation);
        let targets = push_targets(&state, inner);
        let document = state.clone();
        drop(state);
        (document, targets)
    };

    // Retire per-peer transport state for addresses that have left the set: a
    // member that returns at a NEW address must not leave its old address's
    // writer running for the life of the process.
    inner.transport.retain_peers(&targets);

    let message = ClusterMessage::StatePush { state: document };
    let mut sent: u64 = 0;
    for target in &targets {
        if send_signed(inner, target, incarnation, *seq, &message) {
            *seq = seq.saturating_add(1);
            sent = sent.saturating_add(1);
        }
    }
    inner.metrics.pushes_sent.fetch_add(sent, Ordering::Relaxed);
    // Both are transport-owned monotonic counters, mirrored rather than
    // incremented: the transport is the only thing that can observe them.
    inner
        .metrics
        .frames_dropped
        .store(inner.transport.dropped_frames(), Ordering::Relaxed);
    inner
        .metrics
        .framing_rejected
        .store(inner.transport.framing_rejections(), Ordering::Relaxed);
}

/// Republish this node's own record at `incarnation`.
///
/// The record we publish is the one [`ClusterState::refute`] compares an
/// incoming record against, so it must be exactly `Alive` at our current
/// incarnation and our advertised address — otherwise every echo of our own
/// record would look like something to refute and the incarnation would ratchet
/// on every push round.
fn publish_self(state: &mut ClusterState, inner: &ClusterInner, incarnation: Incarnation) {
    let own = MemberRecord::alive(inner.advertise_addr.clone(), incarnation);
    match state.members.get_mut(&inner.node_id) {
        Some(existing) if existing.incarnation <= incarnation => *existing = own,
        // A record about us at a HIGHER incarnation is refutation's business,
        // not ours: overwriting it here would lose the argument on the wire.
        Some(_) => {}
        None => {
            state.members.insert(inner.node_id.clone(), own);
        }
    }
}

/// Every address worth pushing to, for this node's configuration and document.
fn push_targets(state: &ClusterState, inner: &ClusterInner) -> BTreeSet<String> {
    target_addresses(
        state,
        &inner.node_id,
        &inner.seed_peers,
        &[inner.advertise_addr.as_str(), &inner.local_addr.to_string()],
    )
}

/// The configured seeds plus every member this node has learned that is still
/// in the document, minus this node's own addresses.
///
/// Seeds stay in the set forever — they are a dial list, not a membership list,
/// and a peer that went away must still be re-dialable when it comes back.
///
/// **Tombstoned (`Left`) members stay targets until their tombstone is pruned.**
/// Dropping them the moment they depart looks like an obvious saving and is a
/// permanent-partition bug: a node that restarts with a *lower* incarnation (a
/// clock that stepped backwards) is replay-dropped by the incumbent, so its
/// only way back is to hear its own `Left` record and refute past it — and it
/// can only hear it if somebody still pushes to it. The cost is bounded: a few
/// dropped frames aimed at a dead address for one tombstone window.
///
/// Pure (no `ClusterInner`) so the rule above is unit-testable without
/// constructing a whole node.
fn target_addresses(
    state: &ClusterState,
    node_id: &str,
    seeds: &[String],
    own_addrs: &[&str],
) -> BTreeSet<String> {
    let mut targets: BTreeSet<String> = seeds
        .iter()
        .map(|seed| seed.trim().to_owned())
        .filter(|seed| !seed.is_empty())
        .collect();

    for (id, record) in &state.members {
        if id == node_id || record.addr.is_empty() {
            continue;
        }
        targets.insert(record.addr.clone());
    }

    // Never gossip at ourselves: a seed list that contains our own address is a
    // configuration a node must survive, not diagnose.
    for own in own_addrs {
        targets.remove(*own);
    }
    targets
}

/// Sign `message` for `target` and hand the frame to the transport. `false`
/// when the message could not be signed or framed — a drop, never a panic.
///
/// Both failures are counted and (rate-limited) logged by
/// [`note_unsendable`]: unlike a transport-level drop they are *permanent*,
/// because the same document is re-serialized on every round.
fn send_signed(
    inner: &ClusterInner,
    target: &str,
    incarnation: Incarnation,
    seq: u64,
    message: &ClusterMessage,
) -> bool {
    let Some(envelope) = wire::sign_envelope(
        &inner.secret,
        &inner.cluster_name,
        &inner.node_id,
        incarnation,
        seq,
        message,
    ) else {
        note_unsendable(inner, None);
        return false;
    };
    let Some(frame) = wire::encode_frame(&envelope) else {
        note_unsendable(inner, wire::encoded_body_len(&envelope));
        return false;
    };
    inner.transport.send(target, frame);
    true
}

/// Count — and at most once per [`UNSENDABLE_WARN_INTERVAL`], warn about — an
/// outbound message that never reached the transport.
///
/// This is the observability hole that made an oversized document a *silent*
/// cluster split: the push is also the heartbeat, so a node whose document has
/// outgrown [`MAX_FRAME_BYTES`](wire::MAX_FRAME_BYTES) keeps merging its peer's
/// pushes and looks healthy to itself while the peer watches it fall silent and
/// evicts it. `frames_dropped` cannot see it (the transport never got the
/// frame) and `frames_rejected` is inbound-only, so it gets a series of its
/// own plus a line naming the serialized size against the cap.
fn note_unsendable(inner: &ClusterInner, serialized_bytes: Option<usize>) {
    inner
        .metrics
        .pushes_unsendable
        .fetch_add(1, Ordering::Relaxed);

    // Rate limited on the injected clock, so a test can drive it and a
    // permanent failure costs one line a minute rather than two a second.
    let now_ms = u64::try_from(inner.clock.monotonic().since_origin().as_millis())
        .unwrap_or(u64::MAX)
        // Offset by one so `0` can mean "never warned" without a second field.
        .saturating_add(1);
    let due = inner
        .metrics
        .unsendable_warned_at_ms
        .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |last| {
            let elapsed_ms =
                u64::try_from(UNSENDABLE_WARN_INTERVAL.as_millis()).unwrap_or(u64::MAX);
            (last == 0 || now_ms.saturating_sub(last) >= elapsed_ms).then_some(now_ms)
        })
        .is_ok();
    if !due {
        return;
    }

    let Some(bytes) = serialized_bytes else {
        tracing::warn!(
            node_id = %inner.node_id,
            unsendable_total = inner.metrics.pushes_unsendable.load(Ordering::Relaxed),
            "cluster: an outbound message could not be serialized at all and was \
             dropped before the transport"
        );
        return;
    };
    tracing::warn!(
        node_id = %inner.node_id,
        serialized_bytes = bytes,
        frame_cap_bytes = wire::MAX_FRAME_BYTES,
        unsendable_total = inner.metrics.pushes_unsendable.load(Ordering::Relaxed),
        "cluster: this node's replicated document no longer fits one frame, so \
         every state push (which is also the heartbeat) is discarded before the \
         transport; peers will evict this node at the suspicion timeout. Counter \
         cells are never pruned — set a stable node_id and keep counter names a \
         small fixed set (see docs/guide/clustering.md, State growth)"
    );
}

/// The cancellation arm: mark ourselves `Left`, push the final document to the
/// peers we already talk to, say `leave`, and give the transport a bounded
/// window to flush.
///
/// **The final document, not just the notice.** Increments that landed between
/// the last push round and cancellation exist only here; a bare `leave` would
/// tombstone this node at the survivor while those cells died with it, and a
/// rolling restart would then re-learn a total that had gone backwards. The
/// state push carries the same departure — our own record is `Left` in it, and
/// `Left` beats `Alive` at equal incarnation — so the `leave` that follows is
/// belt and braces for a peer that has never held a record for us at all.
///
/// Best effort by design. If the process is killed, the network eats the frame,
/// or the peer is mid-reconnect, the peer still converges at the suspicion
/// timeout — which is the actual contract.
async fn depart(inner: &Arc<ClusterInner>, mut seq: u64) {
    let incarnation = inner.incarnation.load(Ordering::Relaxed);
    let (document, targets) = {
        let mut state = inner.lock_state();
        state.members.insert(
            inner.node_id.clone(),
            MemberRecord::left(inner.advertise_addr.clone(), incarnation),
        );
        let targets = push_targets(&state, inner);
        let document = state.clone();
        drop(state);
        (document, targets)
    };

    let farewell = ClusterMessage::StatePush { state: document };
    for target in &targets {
        if send_signed(inner, target, incarnation, seq, &farewell) {
            seq = seq.saturating_add(1);
        }
        if send_signed(inner, target, incarnation, seq, &ClusterMessage::Leave) {
            seq = seq.saturating_add(1);
        }
    }

    // Bounded so a clean departure never extends the app's drain budget. A
    // transport that delivers synchronously reports nothing pending and this
    // costs one poll.
    let flushed = tokio::time::timeout(LEAVE_BUDGET, async {
        while inner.transport.pending_frames() > 0 {
            tokio::time::sleep(LEAVE_FLUSH_POLL).await;
        }
    })
    .await;
    if flushed.is_err() {
        tracing::debug!(
            budget_ms = LEAVE_BUDGET.as_millis(),
            "cluster: departure notice not fully flushed inside its budget; \
             peers converge on the suspicion timeout instead"
        );
    }
}

// ── The receive pipeline ─────────────────────────────────────────────────────

/// Decode → verify → merge → record receipt, forever.
///
/// Nothing in here can end the loop except cancellation or the transport going
/// away: a rejected frame is counted and dropped, and the next frame is read.
async fn receive_loop(
    inner: Arc<ClusterInner>,
    mut incoming: IncomingFrames,
    shutdown: CancellationToken,
) {
    let mut verifier = FrameVerifier::new(
        inner.cluster_name.clone(),
        inner.node_id.clone(),
        inner.secret.clone(),
    );

    loop {
        let received = tokio::select! {
            item = incoming.recv() => item,
            () = shutdown.cancelled() => None,
        };
        let Some((from, frame)) = received else {
            return;
        };

        // A member the push loop pruned is forgotten WHOLE: its replay
        // watermark goes with its tombstone. Draining here rather than in the
        // push round is what keeps the verifier unshared — it stays a local of
        // this loop, and the two loops never hold each other's state.
        for pruned in inner.take_pruned_senders() {
            verifier.forget(&pruned);
        }

        match verifier.accept(&frame) {
            Ok((envelope, message)) => apply(&inner, &envelope, &from, &message),
            Err(reason) => {
                // Counted per reason: `frames_rejected_total{reason="mac"}`
                // climbing is somebody talking to the port with the wrong
                // secret, which is a different alert from a replay or a
                // truncated frame.
                inner.metrics.record_rejection(reason);
                if !reason.authenticated() {
                    // Reported back to the transport, which owns the socket and
                    // cannot make this judgement itself: the frame never proved
                    // knowledge of the secret, so the connection it arrived on
                    // has not earned the inbound slot it is holding. Without
                    // this the cap is a budget anyone who can reach the port
                    // can exhaust — one well-framed garbage frame per idle
                    // window holds a slot for as long as the sender likes, and
                    // real peers are refused at the cap.
                    inner.transport.note_unauthenticated_frame(&from);
                }
                tracing::debug!(
                    peer = %from,
                    reason = reason.label(),
                    closes_connection = reason.closes_connection(),
                    rejected_total = verifier.rejected_total(),
                    "cluster: inbound frame rejected"
                );
            }
        }
    }
}

/// Apply one **authenticated** message and record its receipt.
fn apply(
    inner: &Arc<ClusterInner>,
    envelope: &Envelope,
    peer_addr: &str,
    message: &ClusterMessage,
) {
    let now = inner.clock.monotonic();
    let refuted = {
        let mut state = inner.lock_state();
        let outcome = match message {
            ClusterMessage::StatePush { state: theirs } => {
                let before = state.clone();
                {
                    // Document first, overlay second — the one lock ordering
                    // rule, same as the push loop's. The merge reads (never
                    // writes) the overlay's recently-pruned memory, so a
                    // straggler cannot teach back a tombstone this node has
                    // already collected: see `ClusterState::merge`.
                    let overlay = inner.lock_overlay();
                    state.merge(theirs, &overlay, now);
                }
                if *state != before {
                    inner.metrics.merges_applied.fetch_add(1, Ordering::Relaxed);
                }
                inner
                    .metrics
                    .pushes_received
                    .fetch_add(1, Ordering::Relaxed);

                // A peer may be carrying a record about US — a replayed leave,
                // or a stale Alive from an earlier boot. Refuting it here, at
                // the merge, is what keeps a live node from being buried.
                let own = MemberRecord::alive(
                    inner.advertise_addr.clone(),
                    inner.incarnation.load(Ordering::Relaxed),
                );
                let bumped = state.refute(&inner.node_id, &own);
                if let Some(bumped) = bumped {
                    // Adopted while the document lock is still held, so the push
                    // loop can never read the old incarnation next to the
                    // already-bumped record and publish a frame that argues
                    // against itself.
                    inner.incarnation.store(bumped, Ordering::Relaxed);
                }
                bumped
            }
            ClusterMessage::Leave => {
                apply_leave(&mut state, envelope, peer_addr);
                None
            }
        };
        // Released before the receipt is recorded, so the overlay lock is held
        // on its own for the write — and, where the merge above needed both,
        // only ever in the document-then-overlay order.
        drop(state);
        inner.lock_overlay().record_receipt(&envelope.sender, now);
        outcome
    };

    if let Some(bumped) = refuted {
        tracing::info!(
            node_id = %inner.node_id,
            incarnation = bumped,
            "cluster: refuting a stale record about this node at a higher incarnation"
        );
        // Push immediately: the refutation is worthless until the peer that
        // holds the stale record hears it.
        inner.notify.notify_one();
    }
}

/// Apply a `leave`, which carries no fields: it is scoped entirely by the
/// authenticated `(sender, incarnation)` in its envelope, so a captured leave
/// can never be replayed against a newer incarnation of that node.
fn apply_leave(state: &mut ClusterState, envelope: &Envelope, peer_addr: &str) {
    // The document's address is authoritative; the source address is only a
    // fallback for a departure from a node we never met — a `leave` that
    // arrives before (or instead of) that node's final state push.
    //
    // That fallback address is the accepted connection's remote endpoint, which
    // for a peer that dialled us is its ephemeral source port, not its
    // listener. Tombstones DO stay push targets until they are pruned (see
    // `target_addresses`), so this node will dial that dead port for one
    // tombstone window and gossip it onward. Both costs are bounded and both
    // are preferred to the alternative: dropping the address would publish a
    // tombstone nobody can ever push to, and the departed node's own record —
    // carrying its real advertised address — wins the merge the moment any
    // state push about it arrives.
    let addr = state
        .members
        .get(&envelope.sender)
        .map_or_else(|| peer_addr.to_owned(), |record| record.addr.clone());
    let tombstone = MemberRecord::left(addr, envelope.incarnation);
    match state.members.get_mut(&envelope.sender) {
        Some(existing) => existing.merge(&tombstone),
        None => {
            state.members.insert(envelope.sender.clone(), tombstone);
        }
    }
}

/// The node's stable identity: the configured override when it is non-empty,
/// otherwise an entropy-derived id.
///
/// Never hostname-derived: hostnames collide across containers and are not a
/// uniqueness guarantee.
pub fn resolve_node_id(configured: Option<&str>, entropy: &dyn Entropy) -> NodeId {
    configured
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map_or_else(
            || format!("node-{}", entropy.uuid_v4().simple()),
            ToOwned::to_owned,
        )
}

#[cfg(test)]
mod tests {
    use super::{PUSH_FLOOR_MIN, push_floor, seed_incarnation, target_addresses};
    use crate::cluster::membership::{ClusterState, MemberRecord};
    use crate::time::FixedClock;
    use std::time::Duration;

    fn document(records: &[(&str, MemberRecord)]) -> ClusterState {
        let mut state = ClusterState::default();
        for (id, record) in records {
            state.members.insert((*id).to_owned(), record.clone());
        }
        state
    }

    /// A departed peer keeps receiving pushes until its tombstone is pruned —
    /// the only channel by which a node that came back at a LOWER incarnation
    /// (a backward clock step) can ever hear its own `Left` record and refute
    /// past it. Dropping `Left` targets partitions that node permanently.
    #[test]
    fn tombstoned_members_stay_push_targets() {
        let state = document(&[
            ("node-a", MemberRecord::alive("127.0.0.1:7001", 4)),
            ("node-b", MemberRecord::left("127.0.0.1:7002", 9)),
        ]);

        let targets: Vec<String> =
            target_addresses(&state, "node-a", &[], &["127.0.0.1:7001", "127.0.0.1:7001"])
                .into_iter()
                .collect();

        assert_eq!(
            targets,
            vec!["127.0.0.1:7002".to_owned()],
            "a tombstoned member must stay a push target until it is pruned, \
             and this node must never gossip at itself; observed {targets:?}"
        );
    }

    #[test]
    fn own_addresses_and_empty_addresses_are_never_targets() {
        let state = document(&[
            ("node-a", MemberRecord::alive("10.0.0.1:7946", 1)),
            // A member learned from a document that carried no address.
            ("node-c", MemberRecord::alive("", 1)),
        ]);
        let seeds = vec![
            "  127.0.0.1:7100  ".to_owned(),
            String::new(),
            // A seed list naming this node is a configuration to survive.
            "10.0.0.1:7946".to_owned(),
        ];

        let targets: Vec<String> =
            target_addresses(&state, "node-a", &seeds, &["10.0.0.1:7946", "127.0.0.1:9"])
                .into_iter()
                .collect();

        assert_eq!(
            targets,
            vec!["127.0.0.1:7100".to_owned()],
            "seeds must be trimmed, blanks and address-less members skipped, and \
             every address of this node removed; observed {targets:?}"
        );
    }

    /// The runtime config holds the HMAC key as a plain `Vec<u8>` copied out
    /// of a `SecretString`, so `Debug` is hand-written to omit it. Re-deriving
    /// `Debug` would print the whole key the first time somebody added
    /// `?config` to a boot log line, and clippy has no lint for it.
    #[test]
    fn runtime_config_debug_never_prints_the_secret() {
        let config = super::ClusterRuntimeConfig {
            cluster_name: "orchard".to_owned(),
            secret: b"a-shared-cluster-secret-value-32".to_vec(),
            node_id: Some("node-a".to_owned()),
            advertise_addr: Some("10.0.1.7:7946".to_owned()),
            seed_peers: vec!["10.0.1.8:7946".to_owned()],
            push_interval: Duration::from_millis(500),
            suspicion_timeout: Duration::from_millis(2_500),
        };

        let rendered = format!("{config:?}");
        assert!(
            !rendered.contains("secret"),
            "the secret field must not appear in Debug output at all; got {rendered}"
        );
        assert!(
            !rendered.contains("97, 45, 115"),
            "…and certainly not as the byte array a derived Debug would print \
             (`a-s` = 97, 45, 115); got {rendered}"
        );
        assert!(
            rendered.contains("orchard") && rendered.contains("10.0.1.7:7946"),
            "the diagnosable fields must still be there, or redaction has cost \
             the struct its usefulness; got {rendered}"
        );
    }

    /// Incarnations are Unix **milliseconds**, and the granularity is
    /// load-bearing rather than cosmetic: at second granularity a crash-restart
    /// inside one second mints a byte-identical self-record, which
    /// `ClusterState::refute` is required to treat as this boot's own echo —
    /// so the dead boot's record is never refuted, the peer's replay watermark
    /// never resets, and both boots share one counter cell.
    ///
    /// A regression to `as_secs()` (or to the existing `clock_unix_secs`
    /// helper) is a one-line change that nothing else in the suite can see.
    #[test]
    fn incarnation_is_seeded_from_unix_milliseconds() {
        let clock = FixedClock::at(
            chrono::DateTime::<chrono::Utc>::from_timestamp(1_765_430_000, 250_000_000)
                .unwrap_or_default(),
        );

        assert_eq!(
            seed_incarnation(&clock),
            1_765_430_000_250,
            "the incarnation must be the clock's Unix MILLISECONDS: seconds \
             granularity (or truncation of the sub-second part) would let two \
             boots inside one second mint byte-identical self-records"
        );

        // …and one millisecond really is a distinguishable boot.
        let one_ms_later = FixedClock::at(
            chrono::DateTime::<chrono::Utc>::from_timestamp(1_765_430_000, 251_000_000)
                .unwrap_or_default(),
        );
        assert!(
            seed_incarnation(&one_ms_later) > seed_incarnation(&clock),
            "a boot one millisecond later must come back strictly higher"
        );

        // The documented clamp: a pre-epoch clock saturates at 0 rather than
        // wrapping. Two boots on such a clock DO collide — the caveat named in
        // `seed_incarnation`'s docs and in the guide, not a property to rely on.
        let pre_epoch = FixedClock::at(
            chrono::DateTime::<chrono::Utc>::from_timestamp(-10, 0).unwrap_or_default(),
        );
        assert_eq!(
            seed_incarnation(&pre_epoch),
            0,
            "a pre-epoch clock must clamp to 0, never wrap to a huge incarnation \
             no later boot could beat"
        );
    }

    #[test]
    fn push_floor_is_bounded_at_both_ends() {
        assert_eq!(
            push_floor(Duration::from_millis(500)),
            Duration::from_millis(125),
            "the floor is a quarter of a comfortable push interval"
        );
        assert_eq!(
            push_floor(Duration::from_millis(80)),
            PUSH_FLOOR_MIN,
            "a quarter of a short interval is floored, so a write-heavy handler \
             cannot gossip at request rate"
        );
        assert_eq!(
            push_floor(Duration::from_millis(10)),
            Duration::from_millis(10),
            "…but never above the interval itself: a prompt push must not be \
             rarer than the periodic one"
        );
    }
}
