//! The cluster transport over the `[iroh]` endpoint.
//!
//! A peer address is `<endpoint-id>` or `<endpoint-id>@<ip:port>`. Each frame
//! goes on its own QUIC uni stream. The frames, the HMAC envelope, the
//! drop-on-full queues and the farewell lane are the same as on TCP.
//!
//! One connection to a peer carries frames in both directions. When a peer
//! dials this node, this node also writes to the peer on that connection. A
//! node that can dial out but cannot be dialed (behind NAT) still gets every
//! push back, so the cluster does not become one-way.
//!
//! The frame source is the peer's endpoint id, which the TLS handshake
//! proves. [`PeerTransport::verify_sender`] refuses a frame when the record
//! that the sender gives for itself names a different endpoint id.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use ::iroh::endpoint::{Connection, ReadToEndError, VarInt};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::{
    DEFAULT_INBOUND_IDLE_TIMEOUT, DIAL_TIMEOUT, FAREWELL_LANE_CAPACITY, InFlightFrame,
    IncomingFrames, MAX_INBOUND_CONNECTIONS, MAX_UNAUTHENTICATED_FRAMES, PEER_QUEUE_CAPACITY,
    PeerAddr, PeerLanes, PeerTransport, RECONNECT_BACKOFF_MAX, RECONNECT_BACKOFF_MIN, TransportIo,
    WRITE_TIMEOUT, claim_inbound_slot, retire_writers,
};
use crate::cluster::wire::{LENGTH_PREFIX_BYTES, MAX_FRAME_BYTES};
use crate::config::parse_iroh_peer;
use crate::entropy::Entropy;
use crate::iroh::{CLUSTER_ALPN, IrohNode};

/// The largest uni stream accepted: one frame and its length prefix.
const MAX_STREAM_BYTES: usize = MAX_FRAME_BYTES.saturating_add(LENGTH_PREFIX_BYTES);

/// The QUIC close code when this node closes a cluster connection.
const CLOSED_BY_CLUSTER: u32 = 2;

/// The live connections, keyed by the peer's endpoint id.
///
/// The writers look here before they dial, so a connection that a peer
/// opened also carries this node's frames to that peer.
#[derive(Default)]
struct Connections {
    by_peer: Mutex<HashMap<String, Connection>>,
}

impl Connections {
    fn lock(&self) -> std::sync::MutexGuard<'_, HashMap<String, Connection>> {
        self.by_peer.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A connection to `peer` that is still open.
    fn get(&self, peer: &str) -> Option<Connection> {
        let mut by_peer = self.lock();
        if by_peer
            .get(peer)
            .is_some_and(|conn| conn.close_reason().is_some())
        {
            by_peer.remove(peer);
        }
        by_peer.get(peer).cloned()
    }

    fn insert(&self, peer: &str, conn: Connection) {
        self.lock().insert(peer.to_owned(), conn);
    }

    /// Forget `conn` when it is still the connection for `peer`.
    fn forget(&self, peer: &str, conn: &Connection) {
        let mut by_peer = self.lock();
        if by_peer
            .get(peer)
            .is_some_and(|known| known.stable_id() == conn.stable_id())
        {
            by_peer.remove(peer);
        }
    }
}

/// One peer's unauthenticated-frame count, and the token that closes every
/// connection from that peer.
struct Strikes {
    count: u32,
    close: CancellationToken,
    connections: usize,
}

/// The unauthenticated-frame budget, per peer endpoint id.
#[derive(Default)]
struct Budget {
    by_peer: Mutex<BTreeMap<String, Strikes>>,
}

impl Budget {
    fn lock(&self) -> std::sync::MutexGuard<'_, BTreeMap<String, Strikes>> {
        self.by_peer.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Count one connection from `peer` until the guard drops. Returns the
    /// token that closes it.
    fn register(self: &Arc<Self>, peer: &str) -> (CancellationToken, BudgetGuard) {
        let mut by_peer = self.lock();
        let strikes = by_peer.entry(peer.to_owned()).or_insert_with(|| Strikes {
            count: 0,
            close: CancellationToken::new(),
            connections: 0,
        });
        // A peer that spent its budget starts again on a new connection, as on
        // TCP.
        if strikes.close.is_cancelled() {
            strikes.count = 0;
            strikes.close = CancellationToken::new();
        }
        strikes.connections = strikes.connections.saturating_add(1);
        let close = strikes.close.clone();
        drop(by_peer);
        (
            close,
            BudgetGuard {
                budget: Arc::clone(self),
                peer: peer.to_owned(),
            },
        )
    }

