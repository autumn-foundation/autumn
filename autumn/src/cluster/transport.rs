//! Peer transport: the swappable bottom of the stack.
//!
//! [`PeerTransport`] is deliberately tiny — send a frame to an address, receive
//! `(from, frame)` pairs, report the bound address — so the choice of TCP is a
//! *detail* rather than a commitment. Two implementations ship:
//!
//! - [`TcpPeerTransport`]: one listener, length-prefixed frames, per-peer
//!   bounded writer queues that **drop on full** (anti-entropy self-heals), a
//!   two-frame *departure lane* per peer that the writer reads first (the one
//!   frame anti-entropy never re-sends is the farewell), and a capped,
//!   entropy-jittered reconnect backoff.
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

/// Depth of a peer's **departure lane**, the side channel a farewell travels on
/// and the writer reads in preference to the queue above.
///
/// Exactly one departure: this node's final document and the `Leave` that
/// follows it. Sized to that message rather than to a buffer, because there is
/// no third frame — a node says goodbye once and then exits.
pub const FAREWELL_LANE_CAPACITY: usize = 2;

/// First delay before re-dialling a peer that refused a connection.
pub const RECONNECT_BACKOFF_MIN: Duration = Duration::from_millis(50);

/// Cap on the reconnect delay. Bounded so a peer that comes back is re-dialled
/// promptly, jittered (see [`super::jittered`]) so two nodes that lost each
/// other do not resynchronize into a dial storm.
pub const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(2);

/// Pause after an `accept()` error before listening again, so a transient
/// resource exhaustion (EMFILE) cannot spin the accept loop hot.
const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(10);

/// Cap on one dial attempt, aligned with [`RECONNECT_BACKOFF_MAX`] so a peer
/// that cannot be reached is retried on the schedule the backoff describes
/// rather than on the operating system's SYN timeout.
const DIAL_TIMEOUT: Duration = RECONNECT_BACKOFF_MAX;

/// Cap on writing one frame to a peer. A frame is worth exactly one push
/// interval; anything slower than this is a stalled connection, not a slow one.
const WRITE_TIMEOUT: Duration = Duration::from_secs(2);

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

/// How many frames one inbound connection may have refused *before proving
/// knowledge of the shared secret* before it is closed.
///
/// The idle deadline alone bounds a **silent** socket, not a talkative one: a
/// host that cannot produce a valid MAC could otherwise hold its slot forever
/// by sending one well-framed garbage frame per idle window, and hold every
/// slot in [`MAX_INBOUND_CONNECTIONS`] the same way — at which point real peers
/// are refused at the cap. Counting those refusals is what makes the connection
/// budget contingent on authentication rather than on reachability.
///
/// Three rather than one, and it is a real trade: a misconfigured or
/// mid-rotation peer gets a couple of frames on the wire (so
/// `frames_rejected_total{reason="mac"}` and the rejection log line can name
/// what is wrong) before its socket is taken away, and the counter never
/// decays, so a connection cannot buy itself more budget by behaving in
/// between. Only *unauthenticated* verdicts count
/// ([`RejectReason::authenticated`]) — a peer that holds the secret has earned
/// its descriptor and is never closed by this bound.
pub const MAX_UNAUTHENTICATED_FRAMES: u32 = 3;

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

    /// Queue this node's **farewell** for `to`, superseding every state push
    /// still queued for that peer.
    ///
    /// [`send`](Self::send)'s bargain — full means drop, the next push carries
    /// the same merged document — holds for state pushes and fails for exactly
    /// one frame: there is no next push after the departure. A peer stalled
    /// long enough to fill its queue would otherwise lose the final document
    /// and the `Leave` together, and its survivor would sit out the suspicion
    /// timeout having been told nothing at all. So the departure travels on
    /// capacity of its own, ahead of whatever is queued; a full-document
    /// protocol makes that safe, because the farewell *is* the latest state and
    /// everything behind it is a strictly older copy of the same thing.
    ///
    /// Returns whether the frame was handed over. Unlike a dropped push this
    /// one is never re-sent, so the caller counts and logs a refusal rather
    /// than reporting a departure it did not make.
    ///
    /// The default is [`send`](Self::send) and `true`: a transport that
    /// delivers synchronously has no queue to supersede.
    fn send_farewell(&self, to: &str, frame: Vec<u8>) -> bool {
        self.send(to, frame);
        true
    }

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
    ///
    /// "Not yet handed to the OS" is literal and includes the frame a writer
    /// task is *holding* — mid-dial or mid-write — not only the ones still
    /// queued. Queue depth alone is not that number: a bounded-channel permit
    /// is returned the instant `recv()` yields, so a frame the writer will
    /// spend the next two seconds dialling for would read as flushed.
    fn pending_frames(&self) -> usize {
        0
    }

    /// Frames dropped because a peer's queue (or departure lane) was full, its
    /// writer had exited, or the transport was never started. Monotonic; never
    /// an error path.
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

    /// Report that the frame delivered from `from` was refused **before** it
    /// proved knowledge of the shared secret
    /// ([`RejectReason::authenticated`] is `false`).
    ///
    /// The consumer has to be the one to say so: verification lives in the
    /// node's receive loop, the transport holds no secret, and from down here a
    /// port scanner and a peer are the same stream of bytes. This is the
    /// back-channel that lets a verdict reach the socket that frame arrived on,
    /// so a connection which never authenticates can be closed after
    /// [`MAX_UNAUTHENTICATED_FRAMES`] instead of holding its slot for as long
    /// as it keeps sending.
    ///
    /// A no-op by default: an in-process transport has no descriptor to
    /// reclaim, so there is nothing to spend the report on.
    fn note_unauthenticated_frame(&self, from: &str) {
        let _ = from;
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
    /// The live connections, so a verdict the node reaches about a frame can
    /// find the socket that frame arrived on.
    connections: Arc<InboundConnections>,
}

