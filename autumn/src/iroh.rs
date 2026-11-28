//! One iroh endpoint per process.
//!
//! iroh gives the process a QUIC endpoint whose address is an ed25519 public
//! key (an [`EndpointId`]). Peers dial the key. The TLS handshake proves that
//! the peer holds the key. With a relay, iroh also goes through NAT.
//!
//! [`IrohNode`] owns the endpoint and gives each incoming connection to the
//! handler for its ALPN. The cluster transport is one handler. An ALPN that no
//! handler registered fails the handshake.
//!
//! The default config talks to no third party: no relay and no address
//! lookup. Peers dial `<endpoint-id>@<ip:port>`. See `[iroh]` in the
//! configuration guide.
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

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::str::FromStr as _;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use ::iroh::endpoint::{Connection, Incoming, VarInt, presets};
use ::iroh::{Endpoint, EndpointAddr, PublicKey, RelayMap, RelayMode, RelayUrl, SecretKey};
use tokio::sync::{Semaphore, mpsc};
use tokio_util::sync::CancellationToken;

use crate::config::{IrohAddressLookup, IrohConfig, IrohRelayMode, parse_iroh_peer};

pub use ::iroh::EndpointId;

/// The ALPN of the cluster transport.
pub const CLUSTER_ALPN: &[u8] = b"autumn/cluster/1";

/// Time limit for one incoming handshake.
const HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(10);

/// Maximum number of incoming handshakes in progress. More are refused.
const MAX_PENDING_HANDSHAKES: usize = 64;

/// Depth of each handler queue. A connection that finds the queue full is
/// closed.
const HANDLER_QUEUE: usize = 64;

/// The QUIC close code when this node refuses a connection.
const REFUSED: u32 = 1;

/// A failure to set up or use the iroh endpoint.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum IrohError {
    /// The secret key file could not be read or written.
    #[error("iroh: secret key file {}: {source}", path.display())]
    KeyIo {
        /// The key file.
        path: PathBuf,
        /// The I/O error.
        source: std::io::Error,
    },
    /// The secret key file does not hold a key.
    #[error(
        "iroh: secret key file {} must hold 32 bytes (raw, or as 64 hex characters)",
        path.display()
    )]
    KeyLength {
        /// The key file.
        path: PathBuf,
    },
    /// Group or others can access the secret key file.
    #[error(
        "iroh: secret key file {} has mode {mode:o}; only its owner may access it (chmod \
         0600), or set [iroh] allow_readable_secret_key = true",
        path.display()
    )]
    KeyMode {
        /// The key file.
        path: PathBuf,
        /// The permission bits of the file.
        mode: u32,
    },
    /// A setting in `[iroh]` is not valid.
    #[error("iroh: {0}")]
    Config(String),
    /// The endpoint could not bind.
    #[error("iroh: could not bind the endpoint on {addr}: {reason}")]
    Bind {
        /// The bind address.
        addr: String,
        /// Why the bind failed.
        reason: String,
    },
    /// A peer address is not `<endpoint-id>` or `<endpoint-id>@<ip:port>`.
    #[error("iroh: {peer:?} is not a peer address: {reason}")]
    Peer {
        /// The peer address.
        peer: String,
        /// Why it is not valid.
        reason: String,
    },
    /// A connection to a peer failed.
    #[error("iroh: could not connect to {peer}: {reason}")]
    Connect {
        /// The peer address.
        peer: String,
        /// Why the connection failed.
        reason: String,
    },
}

/// Format a peer address: `<id>`, or `<id>@<ip:port>`.
#[must_use]
pub fn format_peer_addr(id: &str, direct: Option<SocketAddr>) -> String {
    direct.map_or_else(|| id.to_owned(), |addr| format!("{id}@{addr}"))
}

/// Parse a peer address into an [`EndpointAddr`].
///
/// # Errors
///
/// Returns [`IrohError::Peer`] when `peer` is not `<id>` or `<id>@<ip:port>`.
pub fn peer_endpoint_addr(peer: &str) -> Result<EndpointAddr, IrohError> {
    let invalid = |reason: String| IrohError::Peer {
        peer: peer.to_owned(),
        reason,
    };
    let (id, direct) = parse_iroh_peer(peer).map_err(invalid)?;
    let id = PublicKey::from_str(id).map_err(|err| invalid(err.to_string()))?;
    let addr = EndpointAddr::new(id);
    Ok(match direct {
        Some(direct) => addr.with_ip_addr(direct),
        None => addr,
    })
}

