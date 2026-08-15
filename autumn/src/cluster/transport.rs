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
//! RED PHASE (TDD): [`TcpPeerTransport`] binds its listener (so `local_addr()`
//! is honest and a bind failure is still a hard boot error) but drives no I/O.

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

use std::net::SocketAddr;
use std::sync::{Mutex, PoisonError};

use tokio::sync::mpsc;

use crate::AutumnResult;

/// A peer's dial address, as a string so the transport can stay address-family
/// agnostic and a loopback router can key on it directly.
pub type PeerAddr = String;

/// The receive side handed to a node's receive loop.
pub type IncomingFrames = mpsc::Receiver<(PeerAddr, Vec<u8>)>;

/// Per-peer send-queue depth. Full means drop: the next state push carries the
/// same (merged) document anyway.
pub const PEER_QUEUE_CAPACITY: usize = 64;

/// How a node talks to its peers.
pub trait PeerTransport: Send + Sync + 'static {
    /// Queue `frame` for `to`. Never blocks and never fails loudly — a full
    /// queue drops, because anti-entropy re-sends the whole document anyway.
    fn send(&self, to: &str, frame: Vec<u8>);

    /// Take the inbound frame stream. Returns `Some` exactly once.
    fn take_incoming(&self) -> Option<IncomingFrames>;

    /// The address this transport is actually bound to.
    fn local_addr(&self) -> SocketAddr;
}

/// The production transport: a single TCP listener plus per-peer writers.
pub struct TcpPeerTransport {
    local_addr: SocketAddr,
    /// The bound listener, taken by the accept loop when it starts.
    listener: Mutex<Option<std::net::TcpListener>>,
    incoming: Mutex<Option<IncomingFrames>>,
    /// Kept alive so the receiver stays open before the accept loop exists.
    inbound_tx: mpsc::Sender<(PeerAddr, Vec<u8>)>,
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
}

impl PeerTransport for TcpPeerTransport {
    fn send(&self, to: &str, frame: Vec<u8>) {
        // RED-PHASE STUB: must enqueue onto the per-peer writer, dialing (with
        // capped jittered backoff) when no connection exists yet.
        let _ = (to, frame);
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
}

// ── In-process loopback transport (tests only) ───────────────────────────────

#[cfg(test)]
pub use loopback::{LoopbackRouter, LoopbackTransport};

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
        /// Returns `false` when the destination is unknown or its queue is
        /// full — both are "the packet was lost", which the protocol must
        /// tolerate. Also the injection point for replay tests.
        pub fn deliver(&self, from: &str, to: &str, frame: Vec<u8>) -> bool {
            let sender = self.lock().peers.get(to).cloned();
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