/// One tracked inbound connection: how much of its
/// [`MAX_UNAUTHENTICATED_FRAMES`] budget it has spent, and the handle that
/// closes it.
struct TrackedConnection {
    /// Which registration owns this entry — see [`InboundRegistration::drop`].
    id: u64,
    /// Frames from this connection the node refused before they authenticated.
    /// Never decays: a connection cannot earn budget back by going quiet.
    unauthenticated: u32,
    /// Cancelling this returns the connection reader from whichever `select!`
    /// arm it is parked in, which drops the stream and its slot.
    close: CancellationToken,
}

/// Every inbound connection currently being read, keyed by remote endpoint.
///
/// The key is the address the inbound stream carries and the node reports back
/// (`ip:port`); the TCP 4-tuple makes it unique among *live* connections, since
/// the local half is this one listener. An entry also remembers the
/// registration that created it, so a connection closing late can never retire
/// the entry of a later one that reused its ephemeral port.
#[derive(Default)]
struct InboundConnections {
    live: Mutex<BTreeMap<PeerAddr, TrackedConnection>>,
    /// Hands out the registration ids above.
    next_id: AtomicU64,
}

impl InboundConnections {
    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<PeerAddr, TrackedConnection>> {
        self.live.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Track one accepted connection until the returned guard is dropped.
    fn register(self: &Arc<Self>, peer: &str, close: CancellationToken) -> InboundRegistration {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        self.lock().insert(
            peer.to_owned(),
            TrackedConnection {
                id,
                unauthenticated: 0,
                close,
            },
        );
        InboundRegistration {
            connections: Arc::clone(self),
            peer: peer.to_owned(),
            id,
        }
    }

    /// Charge one unauthenticated frame to `peer`, closing that connection once
    /// it has spent its budget.
    ///
    /// An address with no live entry is ignored: the connection already ended,
    /// and the node's verdict about its last frame has nothing left to close.
    fn note_unauthenticated(&self, peer: &str) {
        let mut live = self.lock();
        let Some(tracked) = live.get_mut(peer) else {
            return;
        };
        tracked.unauthenticated = tracked.unauthenticated.saturating_add(1);
        if tracked.unauthenticated < MAX_UNAUTHENTICATED_FRAMES {
            return;
        }
        tracked.close.cancel();
        drop(live);
        tracing::warn!(
            peer = %peer,
            budget = MAX_UNAUTHENTICATED_FRAMES,
            "cluster: inbound connection spent its unauthenticated-frame budget \
             without ever proving the shared secret; closing it"
        );
    }
}

/// One connection's entry in [`InboundConnections`], removed on drop so an
/// error, an EOF, and a cancellation all retire it on the same path.
struct InboundRegistration {
    connections: Arc<InboundConnections>,
    peer: PeerAddr,
    id: u64,
}

impl Drop for InboundRegistration {
    fn drop(&mut self) {
        let mut live = self.connections.lock();
        // Only if it is still *ours*: an ephemeral port can be reused the
        // moment this connection closes, and removing by key alone would let a
        // dying connection retire the successor that took its address.
        if live
            .get(&self.peer)
            .is_some_and(|tracked| tracked.id == self.id)
        {
            live.remove(&self.peer);
        }
    }
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

/// The two channels one peer's writer task reads.
///
/// Two rather than one, because the frames on them have opposite contracts: a
/// state push is *worth* dropping when the queue is full (the next one carries
/// the same merged document), and a farewell is the last thing this node will
/// ever say to that peer. One shared queue makes the second inherit the first's
/// bargain, which is how a stalled peer plus a shutdown loses a departure with
/// no trace at all.
#[derive(Clone)]
struct PeerLanes {
    /// Ordinary state pushes, [`PEER_QUEUE_CAPACITY`] deep. Full means drop.
    pushes: mpsc::Sender<Vec<u8>>,
    /// The departure lane: [`FAREWELL_LANE_CAPACITY`] deep and read *first* by
    /// the writer, so a farewell overtakes every push already queued.
    farewell: mpsc::Sender<Vec<u8>>,
}

impl PeerLanes {
    /// Frames sitting in either lane that the writer has not taken yet.
    fn queued(&self) -> usize {
        Self::depth(&self.pushes).saturating_add(Self::depth(&self.farewell))
    }

