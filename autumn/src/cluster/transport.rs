//! Peer transport: the swappable bottom of the stack.
//!
//! [`PeerTransport`] is deliberately tiny — send a frame to an address, receive
//! `(from, frame)` pairs, report the bound address — so the choice of TCP is a
//! *detail* rather than a commitment. Two implementations ship:
//!
//! - [`TcpPeerTransport`]: one listener, length-prefixed frames, per-peer
//!   bounded writer queues that **drop on full** (anti-entropy self-heals), and
//!   a capped, entropy-jittered reconnect backoff.
//! - `LoopbackTransport` (test-only): an in-process router keyed by address, so
//!   two whole nodes run deterministically in one process with no sockets.
//!
//! # TCP connection state carries zero liveness meaning
//!
//! A live socket does not mean a live member and a dropped socket does not mean
//! a dead one. Liveness is application-level push receipt only
//! ([`super::membership::LivenessOverlay`]), and a per-connection error is
//! always `continue`, never fatal to the accept loop.
//!
//! # Shape of the TCP implementation
//!
//! Reads and writes are deliberately **not** multiplexed over one socket: the
//! accept loop's connections are read-only and each peer writer owns a
//! write-only connection it dialled itself. That keeps both halves trivially
//! total — a reader that hits EOF just returns and a writer that loses its
//! socket re-dials — and it costs nothing, because every frame is
//! self-describing and authenticated by its envelope rather than by the
//! connection it arrived on.

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

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::wire::{LENGTH_PREFIX_BYTES, RejectReason, frame_len, framed};
use crate::AutumnResult;
use crate::entropy::Entropy;

/// A peer's dial address, as a string so the transport can stay address-family
/// agnostic and a loopback router can key on it directly.
pub type PeerAddr = String;

/// The receive side handed to a node's receive loop.
pub type IncomingFrames = mpsc::Receiver<(PeerAddr, Vec<u8>)>;

/// Per-peer send-queue depth. Full means drop: the next state push carries the
/// same (merged) document anyway.
pub const PEER_QUEUE_CAPACITY: usize = 64;

/// First delay before re-dialling a peer that refused a connection.
pub const RECONNECT_BACKOFF_MIN: Duration = Duration::from_millis(50);

/// Cap on the reconnect delay. Bounded so a peer that comes back is re-dialled
/// promptly, jittered (see [`super::jittered`]) so two nodes that lost each
/// other do not resynchronize into a dial storm.
pub const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(2);

/// Pause after an `accept()` error before listening again, so a transient
/// resource exhaustion (EMFILE) cannot spin the accept loop hot.
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(10);

/// Cap on inbound connections held open at once.
///
/// Anyone who can reach the port can open a socket; nobody can *say* anything
/// without the secret. The cap is what keeps the first fact from costing file
/// descriptors without bound: past it, a new connection is accepted and closed
/// immediately rather than parked. Sized far above any real two-node
/// deployment (one peer needs one), so it can only ever bite an abuser.
pub const MAX_INBOUND_CONNECTIONS: usize = 128;

/// Default deadline for an inbound connection to deliver a *complete frame*.
///
/// A connection that has said nothing for this long is closed, so a socket
/// opened and then left silent costs one descriptor for a bounded time rather
/// than forever. [`TcpPeerTransport::with_inbound_idle_timeout`] raises it for
/// a cluster whose push interval is slower than this.
pub const DEFAULT_INBOUND_IDLE_TIMEOUT: Duration = Duration::from_secs(10);

/// How long per-peer writers outlive the app's shutdown token.
///
/// The departure `Leave` is queued *by* the cancellation arm of the node's push
/// loop, so writers cancelled by the same token would routinely be gone before
/// the frame they exist to carry was written — the clean-leave path would then
/// silently degrade into the suspicion timeout. Writers instead run on a token
/// this transport owns and retires one [`LEAVE_BUDGET`](super::node::LEAVE_BUDGET)
/// after shutdown begins, which is the same budget the departure flush is
/// bounded by.
const WRITER_DRAIN_GRACE: Duration = super::node::LEAVE_BUDGET;