    fn note_unauthenticated(&self, peer: &str) {
        let mut by_peer = self.lock();
        let Some(strikes) = by_peer.get_mut(peer) else {
            return;
        };
        strikes.count = strikes.count.saturating_add(1);
        if strikes.count < MAX_UNAUTHENTICATED_FRAMES {
            return;
        }
        strikes.close.cancel();
        drop(by_peer);
        tracing::warn!(
            peer = %peer,
            budget = MAX_UNAUTHENTICATED_FRAMES,
            "cluster: iroh peer spent its unauthenticated-frame budget without \
             proving the shared secret; closing its connections"
        );
    }
}

/// Removes a peer's budget entry when its last connection ends.
struct BudgetGuard {
    budget: Arc<Budget>,
    peer: String,
}

impl Drop for BudgetGuard {
    fn drop(&mut self) {
        let mut by_peer = self.budget.lock();
        if let Some(strikes) = by_peer.get_mut(&self.peer) {
            strikes.connections = strikes.connections.saturating_sub(1);
            if strikes.connections == 0 {
                by_peer.remove(&self.peer);
            }
        }
    }
}

/// What a connection reader needs, shared by every connection.
#[derive(Clone)]
struct Readers {
    inbound: mpsc::Sender<(PeerAddr, Vec<u8>)>,
    connections: Arc<Connections>,
    budget: Arc<Budget>,
    framing_rejections: Arc<AtomicU64>,
    idle_timeout: Duration,
    shutdown: CancellationToken,
}

/// The cluster transport over the `[iroh]` endpoint.
pub struct IrohPeerTransport {
    node: IrohNode,
    own_id: String,
    advertise: Option<SocketAddr>,
    accepted: Mutex<Option<mpsc::Receiver<Connection>>>,
    incoming: Mutex<Option<IncomingFrames>>,
    inbound_tx: mpsc::Sender<(PeerAddr, Vec<u8>)>,
    peers: Mutex<BTreeMap<PeerAddr, PeerLanes>>,
    io: Mutex<Option<TransportIo>>,
    readers: Mutex<Option<Readers>>,
    connections: Arc<Connections>,
    budget: Arc<Budget>,
    live_inbound: Arc<AtomicUsize>,
    framing_rejections: Arc<AtomicU64>,
    dropped: AtomicU64,
    in_flight: Arc<AtomicUsize>,
    idle_timeout: Duration,
}

impl std::fmt::Debug for IrohPeerTransport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IrohPeerTransport")
            .field("id", &self.own_id)
            .finish_non_exhaustive()
    }
}

impl IrohPeerTransport {
    /// Register the cluster ALPN on `node`.
    ///
    /// When `[iroh] allowed_peers` is set, only those ids may connect.
    /// `advertise` is the direct `ip:port` that peers dial; when `None`, the
    /// node advertises its own direct address.
    pub fn new(node: IrohNode, advertise: Option<SocketAddr>) -> Self {
        let accepted = if node.allowed_peers().is_empty() {
            node.register(CLUSTER_ALPN)
        } else {
            let allow = node.allowed_peers().clone();
            node.register_with_allow_list(CLUSTER_ALPN, allow)
        };
        let (inbound_tx, incoming) = mpsc::channel(PEER_QUEUE_CAPACITY);
        Self {
            own_id: node.id().to_string(),
            node,
            advertise,
            accepted: Mutex::new(Some(accepted)),
            incoming: Mutex::new(Some(incoming)),
            inbound_tx,
            peers: Mutex::new(BTreeMap::new()),
            io: Mutex::new(None),
            readers: Mutex::new(None),
            connections: Arc::new(Connections::default()),
            budget: Arc::new(Budget::default()),
            live_inbound: Arc::new(AtomicUsize::new(0)),
            framing_rejections: Arc::new(AtomicU64::new(0)),
            dropped: AtomicU64::new(0),
            in_flight: Arc::new(AtomicUsize::new(0)),
            idle_timeout: DEFAULT_INBOUND_IDLE_TIMEOUT,
        }
    }