/// Load the endpoint's secret key, or create it on first boot.
///
/// A new file gets mode `0600`, in a directory with mode `0700`. The file
/// holds the key as 64 hex characters. A file with 32 raw bytes is read too.
/// On Unix, a file that group or others can read is refused.
///
/// # Errors
///
/// Returns an [`IrohError`] when the file cannot be read or written, does not
/// hold a key, or is readable by others.
pub fn load_or_create_secret_key(path: &Path) -> Result<SecretKey, IrohError> {
    load_or_create_secret_key_with(path, false)
}

/// [`load_or_create_secret_key`], with the `[iroh] allow_readable_secret_key`
/// opt-in. When `allow_readable` is true, a file that group or others can
/// access is used, and a warning is logged.
///
/// # Errors
///
/// As [`load_or_create_secret_key`].
pub fn load_or_create_secret_key_with(
    path: &Path,
    allow_readable: bool,
) -> Result<SecretKey, IrohError> {
    let io = |source| IrohError::KeyIo {
        path: path.to_owned(),
        source,
    };
    match std::fs::File::open(path) {
        Ok(mut file) => {
            // The mode and the bytes come from one open file, so a swap of
            // the path between the two reads cannot mix two files.
            match check_key_mode(path, &file.metadata().map_err(io)?) {
                Err(IrohError::KeyMode { mode, .. }) if allow_readable => tracing::warn!(
                    path = %path.display(),
                    mode = %format_args!("{mode:o}"),
                    "iroh: the secret key file is not owner-only; \
                     allow_readable_secret_key = true lets it through"
                ),
                checked => checked?,
            }
            let mut bytes = Vec::new();
            std::io::Read::read_to_end(&mut file, &mut bytes).map_err(io)?;
            decode_secret_key(&bytes).ok_or_else(|| IrohError::KeyLength {
                path: path.to_owned(),
            })
        }
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
            let key = SecretKey::generate();
            match write_new_key(path, &key) {
                Ok(()) => Ok(key),
                // Another process created the key first: use that one.
                Err(err) if err.kind() == std::io::ErrorKind::AlreadyExists => {
                    load_or_create_secret_key_with(path, allow_readable)
                }
                Err(err) => Err(io(err)),
            }
        }
        Err(err) => Err(io(err)),
    }
}

fn decode_secret_key(bytes: &[u8]) -> Option<SecretKey> {
    if let Ok(raw) = <[u8; 32]>::try_from(bytes) {
        return Some(SecretKey::from_bytes(&raw));
    }
    let text = std::str::from_utf8(bytes).ok()?.trim();
    let decoded = hex::decode(text).ok()?;
    let raw = <[u8; 32]>::try_from(decoded.as_slice()).ok()?;
    Some(SecretKey::from_bytes(&raw))
}

#[cfg(unix)]
fn check_key_mode(path: &Path, metadata: &std::fs::Metadata) -> Result<(), IrohError> {
    use std::os::unix::fs::PermissionsExt as _;
    let mode = metadata.permissions().mode() & 0o777;
    #[allow(
        clippy::verbose_bit_mask,
        reason = "0o077 reads as the group and other bits"
    )]
    let owner_only = mode & 0o077 == 0;
    if owner_only {
        Ok(())
    } else {
        Err(IrohError::KeyMode {
            path: path.to_owned(),
            mode,
        })
    }
}

#[cfg(not(unix))]
#[allow(clippy::unnecessary_wraps, reason = "same signature as the Unix check")]
const fn check_key_mode(_path: &Path, _metadata: &std::fs::Metadata) -> Result<(), IrohError> {
    Ok(())
}