/// How a node talks to its peers.
pub trait PeerTransport: Send + Sync + 'static {
    /// Queue `frame` for `to`. Never blocks and never fails loudly — a full
    /// queue drops, because anti-entropy re-sends the whole document anyway.
    fn send(&self, to: &str, frame: Vec<u8>);

    /// Take the inbound frame stream. Returns `Some` exactly once.
    fn take_incoming(&self) -> Option<IncomingFrames>;

    /// The address this transport is actually bound to.
    fn local_addr(&self) -> SocketAddr;

    /// Start whatever background I/O this transport needs, tying every task it
    /// spawns to a child of `shutdown`.
    ///
    /// Called once by [`ClusterNode::start`](super::node::ClusterNode::start).
    /// The default is a no-op: an in-process transport (the test loopback) has
    /// no sockets to accept on and no writers to spawn.
    fn start(&self, shutdown: &CancellationToken, entropy: &Arc<dyn Entropy>) {
        let _ = (shutdown, entropy);
    }

    /// Frames accepted by [`send`](Self::send) but not yet handed to the OS.
    ///
    /// Read by the bounded departure flush: a clean `Leave` waits for this to
    /// reach zero, but never for longer than
    /// [`LEAVE_BUDGET`](super::node::LEAVE_BUDGET). Zero for a transport that
    /// delivers synchronously.
    fn pending_frames(&self) -> usize {
        0
    }

    /// Frames dropped because a peer's queue was full, its writer had exited,
    /// or the transport was never started. Monotonic; never an error path.
    fn dropped_frames(&self) -> u64 {
        0
    }

    /// Inbound frames refused by the *framing* layer — a length prefix of zero
    /// or over the cap — which never reach
    /// [`FrameVerifier`](super::wire::FrameVerifier) because the connection is
    /// closed on the spot.
    ///
    /// Mirrored into `autumn_cluster_frames_rejected_total{reason="oversize"}`,
    /// so the series counts the real TCP rejections it documents and not only
    /// the ones a whole-buffer transport can hand to the verifier. Monotonic.
    fn framing_rejections(&self) -> u64 {
        0
    }

    /// Retire the per-peer state of every address **not** in `live`.
    ///
    /// Called with each push round's target set. A node id that returns at a
    /// new address leaves its old address behind in the document only until the
    /// record merges; without this the writer task and queue for the dead
    /// address would live as long as the process, so address churn would
    /// accumulate tasks. A no-op for a transport that keeps no per-peer state.
    fn retain_peers(&self, live: &BTreeSet<String>) {
        let _ = live;
    }
}

/// The background I/O context, captured when [`PeerTransport::start`] runs.
///
/// Held rather than re-derived because [`PeerTransport::send`] is synchronous
/// and may be called from outside a runtime thread: spawning through a stored
/// [`tokio::runtime::Handle`] cannot panic, whereas `tokio::spawn` would.
struct TransportIo {
    runtime: tokio::runtime::Handle,
    /// The token every **writer** runs on. Owned by this transport rather than
    /// derived from the app's shutdown token, and retired one
    /// [`WRITER_DRAIN_GRACE`] after that token fires — see its docs.
    writers: CancellationToken,
    entropy: Arc<dyn Entropy>,
}

/// What one inbound connection is allowed to cost, and where it reports.
#[derive(Clone)]
struct InboundLimits {
    /// Deadline for a connection to deliver one complete frame.
    idle_timeout: Duration,
    /// Inbound connections currently held open, capped at
    /// [`MAX_INBOUND_CONNECTIONS`].
    live: Arc<AtomicUsize>,
    /// Framing-layer rejections, surfaced by
    /// [`PeerTransport::framing_rejections`].
    framing_rejections: Arc<AtomicU64>,
}

/// One accepted connection's slot in the [`MAX_INBOUND_CONNECTIONS`] budget,
/// released on drop so an early return, an error, or a cancellation all give it
/// back on the same path.
struct InboundSlot(Arc<AtomicUsize>);

impl Drop for InboundSlot {
    fn drop(&mut self) {
        // `fetch_update` rather than `fetch_sub`: a counter that underflowed
        // would wrap to `usize::MAX` and permanently close the port.
        let _ = self
            .0
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |live| {
                Some(live.saturating_sub(1))
            });
    }
}