    /// Set how long a connection may go without a frame from the peer
    /// before it is closed.
    #[must_use]
    pub const fn with_inbound_idle_timeout(mut self, idle_timeout: Duration) -> Self {
        self.idle_timeout = idle_timeout;
        self
    }

    fn lock_peers(&self) -> std::sync::MutexGuard<'_, BTreeMap<PeerAddr, PeerLanes>> {
        self.peers.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn lock_io(&self) -> std::sync::MutexGuard<'_, Option<TransportIo>> {
        self.io.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn readers(&self) -> Option<Readers> {
        self.readers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    fn writer_for(&self, to: &str) -> Option<PeerLanes> {
        let mut peers = self.lock_peers();
        if peers.get(to).is_some_and(|lanes| lanes.pushes.is_closed()) {
            peers.remove(to);
        }
        if let Some(existing) = peers.get(to) {
            return Some(existing.clone());
        }
        let (runtime, shutdown, entropy) = self.lock_io().as_ref().map(|io| {
            (
                io.runtime.clone(),
                io.writers.child_token(),
                Arc::clone(&io.entropy),
            )
        })?;
        let readers = self.readers()?;
        let (pushes, queue) = mpsc::channel(PEER_QUEUE_CAPACITY);
        let (farewell, departure) = mpsc::channel(FAREWELL_LANE_CAPACITY);
        runtime.spawn(iroh_writer(
            Writer {
                to: to.to_owned(),
                node: self.node.clone(),
                readers,
                shutdown,
                entropy,
                in_flight: Arc::clone(&self.in_flight),
            },
            queue,
            departure,
        ));
        let lanes = PeerLanes { pushes, farewell };
        peers.insert(to.to_owned(), lanes.clone());
        drop(peers);
        Some(lanes)
    }

    /// The endpoint id of a peer address, or `None` when it is not one.
    fn peer_id(addr: &str) -> Option<&str> {
        parse_iroh_peer(addr.trim()).ok().map(|(id, _)| id)
    }
}

impl PeerTransport for IrohPeerTransport {
    fn send(&self, to: &str, frame: Vec<u8>) {
        let Some(lanes) = self.writer_for(to) else {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return;
        };
        if lanes.pushes.try_send(frame).is_err() {
            self.dropped.fetch_add(1, Ordering::Relaxed);
        }
    }

    fn send_farewell(&self, to: &str, frame: Vec<u8>) -> bool {
        let Some(lanes) = self.writer_for(to) else {
            self.dropped.fetch_add(1, Ordering::Relaxed);
            return false;
        };
        if lanes.farewell.try_send(frame).is_ok() {
            return true;
        }
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
        self.node
            .direct_addr()
            .or_else(|| self.node.endpoint().bound_sockets().first().copied())
            .unwrap_or_else(|| SocketAddr::from(([0, 0, 0, 0], 0)))
    }

    fn advertise_addr(&self, configured: Option<&str>) -> String {
        let direct = self
            .advertise
            .or_else(|| configured.map(str::trim).and_then(|addr| addr.parse().ok()));
        crate::iroh::format_peer_addr(&self.own_id, direct.or_else(|| self.node.direct_addr()))
    }

    fn dial_targets(&self, targets: BTreeSet<String>) -> BTreeSet<String> {
        // One target per endpoint id, never this node. A target with a direct
        // address wins over a bare id.
        let mut by_id: BTreeMap<String, String> = BTreeMap::new();
        for target in targets {
            let Some(id) = Self::peer_id(&target) else {
                tracing::debug!(peer = %target, "cluster: not an iroh peer address; skipping");
                continue;
            };
            if id == self.own_id {
                continue;
            }
            let id = id.to_owned();
            let has_direct = target.contains('@');
            match by_id.get(&id) {
                Some(kept) if kept.contains('@') || !has_direct => {}
                _ => {
                    by_id.insert(id, target.trim().to_owned());
                }
            }
        }
        by_id.into_values().collect()
    }

    fn verify_sender(&self, from: &str, advertised: Option<&str>) -> bool {
        advertised.is_none_or(|addr| Self::peer_id(addr) == Some(from))
    }