    /// One lane's occupancy: a bounded channel's permits are its free space.
    fn depth(lane: &mpsc::Sender<Vec<u8>>) -> usize {
        lane.max_capacity().saturating_sub(lane.capacity())
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
    /// `dial address -> that peer's bounded writer lanes`, created lazily on
    /// the first frame addressed to a peer.
    peers: Mutex<BTreeMap<PeerAddr, PeerLanes>>,
    io: Mutex<Option<TransportIo>>,
    dropped: AtomicU64,
    /// Frames a writer task has taken off its queue and not yet disposed of.
    /// Added to queue depth by [`PeerTransport::pending_frames`] — see its
    /// contract.
    in_flight: Arc<AtomicUsize>,
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
            in_flight: Arc::new(AtomicUsize::new(0)),
            limits: InboundLimits {
                idle_timeout: DEFAULT_INBOUND_IDLE_TIMEOUT,
                live: Arc::new(AtomicUsize::new(0)),
                framing_rejections: Arc::new(AtomicU64::new(0)),
                connections: Arc::new(InboundConnections::default()),
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

    fn lock_peers(&self) -> std::sync::MutexGuard<'_, BTreeMap<PeerAddr, PeerLanes>> {
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

    /// The lanes for `to`, spawning that peer's writer task on first use.
    ///
    /// `None` before [`PeerTransport::start`] has run (nothing can be written
    /// yet) — the caller counts that as a dropped frame rather than blocking a
    /// push round on it.
    fn writer_for(&self, to: &str) -> Option<PeerLanes> {
        let mut peers = self.lock_peers();
        // A closed lane means that writer task exited (cancelled, or its
        // connection is unrecoverable); both lanes close together, since the
        // writer owns both receivers. Forget it so the next frame re-dials.
        if peers.get(to).is_some_and(|lanes| lanes.pushes.is_closed()) {
            peers.remove(to);
        }
        if let Some(existing) = peers.get(to) {
            return Some(existing.clone());
        }

        let (runtime, shutdown, entropy) = self.spawn_context()?;
        let (pushes, queue) = mpsc::channel(PEER_QUEUE_CAPACITY);
        let (farewell, departure) = mpsc::channel(FAREWELL_LANE_CAPACITY);
        runtime.spawn(peer_writer(
            to.to_owned(),
            queue,
            departure,
            shutdown,
            entropy,
            Arc::clone(&self.in_flight),
        ));
        let lanes = PeerLanes { pushes, farewell };
        peers.insert(to.to_owned(), lanes.clone());
        drop(peers);
        Some(lanes)
    }
}

impl PeerTransport for TcpPeerTransport {
    fn send(&self, to: &str, frame: Vec<u8>) {
        let Some(lanes) = self.writer_for(to) else {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        };
        // `try_send`, never `send().await`: a slow or dead peer must not be able
        // to stall the push loop, and the next push carries the same (merged)
        // document anyway. Full and closed are both simply "the packet was
        // lost", which the protocol is built to tolerate.
        if lanes.pushes.try_send(frame).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            tracing::debug!(
                peer = %to,
                "cluster: peer send queue full or closed, dropping a state push \
                 (anti-entropy re-sends the document)"
            );
        }
    }

    fn send_farewell(&self, to: &str, frame: Vec<u8>) -> bool {
        let Some(lanes) = self.writer_for(to) else {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        };
        // The departure lane, never the queue: a peer stalled long enough to
        // fill 64 frames' worth of pushes is precisely the case this exists
        // for. The writer reads this channel first, so the farewell overtakes
        // those pushes instead of queueing behind them — and they are stale by
        // construction, being older copies of the document this frame carries
        // at a lower sequence, so the peer's replay watermark discards whichever
        // of them the writer still gets to.
        if lanes.farewell.try_send(frame).is_ok() {
            return true;
        }
        // Refused: the writer is gone, or a whole departure is already waiting.
        // Counted and reported, never swallowed — nothing re-sends this frame,
        // so a silent drop here is a departure that never happened.
        self.dropped.fetch_add(1, Ordering::Relaxed);
        false
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
        // Both lanes: a farewell waiting in the departure lane is the one frame
        // the flush that reads this number exists to wait for.
        let queued: usize = self.lock_peers().values().map(PeerLanes::queued).sum();
        // Plus whatever the writer tasks are holding: a tokio permit comes back
        // the moment `recv()` yields, so queue depth alone reports a frame as
        // flushed while its writer is still inside a two-second dial.
        queued.saturating_add(self.in_flight.load(Ordering::Relaxed))
    }

    fn dropped_frames(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    fn framing_rejections(&self) -> u64 {
        self.limits.framing_rejections.load(Ordering::Relaxed)
    }

    fn note_unauthenticated_frame(&self, from: &str) {
        self.limits.connections.note_unauthenticated(from);
    }

    fn retain_peers(&self, live: &BTreeSet<String>) {
        // Dropping a peer's lane senders is the retirement: the writer's queue
        // `recv` then yields `None` once it has drained whatever was queued, so
        // a frame already handed over is still transmitted.
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
                let peer = peer.to_string();
                // A child of the accept loop's token, so this connection can be
                // closed on its own — by the unauthenticated-frame budget —
                // without touching the listener or any other connection, while
                // shutdown still closes all of them at once.
                let close = shutdown.child_token();
                let registration = limits.connections.register(&peer, close.clone());
                let reader = connection_reader(
                    stream,
                    peer,
                    inbound.clone(),
                    close,
                    limits.clone(),
                    slot,
                    registration,
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
///
/// The deadline bounds a *silent* connection; a talkative one is bounded by the
/// node reporting back. `shutdown` here is this connection's own token, so
/// [`InboundConnections::note_unauthenticated`] closing it after
/// [`MAX_UNAUTHENTICATED_FRAMES`] wakes whichever `select!` arm this task is
/// parked in — the read, the body read, or the hand-up — and every one of them
/// returns, dropping the stream, the slot and the registration together.
async fn connection_reader(
    mut stream: tokio::net::TcpStream,
    peer: PeerAddr,
    inbound: mpsc::Sender<(PeerAddr, Vec<u8>)>,
    shutdown: CancellationToken,
    limits: InboundLimits,
    slot: InboundSlot,
    registration: InboundRegistration,
) {
    // Held for the whole connection: dropping them gives the budget back and
    // retires the tracking entry on every exit path below, cancellation
    // included.
    let _slot = slot;
    let _registration = registration;
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

/// One frame a writer task has taken off its queue and not yet disposed of.
///
/// Counted separately from queue depth because a bounded-channel permit is
/// released the instant `recv()` yields — see [`PeerTransport::pending_frames`].
/// The decrement is a `Drop`, so *every* disposal path gives the count back on
/// the same line: written, dropped after a failed dial or write, or abandoned
/// when the writer token fires mid-I/O.
struct InFlightFrame(Arc<AtomicUsize>);

impl InFlightFrame {
    /// Count one frame as in flight until the returned guard is dropped.
    fn claim(counter: &Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::Relaxed);
        Self(Arc::clone(counter))
    }
}

impl Drop for InFlightFrame {
    fn drop(&mut self) {
        // `fetch_update` rather than `fetch_sub`: an underflow would wrap to
        // `usize::MAX` and make every departure flush wait out its whole budget.
        let _ = self
            .0
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |held| {
                Some(held.saturating_sub(1))
            });
    }
}

/// Own one peer's outbound connection: dial on demand, write queued frames,
/// re-dial with a capped, jittered backoff.
///
/// What makes the departure work is the token: `shutdown` here is the
/// transport's **writer** token, retired one [`WRITER_DRAIN_GRACE`] after the
/// app begins shutting down rather than with it — so this task is still alive,
/// and still dialling and writing, when the node's cancellation arm queues its
/// farewell.
///
/// The `biased` order is the whole mechanism, and it reads top to bottom: the
/// **departure lane first**, then the queue, then the token.
///
/// - Departure before queue, because a farewell supersedes every push already
///   queued for this peer. They carry older copies of the same document at
///   lower sequence numbers, so nothing is lost by overtaking them, while
///   waiting behind a stalled peer's 64 queued frames loses the departure
///   itself — the one frame anti-entropy never re-sends.
/// - Queue before token, so a frame *already queued* is preferred to the token
///   while both are ready. That is a preference, not a drain guarantee, and the
///   distinction matters: the dial and write arms below race the same token
///   unbiased, so a frame picked up at or after the end of the grace can still
///   be abandoned rather than written. The grace is sized to make the healthy
///   case comfortable, and the actual contract remains the suspicion timeout —
///   a lost farewell costs latency, never correctness.
///
/// The departure arm matches `Some(frame)` rather than binding the whole
/// `Option`: a lane whose sender was dropped ([`PeerTransport::retain_peers`]
/// retiring this peer) yields `None` at once, and disabling that branch is what
/// lets the queue still drain the frames it was handed before retirement.
async fn peer_writer(
    to: PeerAddr,
    mut queue: mpsc::Receiver<Vec<u8>>,
    mut departure: mpsc::Receiver<Vec<u8>>,
    shutdown: CancellationToken,
    entropy: Arc<dyn Entropy>,
    in_flight: Arc<AtomicUsize>,
) {
    let mut connection: Option<tokio::net::TcpStream> = None;
    let mut backoff = RECONNECT_BACKOFF_MIN;

    loop {
        let queued = tokio::select! {
            biased;
            Some(frame) = departure.recv() => Some(frame),
            frame = queue.recv() => frame,
            () = shutdown.cancelled() => None,
        };
        let Some(frame) = queued else { return };
        // Held for the rest of this iteration, so the frame stays visible to
        // `pending_frames` for exactly as long as this task owes the OS a write.
        let _held = InFlightFrame::claim(&in_flight);

        if connection.is_none() {
            // Bounded: a *refused* connection comes back at once, but a
            // blackholed one — packets dropped, no RST, the ordinary shape of a
            // firewall or a machine that vanished — pends for the OS SYN
            // timeout, which is minutes. The backoff below cannot begin until
            // this returns, so an unbounded dial makes the configured cap a
            // fiction and stalls every frame behind it.
            let dialled = tokio::select! {
                result = tokio::time::timeout(
                    DIAL_TIMEOUT,
                    tokio::net::TcpStream::connect(&to),
                ) => result.ok().and_then(Result::ok),
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

        if let Some(stream) = connection.as_mut() {
            // Bounded and cancellable for the same reason the dial is: a peer
            // that completed the handshake and then stopped reading fills the
            // socket buffer, and an unbounded `write_all` would pend there
            // until the OS TCP timeout — one stalled peer holding the only
            // writer this node has for it, and holding shutdown too.
            let written = tokio::select! {
                result = tokio::time::timeout(WRITE_TIMEOUT, stream.write_all(&frame)) => result,
                () = shutdown.cancelled() => return,
            };
            if !matches!(written, Ok(Ok(()))) {
                // Dropped, not retried: a timed-out write may have left half a
                // frame on the wire, so the stream's framing can no longer be
                // trusted and the connection goes with it. Re-dialled on the
                // next frame — the peer may simply be gone, and the next push
                // carries the whole document again.
                connection = None;
            }
        }
    }
}

#[cfg(test)]
mod tcp_tests {
    use super::{
        DIAL_TIMEOUT, FAREWELL_LANE_CAPACITY, MAX_INBOUND_CONNECTIONS, MAX_UNAUTHENTICATED_FRAMES,
        PEER_QUEUE_CAPACITY, PeerTransport as _, TcpPeerTransport,
    };
    use crate::entropy::SeededEntropy;
    use std::collections::BTreeSet;
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU32, Ordering};
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

    /// A frame the *framing* layer accepts and no verifier ever could: a legal
    /// length prefix over a body that is not an envelope at all.
    ///
    /// This is exactly the cheap shape the connection budget exists for — a
    /// few bytes to send, a whole descriptor to hold.
    fn unauthenticated_frame() -> Vec<u8> {
        let body = b"not-an-envelope";
        let mut frame = u32::try_from(body.len())
            .unwrap_or(u32::MAX)
            .to_be_bytes()
            .to_vec();
        frame.extend_from_slice(body);
        frame
    }

    /// Stand in for the node's receive loop: drain the inbound stream and
    /// report every frame as never having authenticated, which is what a MAC
    /// failure does. Returns the number of frames reported so far, so a test
    /// can wait for its own writes to have been judged.
    ///
    /// The transport cannot make this call itself — it holds no secret, and a
    /// port scanner and a peer look identical from down here — so a test that
    /// skipped the consumer would be testing nothing.
    fn spawn_rejecting_consumer(transport: &Arc<TcpPeerTransport>) -> Arc<AtomicU32> {
        let mut incoming = transport
            .take_incoming()
            .expect("the inbound stream must still be available");
        let transport = Arc::clone(transport);
        let reported = Arc::new(AtomicU32::new(0));
        let counter = Arc::clone(&reported);
        tokio::spawn(async move {
            while let Some((from, _frame)) = incoming.recv().await {
                transport.note_unauthenticated_frame(&from);
                counter.fetch_add(1, Ordering::Relaxed);
            }
        });
        reported
    }

    /// A connection that keeps talking without ever authenticating must lose
    /// its slot.
    ///
    /// The idle deadline bounds a *silent* socket only, so without this bound a
    /// stranger holds a descriptor for as long as it likes at the cost of one
    /// well-framed garbage frame per idle window — and holds all
    /// [`MAX_INBOUND_CONNECTIONS`] of them the same way, at which point real
    /// peers are refused at the cap. Reaching the port must not buy more than a
    /// few frames' worth of resources.
    #[tokio::test(flavor = "multi_thread")]
    async fn inbound_connection_is_closed_once_it_spends_its_unauthenticated_budget() {
        // Far longer than this test runs: the close under test must be the
        // budget, never the idle deadline.
        let (transport, token) = started(Duration::from_secs(30));
        let reported = spawn_rejecting_consumer(&transport);
        let mut client = tokio::net::TcpStream::connect(transport.local_addr())
            .await
            .expect("the cluster listener must accept a connection");

        // One frame short of the budget…
        for _ in 1..MAX_UNAUTHENTICATED_FRAMES {
            client
                .write_all(&unauthenticated_frame())
                .await
                .expect("writing a well-framed garbage frame must reach the node");
        }
        let under_budget = MAX_UNAUTHENTICATED_FRAMES.saturating_sub(1);
        poll_until(|| reported.load(Ordering::Relaxed) >= under_budget).await;

        let mut sink = [0_u8; 1];
        let still_open =
            tokio::time::timeout(Duration::from_millis(200), client.read(&mut sink)).await;
        assert!(
            still_open.is_err(),
            "a connection that has not yet spent its budget of \
             {MAX_UNAUTHENTICATED_FRAMES} must stay open — a bound that fires \
             early would cut off a peer mid-secret-rotation before its bad MAC \
             could even be diagnosed; observed {still_open:?}"
        );

        // …and now the frame that spends it.
        client
            .write_all(&unauthenticated_frame())
            .await
            .expect("writing a well-framed garbage frame must reach the node");
        let closed = tokio::time::timeout(Duration::from_secs(5), client.read(&mut sink)).await;
        assert!(
            matches!(closed, Ok(Ok(0))),
            "after {MAX_UNAUTHENTICATED_FRAMES} frames that never proved the \
             shared secret the node must close the connection, or the inbound \
             cap is a budget anyone who can reach the port can hold forever; \
             observed {closed:?}"
        );
        poll_until(|| transport.live_inbound() == 0).await;
        assert_eq!(
            transport.live_inbound(),
            0,
            "closing the connection must give its slot back"
        );

        // And the listener itself is untouched: one abuser's socket is closed,
        // not the port.
        let mut fresh = tokio::net::TcpStream::connect(transport.local_addr())
            .await
            .expect("the listener must still accept connections after closing an abuser");
        fresh
            .write_all(&unauthenticated_frame())
            .await
            .expect("a fresh connection must still be able to deliver a frame");
        let delivered = MAX_UNAUTHENTICATED_FRAMES.saturating_add(1);
        poll_until(|| reported.load(Ordering::Relaxed) >= delivered).await;
        assert_eq!(
            reported.load(Ordering::Relaxed),
            delivered,
            "the listener must keep accepting and delivering frames; a bound \
             that took the whole accept loop down with the connection would be \
             a worse denial of service than the one it fixes"
        );
        token.cancel();
    }

    /// The point of the bound: the connection budget is spent on peers again
    /// once the abusers holding it are closed.
    ///
    /// This is the finding in full — a stranger who fills every slot makes the
    /// node unreachable to the one peer that matters, and the fix is only worth
    /// anything if the cap actually frees up.
    #[tokio::test(flavor = "multi_thread")]
    async fn inbound_cap_becomes_available_again_once_abusers_are_closed() {
        let (transport, token) = started(Duration::from_secs(30));
        let reported = spawn_rejecting_consumer(&transport);

        let mut abusers = Vec::with_capacity(MAX_INBOUND_CONNECTIONS);
        for _ in 0..MAX_INBOUND_CONNECTIONS {
            abusers.push(
                tokio::net::TcpStream::connect(transport.local_addr())
                    .await
                    .expect("the cluster listener must accept a connection"),
            );
        }
        poll_until(|| transport.live_inbound() == MAX_INBOUND_CONNECTIONS).await;
        assert_eq!(
            transport.live_inbound(),
            MAX_INBOUND_CONNECTIONS,
            "sanity: the budget must actually be full, or the refusal below \
             proves nothing"
        );

        // At the cap, even a legitimate peer is turned away — which is why
        // holding the cap indefinitely is a denial of service and not just
        // untidiness.
        let mut refused = tokio::net::TcpStream::connect(transport.local_addr())
            .await
            .expect("a connection at the cap is accepted and then closed");
        let mut sink = [0_u8; 1];
        let at_cap = tokio::time::timeout(Duration::from_secs(5), refused.read(&mut sink)).await;
        assert!(
            matches!(at_cap, Ok(Ok(0))),
            "sanity: at the cap a new connection must be closed immediately; \
             observed {at_cap:?}"
        );

        // Every abuser now spends its budget without ever authenticating.
        for abuser in &mut abusers {
            for _ in 0..MAX_UNAUTHENTICATED_FRAMES {
                abuser
                    .write_all(&unauthenticated_frame())
                    .await
                    .expect("writing a well-framed garbage frame must reach the node");
            }
        }

        poll_until(|| transport.live_inbound() == 0).await;
        assert_eq!(
            transport.live_inbound(),
            0,
            "every connection that spent its budget must be closed, or a host \
             that cannot produce a valid MAC keeps the port to itself"
        );

        // …and the freed budget is really usable: a peer connects and its frame
        // reaches the node.
        let before = reported.load(Ordering::Relaxed);
        let mut peer = tokio::net::TcpStream::connect(transport.local_addr())
            .await
            .expect("the freed budget must accept a new connection");
        peer.write_all(&unauthenticated_frame())
            .await
            .expect("the new connection must be able to deliver a frame");
        poll_until(|| reported.load(Ordering::Relaxed) > before).await;
        assert!(
            reported.load(Ordering::Relaxed) > before,
            "a connection accepted after the abusers were closed must be read \
             from, not merely accepted"
        );
        drop(abusers);
        token.cancel();
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

    /// The departure flush polls `pending_frames()` to zero as its proof that
    /// the farewell reached the wire, so the number has to include the frame a
    /// writer is *holding*: a tokio permit is returned the instant `recv()`
    /// yields, and queue depth alone would report "flushed" while the writer
    /// was still inside a two-second dial or write.
    #[tokio::test(flavor = "multi_thread")]
    async fn pending_frames_counts_the_frame_a_writer_still_holds() {
        // A blocked *write* cannot be arranged portably — Windows loopback
        // auto-grows its kernel buffers far enough to swallow tens of
        // megabytes, completing `write_all` against a peer that never reads —
        // so this parks the writer in its own bounded *dial* instead:
        // 192.0.2.1 (RFC 5737 TEST-NET-1) is reserved and unrouted, so the
        // SYN goes to the void and the dial holds for the full DIAL_TIMEOUT.
        // The frame leaves the queue microseconds after `send`, which means
        // the only thing keeping it visible below is the writer's in-flight
        // claim — exactly the seam the departure flush depends on.
        const BLACKHOLE: &str = "192.0.2.1:9";

        let (transport, token) = started(Duration::from_secs(30));
        assert!(
            DIAL_TIMEOUT >= Duration::from_secs(1),
            "the settle window below assumes a dial parks for at least 1s"
        );

        transport.send(BLACKHOLE, vec![0_u8; 32]);

        // Generous for the process-local queue pickup (microseconds), and
        // well inside DIAL_TIMEOUT, so on a network that really blackholes
        // TEST-NET the writer is provably mid-dial with the frame in hand.
        tokio::time::sleep(Duration::from_millis(500)).await;

        if transport.pending_frames() == 0 {
            // A sandboxed or unusually-routed environment fails the TEST-NET
            // dial fast (ENETUNREACH / immediate RST) instead of letting the
            // SYN time out; the writer already dropped the frame, so there is
            // no dial window to observe the claim in. The arithmetic half of
            // this contract is still pinned unconditionally by
            // `pending_frames_includes_a_claimed_in_flight_frame` below.
            eprintln!("skipping: this network fails TEST-NET dials fast");
            token.cancel();
            return;
        }
        // Observed mid-dial once; it must still be pending shortly after —
        // under a writer that never claims, the count would have collapsed to
        // zero the instant the frame left the queue.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert_eq!(
            transport.pending_frames(),
            1,
            "a frame the writer took off its queue and has not written yet must \
             still count as pending — reporting 0 here tells the departure flush \
             the Leave is on the wire when it is not"
        );

        token.cancel();
    }

    /// The arithmetic half of the in-flight contract, with no network at all:
    /// `pending_frames` must include a claimed frame and release it on drop.
    /// This is what keeps the departure flush honest even in environments
    /// where the dial-parking proof above has to skip.
    #[tokio::test(flavor = "multi_thread")]
    async fn pending_frames_includes_a_claimed_in_flight_frame() {
        let (transport, token) = started(Duration::from_secs(30));
        assert_eq!(
            transport.pending_frames(),
            0,
            "a fresh transport holds nothing"
        );

        let held = super::InFlightFrame::claim(&transport.in_flight);
        assert_eq!(
            transport.pending_frames(),
            1,
            "a claimed in-flight frame must count as pending: queue depth alone \
             reports a frame as flushed while its writer still owes the OS a write"
        );

        drop(held);
        assert_eq!(
            transport.pending_frames(),
            0,
            "dropping the claim must return the count — written, dropped, and \
             abandoned frames all release it on the same guard"
        );
        token.cancel();
    }

    /// A full per-peer queue must not be able to swallow the departure.
    ///
    /// Dropping a state push on a full queue is the design — the next push
    /// carries the same merged document — and the farewell is the single frame
    /// that argument does not cover, because there is no next push. A peer
    /// stalled long enough to fill its 64-deep queue plus a shutdown would
    /// otherwise lose the final document and the `Leave` together, silently:
    /// the survivor waits out the suspicion timeout, and every increment
    /// accepted since the last push round dies with the process.
    ///
    /// Deterministic by construction: a current-thread runtime with no await
    /// between the sends means the writer task cannot run and cannot drain a
    /// single frame, so the queue is provably full when the farewell is offered.
    #[tokio::test]
    async fn a_departure_is_queued_even_when_the_peer_queue_is_full() {
        // Nobody is listening, and nothing here awaits, so no writer ever
        // touches these frames.
        const PEER: &str = "127.0.0.1:9";

        let (transport, token) = started(IDLE);
        for _ in 0..PEER_QUEUE_CAPACITY {
            transport.send(PEER, vec![0_u8; 8]);
        }
        assert_eq!(
            transport.dropped_frames(),
            0,
            "sanity: a queue of {PEER_QUEUE_CAPACITY} must hold \
             {PEER_QUEUE_CAPACITY} frames"
        );
        transport.send(PEER, vec![0_u8; 8]);
        assert_eq!(
            transport.dropped_frames(),
            1,
            "sanity: the queue must now be full and dropping, or the farewell \
             below is not being offered the case it exists for"
        );

        // A whole departure still fits: the final document and the `Leave`.
        for frame in 0..FAREWELL_LANE_CAPACITY {
            assert!(
                transport.send_farewell(PEER, vec![1_u8; 8]),
                "frame {frame} of a departure must be accepted while the peer's \
                 push queue is full — the farewell is the one frame nothing \
                 re-sends"
            );
        }
        assert_eq!(
            transport.dropped_frames(),
            1,
            "…and neither frame may cost a drop"
        );
        assert_eq!(
            transport.pending_frames(),
            PEER_QUEUE_CAPACITY.saturating_add(FAREWELL_LANE_CAPACITY),
            "a farewell waiting in the departure lane must count as pending, or \
             the departure flush returns before it has reached the wire"
        );

        // The lane is bounded like everything else here, and a refusal is
        // REPORTED rather than swallowed: nothing re-sends a farewell, so a
        // silent drop is a departure that never happened.
        assert!(
            !transport.send_farewell(PEER, vec![2_u8; 8]),
            "a departure lane holding a whole departure must refuse a third \
             frame instead of pretending to have taken it"
        );
        assert_eq!(
            transport.dropped_frames(),
            2,
            "…and that refusal must be counted"
        );
        token.cancel();
    }

    /// The departure lane is not just capacity — the writer has to *prefer* it.
    ///
    /// A farewell sitting behind 64 queued state pushes on a stalled peer is a
    /// farewell that never leaves inside the departure budget. This is the
    /// ordering half of the contract, observed on a real socket: pushes queued
    /// first, the departure queued last, the departure on the wire first.
    #[tokio::test]
    async fn the_writer_takes_the_departure_lane_before_anything_queued() {
        const STALE: u8 = 0x11;
        const FAREWELL: u8 = 0x22;
        const FRAME_BYTES: usize = 8;

        // A real peer, so the ordering is read off an actual connection rather
        // than inferred from a counter.
        let peer = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("binding an ephemeral loopback port must succeed");
        let peer_addr = peer
            .local_addr()
            .expect("the peer listener must report its address")
            .to_string();
        let (transport, token) = started(IDLE);

        // Current-thread runtime, no await in between: the writer task cannot
        // run until the accept below parks, so it meets both lanes ready at
        // once — the exact shape a stalled peer plus a shutdown produces.
        for _ in 0..8_u8 {
            transport.send(&peer_addr, vec![STALE; FRAME_BYTES]);
        }
        assert!(
            transport.send_farewell(&peer_addr, vec![FAREWELL; FRAME_BYTES]),
            "sanity: the departure lane must accept the farewell"
        );

        let (mut accepted, _) = tokio::time::timeout(Duration::from_secs(5), peer.accept())
            .await
            .expect("the writer must dial its peer")
            .expect("accepting the writer's connection must succeed");
        let mut first = [0_u8; FRAME_BYTES];
        tokio::time::timeout(Duration::from_secs(5), accepted.read_exact(&mut first))
            .await
            .expect("the writer must write a frame")
            .expect("reading the writer's first frame must succeed");

        assert_eq!(
            first, [FAREWELL; FRAME_BYTES],
            "the first frame on the wire must be the departure, not a queued \
             state push: a farewell that waits its turn behind a stalled peer's \
             queue is one the peer never hears, and the survivor falls back to \
             the suspicion timeout it was supposed to be spared"
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

        fn send_farewell(&self, to: &str, frame: Vec<u8>) -> bool {
            // No per-peer queue to supersede — the router hands the frame
            // straight to the destination — so the only question this answers
            // is whether the endpoint is still plugged in. Reported honestly
            // rather than defaulted to `true`: an unplugged endpoint (a hard
            // kill) must not look like a delivered departure.
            self.router.deliver(&self.addr.to_string(), to, frame)
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