/// The production transport: a single TCP listener plus per-peer writers.
pub struct TcpPeerTransport {
    local_addr: SocketAddr,
    /// The bound listener, taken by the accept loop when it starts.
    listener: Mutex<Option<std::net::TcpListener>>,
    incoming: Mutex<Option<IncomingFrames>>,
    /// Kept alive so the receiver stays open before the accept loop exists.
    inbound_tx: mpsc::Sender<(PeerAddr, Vec<u8>)>,
    /// `dial address -> that peer's bounded writer queue`, created lazily on
    /// the first frame addressed to a peer.
    peers: Mutex<BTreeMap<PeerAddr, mpsc::Sender<Vec<u8>>>>,
    io: Mutex<Option<TransportIo>>,
    dropped: AtomicU64,
    limits: InboundLimits,
}

impl std::fmt::Debug for TcpPeerTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TcpPeerTransport")
            .field("local_addr", &self.local_addr)
            .finish_non_exhaustive()
    }
}

impl TcpPeerTransport {
    /// Bind the cluster listener.
    ///
    /// `127.0.0.1:0` binds an OS-assigned ephemeral port; read it back with
    /// [`PeerTransport::local_addr`] to seed a second node.
    ///
    /// # Errors
    ///
    /// Returns a boot error when the address cannot be bound — a node that
    /// cannot be reached must not pretend to have joined.
    pub fn bind(addr: &str) -> AutumnResult<Self> {
        let listener =
            std::net::TcpListener::bind(addr).map_err(|err| super::bind_error(addr, &err))?;
        let local_addr = listener
            .local_addr()
            .map_err(|err| super::bind_error(addr, &err))?;
        listener
            .set_nonblocking(true)
            .map_err(|err| super::bind_error(addr, &err))?;

        let (inbound_tx, incoming) = mpsc::channel(PEER_QUEUE_CAPACITY);
        Ok(Self {
            local_addr,
            listener: Mutex::new(Some(listener)),
            incoming: Mutex::new(Some(incoming)),
            inbound_tx,
            peers: Mutex::new(BTreeMap::new()),
            io: Mutex::new(None),
            dropped: AtomicU64::new(0),
            limits: InboundLimits {
                idle_timeout: DEFAULT_INBOUND_IDLE_TIMEOUT,
                live: Arc::new(AtomicUsize::new(0)),
                framing_rejections: Arc::new(AtomicU64::new(0)),
            },
        })
    }

    /// Set how long an inbound connection may go without delivering a complete
    /// frame before it is closed.
    ///
    /// The installer derives it from the cluster's own timings, so the deadline
    /// can never fire on a peer that is still pushing: a peer silent for longer
    /// than the suspicion timeout is already out of the view, and re-dialling
    /// costs it one push.
    #[must_use]
    pub const fn with_inbound_idle_timeout(mut self, idle_timeout: Duration) -> Self {
        self.limits.idle_timeout = idle_timeout;
        self
    }

    /// Take the bound listener, for the accept loop.
    pub fn take_listener(&self) -> Option<std::net::TcpListener> {
        self.listener
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }

    /// A sender the accept loop can clone into each per-connection reader.
    pub fn inbound_sender(&self) -> mpsc::Sender<(PeerAddr, Vec<u8>)> {
        self.inbound_tx.clone()
    }

    fn lock_peers(&self) -> std::sync::MutexGuard<'_, BTreeMap<PeerAddr, mpsc::Sender<Vec<u8>>>> {
        self.peers.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn lock_io(&self) -> std::sync::MutexGuard<'_, Option<TransportIo>> {
        self.io.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Everything needed to spawn one writer task, cloned out of the stored I/O
    /// context. `None` before [`PeerTransport::start`] has run.
    fn spawn_context(
        &self,
    ) -> Option<(tokio::runtime::Handle, CancellationToken, Arc<dyn Entropy>)> {
        self.lock_io().as_ref().map(|io| {
            (
                io.runtime.clone(),
                io.writers.child_token(),
                Arc::clone(&io.entropy),
            )
        })
    }

    /// Inbound connections currently counted against
    /// [`MAX_INBOUND_CONNECTIONS`].
    ///
    /// Test observability only: the number an operator sees is the cap being
    /// hit in the log, and the number that matters is that this returns to zero
    /// when connections close — a leaked slot would close the port for good.
    #[cfg(test)]
    fn live_inbound(&self) -> usize {
        self.limits.live.load(Ordering::Relaxed)
    }