fn write_new_key(path: &Path, key: &SecretKey) -> std::io::Result<()> {
    use std::io::Write as _;
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        let mut dir = std::fs::DirBuilder::new();
        dir.recursive(true);
        #[cfg(unix)]
        std::os::unix::fs::DirBuilderExt::mode(&mut dir, 0o700);
        dir.create(parent)?;
    }
    // Write a temporary file, then link it to `path`. A crash never leaves a
    // part key at `path`, and the link fails when `path` exists, so two
    // processes that start at once never share or replace a key.
    let mut temp_name = path.as_os_str().to_owned();
    temp_name.push(format!(".tmp-{}", key.public()));
    let temp = PathBuf::from(temp_name);
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    std::os::unix::fs::OpenOptionsExt::mode(&mut options, 0o600);
    let written = (|| {
        let mut file = options.open(&temp)?;
        file.write_all(hex::encode(key.to_bytes()).as_bytes())?;
        file.write_all(b"\n")?;
        file.sync_all()?;
        std::fs::hard_link(&temp, path)
    })();
    let _ = std::fs::remove_file(&temp);
    written
}

/// The queue and the allow-list of one ALPN.
#[derive(Clone)]
struct Handler {
    queue: mpsc::Sender<Connection>,
    allow: Option<Arc<BTreeSet<EndpointId>>>,
}

struct Inner {
    endpoint: Endpoint,
    handlers: Mutex<BTreeMap<Vec<u8>, Handler>>,
    allowed_peers: BTreeSet<EndpointId>,
}

/// The process's iroh endpoint and its ALPN registry.
///
/// Cheap to clone. Call [`IrohNode::run`] once to accept connections.
#[derive(Clone)]
pub struct IrohNode {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for IrohNode {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("IrohNode")
            .field("id", &self.id().to_string())
            .finish_non_exhaustive()
    }
}

impl IrohNode {
    /// Bind the endpoint from `[iroh]`.
    ///
    /// Loads or creates the secret key, then binds `bind_addr` with the
    /// relay and address-lookup modes of the config.
    ///
    /// # Errors
    ///
    /// Returns an [`IrohError`] when the key, a setting or the bind fails.
    pub async fn bind(config: &IrohConfig) -> Result<Self, IrohError> {
        let secret = load_or_create_secret_key_with(
            &config.secret_key_path,
            config.allow_readable_secret_key,
        )?;
        let bind_addr: SocketAddr = config
            .bind_addr
            .parse()
            .map_err(|err| IrohError::Config(format!("bind_addr {:?}: {err}", config.bind_addr)))?;
        let allowed_peers = config
            .allowed_peers
            .iter()
            .map(|peer| {
                PublicKey::from_str(peer)
                    .map_err(|err| IrohError::Config(format!("allowed_peers {peer:?}: {err}")))
            })
            .collect::<Result<BTreeSet<_>, _>>()?;

        let bind_failed = |reason: String| IrohError::Bind {
            addr: config.bind_addr.clone(),
            reason,
        };
        let mut builder = Endpoint::builder(presets::Minimal)
            .secret_key(secret)
            .relay_mode(relay_mode(config)?)
            .clear_ip_transports()
            .bind_addr(bind_addr)
            .map_err(|err| bind_failed(err.to_string()))?;
        if config.address_lookup == IrohAddressLookup::N0Dns {
            builder = builder
                .address_lookup(::iroh::address_lookup::PkarrPublisher::n0_dns())
                .address_lookup(::iroh::address_lookup::PkarrResolver::n0_dns())
                .address_lookup(::iroh::address_lookup::DnsAddressLookup::n0_dns());
        }
        let endpoint = builder
            .bind()
            .await
            .map_err(|err| bind_failed(err.to_string()))?;
        if bind_addr.ip().is_unspecified() {
            wait_for_interface_addrs(&endpoint).await;
        }
        Ok(Self {
            inner: Arc::new(Inner {
                endpoint,
                handlers: Mutex::new(BTreeMap::new()),
                allowed_peers,
            }),
        })
    }

    /// This endpoint's id: its public key.
    #[must_use]
    pub fn id(&self) -> EndpointId {
        self.inner.endpoint.id()
    }

    /// The underlying endpoint.
    #[must_use]
    pub fn endpoint(&self) -> &Endpoint {
        &self.inner.endpoint
    }

    /// The `[iroh] allowed_peers` ids.
    #[must_use]
    pub fn allowed_peers(&self) -> &BTreeSet<EndpointId> {
        &self.inner.allowed_peers
    }

