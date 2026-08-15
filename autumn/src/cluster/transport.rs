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

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::wire::{LENGTH_PREFIX_BYTES, RejectReason, frame_len};
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
}

/// The background I/O context, captured when [`PeerTransport::start`] runs.
///
/// Held rather than re-derived because [`PeerTransport::send`] is synchronous
/// and may be called from outside a runtime thread: spawning through a stored
/// [`tokio::runtime::Handle`] cannot panic, whereas `tokio::spawn` would.
struct TransportIo {
    runtime: tokio::runtime::Handle,
    shutdown: CancellationToken,
    entropy: Arc<dyn Entropy>,
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
        })
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
                io.shutdown.child_token(),
                Arc::clone(&io.entropy),
            )
        })
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
        {
            let mut io = self.lock_io();
            if io.is_some() {
                // Started already: the accept loop is running and the writers
                // are keyed off the first `io` we stored.
                return;
            }
            *io = Some(TransportIo {
                runtime: runtime.clone(),
                shutdown: shutdown.clone(),
                entropy: Arc::clone(entropy),
            });
        }
        let Some(listener) = self.take_listener() else {
            return;
        };
        runtime.spawn(accept_loop(
            listener,
            self.inbound_sender(),
            shutdown.child_token(),
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
                let reader = connection_reader(
                    stream,
                    peer.to_string(),
                    inbound.clone(),
                    shutdown.child_token(),
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

/// Read length-prefixed frames off one inbound connection and hand them up.
///
/// Framing is the only thing decided here; authentication is the node's
/// business ([`super::wire::FrameVerifier`]), and the source address is
/// forwarded for diagnostics only — it is never an identity.
async fn connection_reader(
    mut stream: tokio::net::TcpStream,
    peer: PeerAddr,
    inbound: mpsc::Sender<(PeerAddr, Vec<u8>)>,
    shutdown: CancellationToken,
) {
    loop {
        let mut prefix = [0u8; LENGTH_PREFIX_BYTES];
        let read = tokio::select! {
            result = stream.read_exact(&mut prefix) => result,
            () = shutdown.cancelled() => return,
        };
        if read.is_err() {
            // EOF or a reset: the peer re-dials, and a closed connection carries
            // no liveness meaning at all.
            return;
        }

        // Receive-path step 1, and the one place a rejection closes the
        // connection: after a bad length prefix there is no way to know where
        // the next frame starts, so the stream is unusable. The cap is checked
        // on the declared `u32`, before a buffer of that size is reserved.
        let Some(declared) = frame_len(prefix) else {
            tracing::warn!(
                peer = %peer,
                reason = RejectReason::Oversize.label(),
                "cluster: illegal frame length prefix; closing the connection"
            );
            return;
        };

        let mut body = vec![0u8; declared];
        let read = tokio::select! {
            result = stream.read_exact(&mut body) => result,
            () = shutdown.cancelled() => return,
        };
        if read.is_err() {
            return;
        }

        let mut frame = Vec::with_capacity(declared.saturating_add(LENGTH_PREFIX_BYTES));
        frame.extend_from_slice(&prefix);
        frame.extend_from_slice(&body);

        let handed_up = tokio::select! {
            result = inbound.send((peer.clone(), frame)) => result,
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