    /// How many peers currently have a writer task and queue.
    ///
    /// Test observability only: production reads nothing here, and the number
    /// is exactly what [`PeerTransport::retain_peers`] exists to bound.
    #[cfg(test)]
    fn writer_count(&self) -> usize {
        self.lock_peers().len()
    }

    /// The queue for `to`, spawning that peer's writer task on first use.
    ///
    /// `None` before [`PeerTransport::start`] has run (nothing can be written
    /// yet) — the caller counts that as a dropped frame rather than blocking a
    /// push round on it.
    fn writer_for(&self, to: &str) -> Option<mpsc::Sender<Vec<u8>>> {
        let mut peers = self.lock_peers();
        // A closed queue means that writer task exited (cancelled, or its
        // connection is unrecoverable). Forget it so the next frame re-dials.
        if peers.get(to).is_some_and(mpsc::Sender::is_closed) {
            peers.remove(to);
        }
        if let Some(existing) = peers.get(to) {
            return Some(existing.clone());
        }

        let (runtime, shutdown, entropy) = self.spawn_context()?;
        let (tx, queue) = mpsc::channel(PEER_QUEUE_CAPACITY);
        runtime.spawn(peer_writer(to.to_owned(), queue, shutdown, entropy));
        peers.insert(to.to_owned(), tx.clone());
        drop(peers);
        Some(tx)
    }
}

impl PeerTransport for TcpPeerTransport {
    fn send(&self, to: &str, frame: Vec<u8>) {
        let Some(queue) = self.writer_for(to) else {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        };
        // `try_send`, never `send().await`: a slow or dead peer must not be able
        // to stall the push loop, and the next push carries the same (merged)
        // document anyway. Full and closed are both simply "the packet was
        // lost", which the protocol is built to tolerate.
        if queue.try_send(frame).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            tracing::debug!(
                peer = %to,
                "cluster: peer send queue full or closed, dropping a state push \
                 (anti-entropy re-sends the document)"
            );
        }
    }

    fn take_incoming(&self) -> Option<IncomingFrames> {
        self.incoming
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take()
    }

    fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    fn start(&self, shutdown: &CancellationToken, entropy: &Arc<dyn Entropy>) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            tracing::warn!(
                "cluster: the peer transport was started outside a Tokio runtime; \
                 no cluster I/O will run"
            );
            return;
        };
        let writers = CancellationToken::new();
        {
            let mut io = self.lock_io();
            if io.is_some() {
                // Started already: the accept loop is running and the writers
                // are keyed off the first `io` we stored.
                return;
            }
            *io = Some(TransportIo {
                runtime: runtime.clone(),
                writers: writers.clone(),
                entropy: Arc::clone(entropy),
            });
        }
        runtime.spawn(retire_writers(shutdown.clone(), writers));
        let Some(listener) = self.take_listener() else {
            return;
        };
        runtime.spawn(accept_loop(
            listener,
            self.inbound_sender(),
            shutdown.child_token(),
            self.limits.clone(),
        ));
    }

    fn pending_frames(&self) -> usize {
        self.lock_peers()
            .values()
            .map(|queue| queue.max_capacity().saturating_sub(queue.capacity()))
            .sum()
    }

    fn dropped_frames(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    fn framing_rejections(&self) -> u64 {
        self.limits.framing_rejections.load(Ordering::Relaxed)
    }

    fn retain_peers(&self, live: &BTreeSet<String>) {
        // Dropping the queue's sender is the retirement: the writer's `recv`
        // then yields `None` once it has drained whatever was queued, so a
        // frame already handed over is still transmitted.
        self.lock_peers().retain(|addr, _| live.contains(addr));
    }
}

/// Retire the per-peer writers a bounded grace after shutdown begins.
///
/// Not a loop and not detached: it awaits the app's token, waits out
/// [`WRITER_DRAIN_GRACE`], and ends. That gap is the whole point — see
/// [`WRITER_DRAIN_GRACE`] — and it is what makes the clean-leave path work
/// under cancellation instead of degrading to the suspicion timeout.
async fn retire_writers(shutdown: CancellationToken, writers: CancellationToken) {
    shutdown.cancelled().await;
    tokio::time::sleep(WRITER_DRAIN_GRACE).await;
    writers.cancel();
}