    /// A direct address of this endpoint that a peer can dial.
    ///
    /// The bound socket when it names an IP. When the bind is on the
    /// unspecified address, the first interface address iroh found. `None`
    /// when there is none.
    #[must_use]
    pub fn direct_addr(&self) -> Option<SocketAddr> {
        let bound = self.inner.endpoint.bound_sockets();
        if let Some(addr) = bound.iter().find(|addr| !addr.ip().is_unspecified()) {
            return Some(*addr);
        }
        let port = bound.first().map(SocketAddr::port)?;
        self.inner
            .endpoint
            .addr()
            .ip_addrs()
            .find(|addr| addr.port() == port)
            .copied()
    }

    /// This endpoint's peer address: `<id>@<direct address>`, or `<id>`.
    #[must_use]
    pub fn dial_addr(&self) -> String {
        format_peer_addr(&self.id().to_string(), self.direct_addr())
    }

    /// Accept connections for `alpn`.
    ///
    /// When `[iroh] allowed_peers` is set, only those peers get through.
    /// Else any peer does. Returns the queue of connections. A second call
    /// for the same ALPN replaces the first.
    ///
    /// The endpoint can accept 0-RTT data. A handler that must not act on
    /// replayable data refuses streams where `RecvStream::is_0rtt` is true.
    #[must_use]
    pub fn register(&self, alpn: &[u8]) -> mpsc::Receiver<Connection> {
        let allow = (!self.inner.allowed_peers.is_empty())
            .then(|| Arc::new(self.inner.allowed_peers.clone()));
        self.add_handler(alpn, allow)
    }

    /// Accept connections for `alpn` only from the peers in `allow`.
    ///
    /// The handshake completes, then a peer not in `allow` is closed before
    /// the handler sees it.
    #[must_use]
    pub fn register_with_allow_list(
        &self,
        alpn: &[u8],
        allow: impl IntoIterator<Item = EndpointId>,
    ) -> mpsc::Receiver<Connection> {
        self.add_handler(alpn, Some(Arc::new(allow.into_iter().collect())))
    }

    fn add_handler(
        &self,
        alpn: &[u8],
        allow: Option<Arc<BTreeSet<EndpointId>>>,
    ) -> mpsc::Receiver<Connection> {
        let (queue, accepted) = mpsc::channel(HANDLER_QUEUE);
        let alpns = {
            let mut handlers = self.lock_handlers();
            handlers.insert(alpn.to_vec(), Handler { queue, allow });
            handlers.keys().cloned().collect()
        };
        self.inner.endpoint.set_alpns(alpns);
        accepted
    }

    fn lock_handlers(&self) -> std::sync::MutexGuard<'_, BTreeMap<Vec<u8>, Handler>> {
        self.inner
            .handlers
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Connect to `peer` (`<id>` or `<id>@<ip:port>`) on `alpn`.
    ///
    /// The handshake proves that the remote end holds the key of `<id>`.
    ///
    /// # Errors
    ///
    /// Returns an [`IrohError`] when the address is not valid or the
    /// connection fails.
    pub async fn connect(&self, peer: &str, alpn: &[u8]) -> Result<Connection, IrohError> {
        let addr = peer_endpoint_addr(peer)?;
        self.inner
            .endpoint
            .connect(addr, alpn)
            .await
            .map_err(|err| IrohError::Connect {
                peer: peer.to_owned(),
                reason: err.to_string(),
            })
    }

    /// Connect to the peer with endpoint id `id` on `alpn`, trying each
    /// address in `direct`. iroh uses the addresses it already knows too.
    ///
    /// # Errors
    ///
    /// Returns an [`IrohError`] when `id` is not an endpoint id or the
    /// connection fails.
    pub async fn connect_direct(
        &self,
        id: &str,
        direct: impl IntoIterator<Item = SocketAddr>,
        alpn: &[u8],
    ) -> Result<Connection, IrohError> {
        let key = PublicKey::from_str(id).map_err(|err| IrohError::Peer {
            peer: id.to_owned(),
            reason: err.to_string(),
        })?;
        let addr = direct
            .into_iter()
            .fold(EndpointAddr::new(key), EndpointAddr::with_ip_addr);
        self.inner
            .endpoint
            .connect(addr, alpn)
            .await
            .map_err(|err| IrohError::Connect {
                peer: id.to_owned(),
                reason: err.to_string(),
            })
    }