    fn start(&self, shutdown: &CancellationToken, entropy: &Arc<dyn Entropy>) {
        let Ok(runtime) = tokio::runtime::Handle::try_current() else {
            tracing::warn!(
                "cluster: the iroh transport was started outside a Tokio runtime; \
                 no cluster I/O will run"
            );
            return;
        };
        let writers = CancellationToken::new();
        {
            let mut io = self.lock_io();
            if io.is_some() {
                return;
            }
            *io = Some(TransportIo {
                runtime: runtime.clone(),
                writers: writers.clone(),
                entropy: Arc::clone(entropy),
            });
        }
        let readers = Readers {
            inbound: self.inbound_tx.clone(),
            connections: Arc::clone(&self.connections),
            budget: Arc::clone(&self.budget),
            framing_rejections: Arc::clone(&self.framing_rejections),
            idle_timeout: self.idle_timeout,
            // The writers' token: a connection that carries the farewell must
            // outlive the app's token as long as the writers do.
            shutdown: writers.clone(),
        };
        *self.readers.lock().unwrap_or_else(PoisonError::into_inner) = Some(readers.clone());
        runtime.spawn(retire_writers(shutdown.clone(), writers));
        let accepted = self
            .accepted
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .take();
        if let Some(accepted) = accepted {
            runtime.spawn(accept_loop(
                accepted,
                readers,
                Arc::clone(&self.live_inbound),
                shutdown.child_token(),
            ));
        }
    }

    fn pending_frames(&self) -> usize {
        let queued: usize = self.lock_peers().values().map(PeerLanes::queued).sum();
        queued.saturating_add(self.in_flight.load(Ordering::Relaxed))
    }

    fn dropped_frames(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    fn framing_rejections(&self) -> u64 {
        self.framing_rejections.load(Ordering::Relaxed)
    }

    fn note_unauthenticated_frame(&self, from: &str) {
        self.budget.note_unauthenticated(from);
    }

    fn retain_peers(&self, live: &BTreeSet<String>) {
        self.lock_peers().retain(|addr, _| live.contains(addr));
    }
}

/// Take connections from the endpoint and read frames off each.
async fn accept_loop(
    mut accepted: mpsc::Receiver<Connection>,
    readers: Readers,
    live: Arc<AtomicUsize>,
    shutdown: CancellationToken,
) {
    loop {
        let conn = tokio::select! {
            conn = accepted.recv() => conn,
            () = shutdown.cancelled() => return,
        };
        let Some(conn) = conn else { return };
        let Some(slot) = claim_inbound_slot(&live) else {
            tracing::warn!(
                cap = MAX_INBOUND_CONNECTIONS,
                "cluster: inbound connection cap reached; closing the new iroh connection"
            );
            conn.close(VarInt::from_u32(CLOSED_BY_CLUSTER), b"busy");
            continue;
        };
        let peer = conn.remote_id().to_string();
        readers.connections.insert(&peer, conn.clone());
        tokio::spawn(read_frames(conn, peer, readers.clone(), Some(slot)));
    }
}

/// Read one frame per uni stream off `conn`, and hand each up as from `peer`.
///
/// Ends when the connection closes, when it is idle for the idle timeout,
/// when a stream is larger than a frame, or when the peer spends its
/// unauthenticated-frame budget.
async fn read_frames(
    conn: Connection,
    peer: String,
    readers: Readers,
    slot: Option<super::InboundSlot>,
) {
    let _slot = slot;
    let (close, _budget) = readers.budget.register(&peer);
    loop {
        let stream = tokio::select! {
            stream = tokio::time::timeout(readers.idle_timeout, conn.accept_uni()) => stream,
            () = close.cancelled() => break,
            () = readers.shutdown.cancelled() => break,
        };
        let mut stream = match stream {
            Ok(Ok(stream)) => stream,
            Ok(Err(_)) => break,
            Err(_elapsed) => {
                tracing::debug!(peer = %peer, "cluster: iroh connection idle; closing it");
                break;
            }
        };
        let read = tokio::select! {
            read = tokio::time::timeout(readers.idle_timeout, stream.read_to_end(MAX_STREAM_BYTES)) => read,
            () = close.cancelled() => break,
            () = readers.shutdown.cancelled() => break,
        };
        let frame = match read {
            Ok(Ok(frame)) => frame,
            Ok(Err(ReadToEndError::TooLong)) => {
                readers.framing_rejections.fetch_add(1, Ordering::Relaxed);
                tracing::warn!(peer = %peer, "cluster: iroh stream larger than a frame; closing");
                break;
            }
            // A reset stream drops one frame. The connection stays.
            Ok(Err(_)) => continue,
            Err(_elapsed) => break,
        };
        let handed_up = tokio::select! {
            sent = readers.inbound.send((peer.clone(), frame)) => sent,
            () = close.cancelled() => break,
            () = readers.shutdown.cancelled() => break,
        };
        if handed_up.is_err() {
            break;
        }
    }
    readers.connections.forget(&peer, &conn);
    conn.close(VarInt::from_u32(CLOSED_BY_CLUSTER), b"closed");
}

/// What one peer's writer task owns.
struct Writer {
    to: PeerAddr,
    node: IrohNode,
    readers: Readers,
    shutdown: CancellationToken,
    entropy: Arc<dyn Entropy>,
    in_flight: Arc<AtomicUsize>,
}

impl Writer {
    /// A connection to the peer: one that is open (in either direction), or a
    /// new one. A new connection also gets a reader.
    async fn connection(&self, peer: &str) -> Option<Connection> {
        if let Some(conn) = self.readers.connections.get(peer) {
            return Some(conn);
        }
        let dialled = tokio::select! {
            result = tokio::time::timeout(DIAL_TIMEOUT, self.node.connect(&self.to, CLUSTER_ALPN)) => result,
            () = self.shutdown.cancelled() => return None,
        };
        match dialled {
            Ok(Ok(conn)) => {
                self.readers.connections.insert(peer, conn.clone());
                tokio::spawn(read_frames(
                    conn.clone(),
                    peer.to_owned(),
                    self.readers.clone(),
                    None,
                ));
                Some(conn)
            }
            Ok(Err(err)) => {
                tracing::debug!(peer = %self.to, error = %err, "cluster: iroh dial failed");
                None
            }
            Err(_) => None,
        }
    }