/// Accept inbound connections until cancelled.
///
/// A per-connection error is always `continue`: one peer's bad socket must
/// never take the listener down, because a node that stops accepting is
/// unreachable to every *other* peer too.
async fn accept_loop(
    listener: std::net::TcpListener,
    inbound: mpsc::Sender<(PeerAddr, Vec<u8>)>,
    shutdown: CancellationToken,
    limits: InboundLimits,
) {
    let Ok(listener) = tokio::net::TcpListener::from_std(listener) else {
        tracing::warn!("cluster: could not adopt the bound listener; no peer can connect");
        return;
    };
    loop {
        let accepted = tokio::select! {
            result = listener.accept() => result,
            () = shutdown.cancelled() => return,
        };
        match accepted {
            Ok((stream, peer)) => {
                let Some(slot) = claim_inbound_slot(&limits.live) else {
                    // Accepted and closed at once rather than parked: the
                    // socket has already cost a descriptor, and refusing it
                    // here is what keeps the cost bounded.
                    drop(stream);
                    tracing::warn!(
                        peer = %peer,
                        cap = MAX_INBOUND_CONNECTIONS,
                        "cluster: inbound connection cap reached; closing the new connection"
                    );
                    continue;
                };
                let reader = connection_reader(
                    stream,
                    peer.to_string(),
                    inbound.clone(),
                    shutdown.child_token(),
                    limits.clone(),
                    slot,
                );
                tokio::spawn(reader);
            }
            Err(err) => {
                tracing::debug!(error = %err, "cluster: accept failed; the listener keeps running");
                tokio::select! {
                    () = tokio::time::sleep(ACCEPT_ERROR_BACKOFF) => {}
                    () = shutdown.cancelled() => return,
                }
            }
        }
    }
}

/// Take one inbound connection's slot in the [`MAX_INBOUND_CONNECTIONS`]
/// budget, or `None` when the budget is spent.
fn claim_inbound_slot(live: &Arc<AtomicUsize>) -> Option<InboundSlot> {
    live.fetch_update(Ordering::Relaxed, Ordering::Relaxed, |live| {
        (live < MAX_INBOUND_CONNECTIONS).then(|| live.saturating_add(1))
    })
    .ok()
    .map(|_| InboundSlot(Arc::clone(live)))
}

/// Read length-prefixed frames off one inbound connection and hand them up.
///
/// Framing is the only thing decided here; authentication is the node's
/// business ([`super::wire::FrameVerifier`]), and the source address is
/// forwarded for diagnostics only — it is never an identity.
///
/// Every read is bounded by [`InboundLimits::idle_timeout`]. Nothing on this
/// path knows the secret yet, so an unauthenticated socket that never completes
/// a frame must not be able to hold a descriptor open indefinitely — the
/// deadline is the difference between "somebody can make me hold a socket" and
/// "somebody can make me hold every socket".
async fn connection_reader(
    mut stream: tokio::net::TcpStream,
    peer: PeerAddr,
    inbound: mpsc::Sender<(PeerAddr, Vec<u8>)>,
    shutdown: CancellationToken,
    limits: InboundLimits,
    slot: InboundSlot,
) {
    // Held for the whole connection: dropping it gives the budget back on every
    // exit path below, including cancellation.
    let _slot = slot;
    loop {
        let mut prefix = [0u8; LENGTH_PREFIX_BYTES];
        let read = tokio::select! {
            result = tokio::time::timeout(
                limits.idle_timeout,
                stream.read_exact(&mut prefix),
            ) => result,
            () = shutdown.cancelled() => return,
        };
        match read {
            Ok(Ok(_)) => {}
            // EOF or a reset: the peer re-dials, and a closed connection carries
            // no liveness meaning at all.
            Ok(Err(_)) => return,
            Err(_elapsed) => {
                tracing::debug!(
                    peer = %peer,
                    idle_ms = limits.idle_timeout.as_millis(),
                    "cluster: inbound connection delivered no frame within its idle \
                     deadline; closing it"
                );
                return;
            }
        }

        // Receive-path step 1, and the one place a rejection closes the
        // connection: after a bad length prefix there is no way to know where
        // the next frame starts, so the stream is unusable. The cap is checked
        // on the declared `u32`, before a buffer of that size is reserved.
        let Some(declared) = frame_len(prefix) else {
            // Counted here, not by the verifier: this frame never reaches it,
            // and an `oversize` series that stays at zero for exactly the
            // traffic it documents is worse than no series at all.
            limits.framing_rejections.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                peer = %peer,
                reason = RejectReason::Oversize.label(),
                "cluster: illegal frame length prefix; closing the connection"
            );
            return;
        };

        let mut body = vec![0u8; declared];
        let read = tokio::select! {
            result = tokio::time::timeout(
                limits.idle_timeout,
                stream.read_exact(&mut body),
            ) => result,
            () = shutdown.cancelled() => return,
        };
        // A prefix followed by a stalled body is the same posture as a silent
        // socket: bounded, then closed.
        if !matches!(read, Ok(Ok(_))) {
            return;
        }

        let handed_up = tokio::select! {
            result = inbound.send((peer.clone(), framed(prefix, &body))) => result,
            () = shutdown.cancelled() => return,
        };
        if handed_up.is_err() {
            // The node's receive loop is gone; nothing left to read for.
            return;
        }
    }
}