    /// Accept connections and give each to the handler of its ALPN, until
    /// `shutdown` fires or the endpoint closes.
    pub async fn run(self, shutdown: CancellationToken) {
        let pending = Arc::new(Semaphore::new(MAX_PENDING_HANDSHAKES));
        loop {
            let incoming = tokio::select! {
                incoming = self.inner.endpoint.accept() => incoming,
                () = shutdown.cancelled() => return,
            };
            let Some(incoming) = incoming else { return };
            // Under load, ask the client to prove its address first. A spoofed
            // source cannot answer, so it never holds a handshake slot.
            if pending.available_permits() < MAX_PENDING_HANDSHAKES.div_euclid(2)
                && !incoming.remote_addr_validated()
            {
                if let Err(err) = incoming.retry() {
                    err.into_incoming().refuse();
                }
                continue;
            }
            let Ok(permit) = Arc::clone(&pending).try_acquire_owned() else {
                tracing::debug!("iroh: too many handshakes in progress; refusing a connection");
                incoming.refuse();
                continue;
            };
            let node = self.clone();
            let shutdown = shutdown.child_token();
            tokio::spawn(async move {
                let _permit = permit;
                tokio::select! {
                    () = node.dispatch(incoming) => {}
                    () = shutdown.cancelled() => {}
                }
            });
        }
    }

    async fn dispatch(&self, incoming: Incoming) {
        let Ok(accepting) = incoming.accept() else {
            return;
        };
        let conn = match tokio::time::timeout(HANDSHAKE_TIMEOUT, accepting).await {
            Ok(Ok(conn)) => conn,
            Ok(Err(err)) => {
                tracing::debug!(error = %err, "iroh: incoming handshake failed");
                return;
            }
            Err(_) => {
                tracing::debug!("iroh: incoming handshake timed out");
                return;
            }
        };
        let handler = self.lock_handlers().get(conn.alpn()).cloned();
        let Some(handler) = handler else {
            conn.close(VarInt::from_u32(REFUSED), b"unknown protocol");
            return;
        };
        let remote = conn.remote_id();
        if handler
            .allow
            .as_ref()
            .is_some_and(|allow| !allow.contains(&remote))
        {
            tracing::debug!(peer = %remote, "iroh: peer is not on the allow-list; closing");
            conn.close(VarInt::from_u32(REFUSED), b"not allowed");
            return;
        }
        if let Err(
            mpsc::error::TrySendError::Full(conn) | mpsc::error::TrySendError::Closed(conn),
        ) = handler.queue.try_send(conn)
        {
            conn.close(VarInt::from_u32(REFUSED), b"busy");
        }
    }

    /// Close the endpoint and every connection on it.
    pub async fn close(&self) {
        self.inner.endpoint.close().await;
    }
}

/// Time limit to close the endpoint at shutdown.
pub(crate) const CLOSE_BUDGET: Duration = Duration::from_secs(2);

/// Bind the `[iroh]` endpoint at boot, put it on `state`, and accept
/// connections until `shutdown` fires.
///
/// Returns `None` when `[iroh] enabled = false`. The endpoint id is public and
/// is logged. The secret key is never logged.
///
/// # Errors
///
/// Returns an [`IrohError`] when the endpoint cannot bind.
pub async fn start_from_config(
    state: &crate::state::AppState,
    config: &IrohConfig,
    shutdown: &CancellationToken,
) -> Result<Option<IrohNode>, IrohError> {
    if !config.enabled {
        return Ok(None);
    }
    let node = IrohNode::bind(config).await?;
    tracing::info!(
        endpoint_id = %node.id(),
        dial_addr = %node.dial_addr(),
        relay = ?config.relay,
        address_lookup = ?config.address_lookup,
        "iroh endpoint bound"
    );
    state.insert_extension(node.clone());
    tokio::spawn(node.clone().run(shutdown.clone()));
    Ok(Some(node))
}

/// Time limit to find the interface addresses after a wildcard bind.
const INTERFACE_ADDRS_BUDGET: Duration = Duration::from_secs(1);

/// Wait until iroh knows an interface address, or for
/// [`INTERFACE_ADDRS_BUDGET`]. A wildcard bind finds them after `bind`
/// returns, and the dial address that boot logs needs one.
async fn wait_for_interface_addrs(endpoint: &Endpoint) {
    let found = async {
        while endpoint.addr().ip_addrs().next().is_none() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    };
    let _ = tokio::time::timeout(INTERFACE_ADDRS_BUDGET, found).await;
}