    /// Write `frame` on its own uni stream.
    async fn write(&self, conn: &Connection, frame: &[u8]) -> bool {
        let written = async {
            let mut stream = conn.open_uni().await.ok()?;
            stream.write_all(frame).await.ok()?;
            stream.finish().ok()
        };
        tokio::select! {
            result = tokio::time::timeout(WRITE_TIMEOUT, written) => matches!(result, Ok(Some(()))),
            () = self.shutdown.cancelled() => false,
        }
    }
}

/// Own one peer's frames: the farewell lane first, then the queue.
///
/// The same order and the same drop rules as the TCP writer: a frame that
/// cannot be written is dropped, and the next push carries the document
/// again.
async fn iroh_writer(
    writer: Writer,
    mut queue: mpsc::Receiver<Vec<u8>>,
    mut departure: mpsc::Receiver<Vec<u8>>,
) {
    let Some(peer) = IrohPeerTransport::peer_id(&writer.to).map(ToOwned::to_owned) else {
        tracing::debug!(peer = %writer.to, "cluster: not an iroh peer address; no writer");
        return;
    };
    let mut backoff = RECONNECT_BACKOFF_MIN;
    loop {
        let queued = tokio::select! {
            biased;
            Some(frame) = departure.recv() => Some(frame),
            frame = queue.recv() => frame,
            () = writer.shutdown.cancelled() => None,
        };
        let Some(frame) = queued else { return };
        let _held = InFlightFrame::claim(&writer.in_flight);

        let Some(conn) = writer.connection(&peer).await else {
            if writer.shutdown.is_cancelled() {
                return;
            }
            tokio::select! {
                () = tokio::time::sleep(crate::cluster::jittered(backoff, writer.entropy.as_ref())) => {}
                () = writer.shutdown.cancelled() => return,
            }
            backoff = backoff.saturating_mul(2).min(RECONNECT_BACKOFF_MAX);
            continue;
        };
        backoff = RECONNECT_BACKOFF_MIN;
        if !writer.write(&conn, &frame).await {
            writer.readers.connections.forget(&peer, &conn);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::cluster::ClusterHandle;
    use crate::cluster::membership::{ClusterState, MemberRecord};
    use crate::cluster::node::{ClusterNode, ClusterRuntimeConfig};
    use crate::cluster::wire::{self, ClusterMessage, RejectReason};
    use crate::entropy::SeededEntropy;

    const SECRET: &[u8] = b"a-shared-cluster-secret-value-32";
    const OTHER_SECRET: &[u8] = b"a-different-cluster-secret-value";
    const PUSH: Duration = Duration::from_millis(50);
    const SUSPICION: Duration = Duration::from_secs(1);
    const DEADLINE: Duration = Duration::from_secs(15);
    /// TEST-NET-1: an address that nobody can dial.
    const UNREACHABLE: &str = "192.0.2.1:9";

    async fn iroh_node(dir: &std::path::Path, name: &str) -> IrohNode {
        IrohNode::bind(&crate::config::IrohConfig {
            enabled: true,
            secret_key_path: dir.join(name),
            bind_addr: "127.0.0.1:0".to_owned(),
            ..crate::config::IrohConfig::default()
        })
        .await
        .expect("binds on loopback")
    }

    struct Started {
        handle: ClusterHandle,
        node: IrohNode,
        token: CancellationToken,
    }

    fn start(
        node: IrohNode,
        id: &str,
        secret: &[u8],
        seeds: Vec<String>,
        advertise: Option<SocketAddr>,
    ) -> Started {
        let token = CancellationToken::new();
        let transport = IrohPeerTransport::new(node.clone(), advertise);
        tokio::spawn(node.clone().run(token.child_token()));
        let handle = ClusterNode::start(
            ClusterRuntimeConfig {
                cluster_name: "autumn".to_owned(),
                secret: secret.to_vec(),
                node_id: Some(id.to_owned()),
                advertise_addr: None,
                seed_peers: seeds,
                push_interval: PUSH,
                suspicion_timeout: SUSPICION,
            },
            Arc::new(SeededEntropy::new(7)),
            Arc::new(crate::time::SystemClock),
            token.clone(),
            Arc::new(transport),
        )
        .expect("the node starts");
        Started {
            handle,
            node,
            token,
        }
    }

    fn view(handle: &ClusterHandle) -> Vec<String> {
        let mut ids: Vec<String> = handle.members().into_iter().map(|m| m.id).collect();
        ids.sort();
        ids
    }

    async fn wait_for(mut done: impl FnMut() -> bool) -> bool {
        let poll = async {
            while !done() {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        };
        tokio::time::timeout(DEADLINE, poll).await.is_ok()
    }

    async fn stop(nodes: Vec<Started>) {
        for started in nodes {
            started.token.cancel();
            started.node.close().await;
        }
    }

    fn rejected(handle: &ClusterHandle, reason: RejectReason) -> u64 {
        handle
            .inner
            .metrics
            .rejections_by_reason()
            .into_iter()
            .find(|(label, _)| *label == reason.label())
            .map_or(0, |(_, count)| count)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn two_nodes_converge_over_iroh() {
        let dir = tempfile::tempdir().unwrap();
        let a = start(
            iroh_node(dir.path(), "a").await,
            "node-a",
            SECRET,
            vec![],
            None,
        );
        let seed = a.node.dial_addr();
        let b = start(
            iroh_node(dir.path(), "b").await,
            "node-b",
            SECRET,
            vec![seed],
            None,
        );

        let both = ["node-a".to_owned(), "node-b".to_owned()];
        assert!(
            wait_for(|| view(&a.handle) == both && view(&b.handle) == both).await,
            "both views hold both nodes: a={:?} b={:?}",
            view(&a.handle),
            view(&b.handle)
        );
        let b_record = a
            .handle
            .members()
            .into_iter()
            .find(|m| m.id == "node-b")
            .expect("a knows b");
        assert!(
            b_record.addr.starts_with(&b.node.id().to_string()),
            "a node advertises <endpoint-id>@<addr>: {}",
            b_record.addr
        );
        stop(vec![a, b]).await;
    }

    /// The one-way cluster: `b` advertises an address that nobody can dial,
    /// as a node behind NAT does. `a` must still reach `b`, on the
    /// connection that `b` opened.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_node_that_cannot_be_dialed_still_hears_back() {
        let dir = tempfile::tempdir().unwrap();
        let a = start(
            iroh_node(dir.path(), "a").await,
            "node-a",
            SECRET,
            vec![],
            None,
        );
        let seed = a.node.dial_addr();
        let b = start(
            iroh_node(dir.path(), "b").await,
            "node-b",
            SECRET,
            vec![seed],
            Some(UNREACHABLE.parse().unwrap()),
        );

        let both = ["node-a".to_owned(), "node-b".to_owned()];
        assert!(
            wait_for(|| view(&a.handle) == both && view(&b.handle) == both).await,
            "the node behind NAT sees the whole cluster: a={:?} b={:?}",
            view(&a.handle),
            view(&b.handle)
        );
        let advertised = a
            .handle
            .members()
            .into_iter()
            .find(|m| m.id == "node-b")
            .map(|m| m.addr)
            .unwrap_or_default();
        assert!(advertised.ends_with(UNREACHABLE), "{advertised}");
        stop(vec![a, b]).await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_node_with_the_wrong_secret_never_joins() {
        let dir = tempfile::tempdir().unwrap();
        let a = start(
            iroh_node(dir.path(), "a").await,
            "node-a",
            SECRET,
            vec![],
            None,
        );
        let seed = a.node.dial_addr();
        let intruder = start(
            iroh_node(dir.path(), "x").await,
            "node-x",
            OTHER_SECRET,
            vec![seed],
            None,
        );
        assert!(
            wait_for(|| rejected(&a.handle, RejectReason::Mac) > 0).await,
            "the intruder's frames arrive and fail the MAC"
        );
        assert_eq!(view(&a.handle), ["node-a".to_owned()]);
        stop(vec![a, intruder]).await;
    }

    /// A holder of the secret signs a frame as `node-b`, with `node-b`'s own
    /// record, from its own key. The record names `b`'s endpoint id, the
    /// connection proves another id, so the frame is refused.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_frame_whose_sender_names_another_endpoint_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let a = start(
            iroh_node(dir.path(), "a").await,
            "node-a",
            SECRET,
            vec![],
            None,
        );
        let b = iroh_node(dir.path(), "b").await;
        let x = iroh_node(dir.path(), "x").await;

        let mut forged = ClusterState::default();
        forged
            .members
            .insert("node-b".to_owned(), MemberRecord::alive(b.dial_addr(), 1));
        let envelope = wire::sign_envelope(
            SECRET,
            "autumn",
            "node-b",
            1,
            1,
            &ClusterMessage::StatePush { state: forged },
        )
        .expect("signs");
        let frame = wire::encode_frame(&envelope).expect("frames");

        let conn = x
            .connect(&a.node.dial_addr(), CLUSTER_ALPN)
            .await
            .expect("x connects");
        let mut stream = conn.open_uni().await.expect("opens a stream");
        stream.write_all(&frame).await.expect("writes");
        stream.finish().expect("finishes");

        assert!(
            wait_for(|| rejected(&a.handle, RejectReason::Sender) > 0).await,
            "the frame is refused as a sender mismatch"
        );
        assert_eq!(view(&a.handle), ["node-a".to_owned()], "b never joins");
        stop(vec![a]).await;
        b.close().await;
        x.close().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dial_targets_keep_one_address_per_id_and_never_this_node() {
        let dir = tempfile::tempdir().unwrap();
        let node = iroh_node(dir.path(), "a").await;
        let own = node.id().to_string();
        let transport = IrohPeerTransport::new(node.clone(), None);
        let peer = "ae58ff8833241ac82d6ff7611046ed67b5072d142c588d0063e942d9a75502b6";
        let targets: BTreeSet<String> = [
            peer.to_owned(),
            format!("{peer}@10.0.0.2:7947"),
            own.clone(),
            format!("{own}@127.0.0.1:1"),
            "10.0.0.9:7946".to_owned(),
        ]
        .into_iter()
        .collect();
        let kept: Vec<String> = transport.dial_targets(targets).into_iter().collect();
        assert_eq!(kept, vec![format!("{peer}@10.0.0.2:7947")]);

        assert!(transport.verify_sender(peer, Some(&format!("{peer}@10.0.0.2:7947"))));
        assert!(transport.verify_sender(peer, Some(peer)));
        assert!(transport.verify_sender(peer, None), "no record to compare");
        assert!(!transport.verify_sender(peer, Some(&own)));
        assert!(!transport.verify_sender(peer, Some("10.0.0.9:7946")));

        let advertised = transport.advertise_addr(None);
        assert!(
            advertised.starts_with(&format!("{own}@127.0.0.1:")),
            "{advertised}"
        );
        assert_eq!(
            transport.advertise_addr(Some("203.0.113.5:7947")),
            format!("{own}@203.0.113.5:7947")
        );
        node.close().await;
    }
}