/// Own one peer's outbound connection: dial on demand, write queued frames,
/// re-dial with a capped, jittered backoff.
///
/// The queue arm is `biased`, so a frame that is already queued always wins
/// over cancellation. That is what flushes the departure `Leave` on shutdown:
/// the task drains what it has and only then sees the cancelled token.
async fn peer_writer(
    to: PeerAddr,
    mut queue: mpsc::Receiver<Vec<u8>>,
    shutdown: CancellationToken,
    entropy: Arc<dyn Entropy>,
) {
    let mut connection: Option<tokio::net::TcpStream> = None;
    let mut backoff = RECONNECT_BACKOFF_MIN;

    loop {
        let queued = tokio::select! {
            biased;
            frame = queue.recv() => frame,
            () = shutdown.cancelled() => None,
        };
        let Some(frame) = queued else { return };

        if connection.is_none() {
            let dialled = tokio::select! {
                result = tokio::net::TcpStream::connect(&to) => result.ok(),
                () = shutdown.cancelled() => return,
            };
            if let Some(stream) = dialled {
                backoff = RECONNECT_BACKOFF_MIN;
                connection = Some(stream);
            } else {
                // Drop this frame and back off. Losing a push is a non-event:
                // the next one carries the whole document again.
                tokio::select! {
                    () = tokio::time::sleep(super::jittered(backoff, entropy.as_ref())) => {}
                    () = shutdown.cancelled() => return,
                }
                backoff = backoff.saturating_mul(2).min(RECONNECT_BACKOFF_MAX);
                continue;
            }
        }

        if let Some(stream) = connection.as_mut()
            && stream.write_all(&frame).await.is_err()
        {
            // Re-dial on the next frame rather than here: the peer may simply
            // be gone, and there is nothing worth retrying this frame for.
            connection = None;
        }
    }
}

#[cfg(test)]
mod tcp_tests {
    use super::{MAX_INBOUND_CONNECTIONS, PeerTransport as _, TcpPeerTransport};
    use crate::entropy::SeededEntropy;
    use std::collections::BTreeSet;
    use std::sync::Arc;
    use std::time::Duration;
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
    use tokio_util::sync::CancellationToken;

    /// Short enough to keep the test quick, long enough that a local connect
    /// and write can never lose the race to it.
    const IDLE: Duration = Duration::from_millis(200);