fn relay_mode(config: &IrohConfig) -> Result<RelayMode, IrohError> {
    Ok(match config.relay {
        IrohRelayMode::Disabled => RelayMode::Disabled,
        IrohRelayMode::N0 => RelayMode::Default,
        IrohRelayMode::Custom => {
            let urls = config
                .relay_urls
                .iter()
                .map(|url| {
                    url.parse::<RelayUrl>()
                        .map_err(|err| IrohError::Config(format!("relay_urls {url:?}: {err}")))
                })
                .collect::<Result<Vec<_>, _>>()?;
            RelayMode::Custom(urls.into_iter().collect::<RelayMap>())
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::time::Duration;

    const ALPN: &[u8] = b"autumn/test/1";
    const OTHER_ALPN: &[u8] = b"autumn/other/1";
    const DEADLINE: Duration = Duration::from_secs(10);

    fn config(dir: &std::path::Path, name: &str) -> crate::config::IrohConfig {
        crate::config::IrohConfig {
            enabled: true,
            secret_key_path: dir.join(name),
            bind_addr: "127.0.0.1:0".to_owned(),
            ..crate::config::IrohConfig::default()
        }
    }

    #[test]
    fn a_new_secret_key_file_is_owner_only_and_reused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("keys").join("iroh.key");
        let first = load_or_create_secret_key(&path).expect("creates the key");
        let second = load_or_create_secret_key(&path).expect("reads the key");
        assert_eq!(first.public(), second.public(), "the identity is stable");
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
            assert_eq!(mode, 0o600, "only the owner can read the key");
        }
    }

    #[test]
    fn a_secret_key_file_of_the_wrong_length_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("iroh.key");
        std::fs::write(&path, b"short").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600)).unwrap();
        }
        let err = load_or_create_secret_key(&path).expect_err("not a key");
        assert!(err.to_string().contains("32 bytes"), "{err}");
    }

    #[cfg(unix)]
    #[test]
    fn a_secret_key_file_that_others_can_read_is_refused() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("iroh.key");
        load_or_create_secret_key(&path).unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
        let err = load_or_create_secret_key(&path).expect_err("group and others can read it");
        assert!(err.to_string().contains("0600"), "{err}");
        assert!(
            err.to_string().contains("allow_readable_secret_key"),
            "{err}"
        );
        let key = load_or_create_secret_key_with(&path, true).expect("the opt-in lets it through");
        assert_eq!(
            key.public(),
            load_or_create_secret_key_with(&path, true)
                .unwrap()
                .public()
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn two_nodes_connect_by_id_and_address_and_dispatch_by_alpn() {
        let dir = tempfile::tempdir().unwrap();
        let server = IrohNode::bind(&config(dir.path(), "server.key"))
            .await
            .unwrap();
        let client = IrohNode::bind(&config(dir.path(), "client.key"))
            .await
            .unwrap();
        let shutdown = tokio_util::sync::CancellationToken::new();
        let mut accepted = server.register(ALPN);
        tokio::spawn(server.clone().run(shutdown.clone()));

        let conn = tokio::time::timeout(DEADLINE, client.connect(&server.dial_addr(), ALPN))
            .await
            .expect("in time")
            .expect("connects by id@addr");
        let incoming = tokio::time::timeout(DEADLINE, accepted.recv())
            .await
            .expect("in time")
            .expect("the ALPN handler gets the connection");
        assert_eq!(incoming.remote_id(), client.id());
        assert_eq!(conn.remote_id(), server.id());
        shutdown.cancel();
        server.close().await;
        client.close().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_unregistered_alpn_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let server = IrohNode::bind(&config(dir.path(), "server.key"))
            .await
            .unwrap();
        let client = IrohNode::bind(&config(dir.path(), "client.key"))
            .await
            .unwrap();
        let shutdown = tokio_util::sync::CancellationToken::new();
        let _accepted = server.register(ALPN);
        tokio::spawn(server.clone().run(shutdown.clone()));
        let result =
            tokio::time::timeout(DEADLINE, client.connect(&server.dial_addr(), OTHER_ALPN)).await;
        assert!(
            !matches!(result, Ok(Ok(_))),
            "a protocol nobody registered must not connect"
        );
        shutdown.cancel();
        server.close().await;
        client.close().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn an_allow_list_turns_away_other_peers() {
        let dir = tempfile::tempdir().unwrap();
        let friend = IrohNode::bind(&config(dir.path(), "friend.key"))
            .await
            .unwrap();
        let stranger = IrohNode::bind(&config(dir.path(), "stranger.key"))
            .await
            .unwrap();
        let server = IrohNode::bind(&config(dir.path(), "server.key"))
            .await
            .unwrap();
        let shutdown = tokio_util::sync::CancellationToken::new();
        let mut accepted = server.register_with_allow_list(ALPN, [friend.id()]);
        tokio::spawn(server.clone().run(shutdown.clone()));

        let refused = tokio::time::timeout(DEADLINE, async {
            let conn = stranger.connect(&server.dial_addr(), ALPN).await?;
            conn.closed().await;
            Ok::<_, IrohError>(())
        })
        .await;
        assert!(refused.is_ok(), "the stranger's connection ends");
        assert!(
            tokio::time::timeout(Duration::from_millis(300), accepted.recv())
                .await
                .is_err(),
            "the handler never sees a peer that is not on the list"
        );

        let _conn = friend
            .connect(&server.dial_addr(), ALPN)
            .await
            .expect("the friend connects");
        let incoming = tokio::time::timeout(DEADLINE, accepted.recv())
            .await
            .expect("in time")
            .expect("the handler gets the friend");
        assert_eq!(incoming.remote_id(), friend.id());
        shutdown.cancel();
        for node in [server, friend, stranger] {
            node.close().await;
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_configured_allow_list_applies_to_every_registered_alpn() {
        let dir = tempfile::tempdir().unwrap();
        let friend = IrohNode::bind(&config(dir.path(), "friend.key"))
            .await
            .unwrap();
        let stranger = IrohNode::bind(&config(dir.path(), "stranger.key"))
            .await
            .unwrap();
        let server = IrohNode::bind(&crate::config::IrohConfig {
            allowed_peers: vec![friend.id().to_string()],
            ..config(dir.path(), "server.key")
        })
        .await
        .unwrap();
        let shutdown = tokio_util::sync::CancellationToken::new();
        let mut accepted = server.register(ALPN);
        tokio::spawn(server.clone().run(shutdown.clone()));

        if let Ok(conn) = stranger.connect(&server.dial_addr(), ALPN).await {
            let _ = tokio::time::timeout(DEADLINE, conn.closed()).await;
        }
        let _conn = friend.connect(&server.dial_addr(), ALPN).await.unwrap();
        let first = tokio::time::timeout(DEADLINE, accepted.recv())
            .await
            .expect("in time")
            .expect("a connection");
        assert_eq!(
            first.remote_id(),
            friend.id(),
            "only the friend gets through"
        );
        shutdown.cancel();
        for node in [server, friend, stranger] {
            node.close().await;
        }
    }

    #[test]
    fn a_new_key_file_is_written_whole_and_never_replaced() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("iroh.key");
        let key = load_or_create_secret_key(&path).unwrap();
        let names: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .map(|entry| entry.unwrap().file_name())
            .collect();
        assert_eq!(
            names,
            vec![std::ffi::OsString::from("iroh.key")],
            "no temporary file stays"
        );
        let text = std::fs::read_to_string(&path).unwrap();
        assert_eq!(
            text.trim(),
            hex::encode(key.to_bytes()),
            "the file holds the key as hex"
        );
        assert_eq!(
            write_new_key(&path, &SecretKey::generate())
                .unwrap_err()
                .kind(),
            std::io::ErrorKind::AlreadyExists,
            "a second writer never replaces the key"
        );
        assert_eq!(
            load_or_create_secret_key(&path).unwrap().public(),
            key.public()
        );
    }

    #[test]
    fn a_dial_addr_names_the_id_and_a_bound_socket() {
        let id = "af06a3e3291714e4f356c19c9b15cd1951ec6e6662aa77be07547f289383341d";
        let addr: SocketAddr = "127.0.0.1:7947".parse().unwrap();
        assert_eq!(
            format_peer_addr(id, Some(addr)),
            format!("{id}@127.0.0.1:7947")
        );
        assert_eq!(format_peer_addr(id, None), id);
    }
}