    /// Poll `condition` until it holds or a generous ceiling elapses, then
    /// return either way — the assertion that follows produces the real
    /// message. A ceiling, not a wait: the loop exits as soon as the
    /// background accept/close work lands.
    async fn poll_until(mut condition: impl FnMut() -> bool) {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
        while !condition() && tokio::time::Instant::now() < deadline {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }

    fn started(idle: Duration) -> (Arc<TcpPeerTransport>, CancellationToken) {
        let transport = Arc::new(
            TcpPeerTransport::bind("127.0.0.1:0")
                .expect("binding an ephemeral loopback port must succeed")
                .with_inbound_idle_timeout(idle),
        );
        let token = CancellationToken::new();
        let entropy: Arc<dyn crate::entropy::Entropy> = Arc::new(SeededEntropy::new(7));
        transport.start(&token, &entropy);
        (transport, token)
    }

    /// An inbound connection that never says anything must not be able to hold
    /// a descriptor open: nothing on the read path knows the secret yet, so
    /// "connected" has to cost strictly less than "authenticated".
    #[tokio::test(flavor = "multi_thread")]
    async fn silent_inbound_connection_is_closed_at_the_idle_deadline() {
        let (transport, token) = started(IDLE);
        let mut client = tokio::net::TcpStream::connect(transport.local_addr())
            .await
            .expect("the cluster listener must accept a connection");

        // Say nothing at all, then read: the server closing is EOF here.
        let mut sink = [0_u8; 1];
        let closed = tokio::time::timeout(Duration::from_secs(5), client.read(&mut sink)).await;

        assert!(
            matches!(closed, Ok(Ok(0))),
            "a connection that delivers no frame within its idle deadline must be \
             closed by the node, not parked forever; observed {closed:?}"
        );
        token.cancel();
    }

    /// The connection budget must be given back when a connection ends.
    ///
    /// A leaked slot is the failure mode that matters here: it would not look
    /// like a bug at all until a long-lived node had accepted
    /// [`MAX_INBOUND_CONNECTIONS`] connections over its lifetime and then
    /// stopped accepting any, peer included.
    #[tokio::test(flavor = "multi_thread")]
    async fn inbound_connection_slots_are_released_when_connections_close() {
        let (transport, token) = started(Duration::from_secs(30));

        let mut clients = Vec::new();
        for _ in 0_u8..4 {
            clients.push(
                tokio::net::TcpStream::connect(transport.local_addr())
                    .await
                    .expect("the cluster listener must accept a connection"),
            );
        }
        poll_until(|| transport.live_inbound() == 4).await;
        assert_eq!(
            transport.live_inbound(),
            4,
            "every accepted connection must take one slot in the budget"
        );

        drop(clients);
        poll_until(|| transport.live_inbound() == 0).await;
        assert_eq!(
            transport.live_inbound(),
            0,
            "a closed connection must give its slot back, or the node stops \
             accepting anything once it has seen {MAX_INBOUND_CONNECTIONS} \
             connections in its life"
        );
        token.cancel();
    }

    /// The connection-fatal framing rejection must be *counted*, or
    /// `frames_rejected_total{reason="oversize"}` reads zero for exactly the
    /// traffic it documents.
    #[tokio::test(flavor = "multi_thread")]
    async fn oversize_length_prefix_is_counted_and_closes_the_connection() {
        let (transport, token) = started(Duration::from_secs(30));
        let mut client = tokio::net::TcpStream::connect(transport.local_addr())
            .await
            .expect("the cluster listener must accept a connection");

        assert_eq!(
            transport.framing_rejections(),
            0,
            "sanity: nothing has been rejected yet"
        );
        client
            .write_all(&u32::MAX.to_be_bytes())
            .await
            .expect("writing a hostile length prefix must reach the node");

        let mut sink = [0_u8; 1];
        let closed = tokio::time::timeout(Duration::from_secs(5), client.read(&mut sink)).await;
        assert!(
            matches!(closed, Ok(Ok(0))),
            "a 4 GiB length prefix desynchronizes the framing, so the connection \
             must close; observed {closed:?}"
        );
        assert_eq!(
            transport.framing_rejections(),
            1,
            "the oversize rejection must be counted even though the frame never \
             reaches the verifier"
        );
        token.cancel();
    }

    /// A node id that comes back at a new address must not leave the old
    /// address's writer task and queue alive for the life of the process.
    #[tokio::test(flavor = "multi_thread")]
    async fn writers_retire_when_an_address_leaves_the_target_set() {
        let (transport, token) = started(IDLE);

        // Two addresses nobody is listening on: a writer is created by the
        // send, and whether the dial succeeds is beside the point.
        transport.send("127.0.0.1:9", vec![1, 2, 3]);
        transport.send("127.0.0.1:10", vec![4, 5, 6]);
        assert_eq!(
            transport.writer_count(),
            2,
            "each addressed peer must get its own writer queue"
        );

        // The membership view now knows only one of them.
        let live: BTreeSet<String> = std::iter::once("127.0.0.1:10".to_owned()).collect();
        transport.retain_peers(&live);

        assert_eq!(
            transport.writer_count(),
            1,
            "an address that has left the target set must not keep a writer \
             queue alive — repeated address churn would otherwise accumulate \
             one task per address, forever"
        );
        token.cancel();
    }
}

// ── In-process loopback transport (tests only) ───────────────────────────────

#[cfg(test)]
// `LoopbackTransport` itself is never named outside its module — a test builds
// one through `LoopbackRouter::endpoint` and coerces it straight to
// `Arc<dyn PeerTransport>` — so only the router is re-exported.
pub use loopback::LoopbackRouter;

#[cfg(test)]
mod loopback {
    use super::{IncomingFrames, PEER_QUEUE_CAPACITY, PeerAddr, PeerTransport};
    use std::collections::BTreeMap;
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex, PoisonError};
    use tokio::sync::mpsc;

    /// First port handed out by [`LoopbackRouter`]; the exact numbers are
    /// irrelevant, only that each endpoint has a distinct dial address (which
    /// is what makes the loopback model real seed-dial addressing).
    const FIRST_LOOPBACK_PORT: u16 = 47_000;

    #[derive(Default)]
    struct RouterInner {
        issued: u16,
        peers: BTreeMap<PeerAddr, mpsc::Sender<(PeerAddr, Vec<u8>)>>,
    }

    /// An in-process message router: whole nodes, no sockets, no wall clock.
    #[derive(Clone, Default)]
    pub struct LoopbackRouter {
        inner: Arc<Mutex<RouterInner>>,
    }

    impl LoopbackRouter {
        pub fn new() -> Self {
            Self::default()
        }

        fn lock(&self) -> std::sync::MutexGuard<'_, RouterInner> {
            self.inner.lock().unwrap_or_else(PoisonError::into_inner)
        }

        /// Register a new endpoint and hand back its transport.
        pub fn endpoint(&self) -> Arc<LoopbackTransport> {
            let (tx, rx) = mpsc::channel(PEER_QUEUE_CAPACITY);
            let addr = {
                let mut guard = self.lock();
                let port = FIRST_LOOPBACK_PORT.saturating_add(guard.issued);
                guard.issued = guard.issued.saturating_add(1);
                let addr = SocketAddr::from(([127, 0, 0, 1], port));
                guard.peers.insert(addr.to_string(), tx);
                addr
            };
            Arc::new(LoopbackTransport {
                router: self.clone(),
                addr,
                incoming: Mutex::new(Some(rx)),
            })
        }

        /// Deliver `frame` to `to`, attributed to `from`.
        ///
        /// Returns `false` when either endpoint is unknown or the destination's
        /// queue is full — all of them are "the packet was lost", which the
        /// protocol must tolerate. Also the injection point for replay tests.
        ///
        /// The `from` check is what makes [`disconnect`](Self::disconnect) a
        /// *hard* kill: an unplugged endpoint can neither receive nor send, so
        /// it cannot get a clean departure notice out either. Without it, a
        /// `kill -9` scenario would still deliver the victim's `Leave` and the
        /// suspicion timeout — the actual correctness path — would never be
        /// exercised.
        pub fn deliver(&self, from: &str, to: &str, frame: Vec<u8>) -> bool {
            let sender = {
                let guard = self.lock();
                if !guard.peers.contains_key(from) {
                    return false;
                }
                guard.peers.get(to).cloned()
            };
            sender.is_some_and(|tx| tx.try_send((from.to_owned(), frame)).is_ok())
        }

        /// Remove an endpoint: a hard kill with no clean departure.
        pub fn disconnect(&self, addr: &str) {
            self.lock().peers.remove(addr);
        }
    }

    /// One endpoint on a [`LoopbackRouter`].
    pub struct LoopbackTransport {
        router: LoopbackRouter,
        addr: SocketAddr,
        incoming: Mutex<Option<IncomingFrames>>,
    }

    impl std::fmt::Debug for LoopbackTransport {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            f.debug_struct("LoopbackTransport")
                .field("addr", &self.addr)
                .finish_non_exhaustive()
        }
    }

    impl PeerTransport for LoopbackTransport {
        fn send(&self, to: &str, frame: Vec<u8>) {
            // Loss is a legal outcome; anti-entropy re-sends the document.
            let _delivered = self.router.deliver(&self.addr.to_string(), to, frame);
        }

        fn take_incoming(&self) -> Option<IncomingFrames> {
            self.incoming
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .take()
        }

        fn local_addr(&self) -> SocketAddr {
            self.addr
        }
    }
}
