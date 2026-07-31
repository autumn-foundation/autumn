//! Inbound (server-side) TLS support (issue #1603).
//!
//! This module lets an Autumn app terminate HTTPS in-process, without a
//! sidecar reverse proxy, when `[server.tls]` names a certificate + key.
//! It provides three things:
//!
//! 1. **Fail-fast loading** — [`load_certified_key`] reads the PEM cert chain
//!    and private key from disk, verifies they parse and that the private key
//!    matches the leaf certificate (rustls' [`CertifiedKey::from_der`] compares
//!    `SubjectPublicKeyInfo`), and rejects an already-expired leaf certificate.
//!    Every error names the offending path so a misconfiguration is actionable.
//! 2. **A reloadable resolver** — [`ReloadableCertResolver`] holds the current
//!    [`CertifiedKey`] behind an `RwLock` and implements
//!    [`ResolvesServerCert`], so the certificate can be swapped atomically at
//!    runtime (e.g. after an ACME/`certbot` renewal) without dropping the
//!    listener or restarting the process.
//! 3. **Expiry inspection** — [`inspect_leaf`] returns the leaf certificate's
//!    `notAfter` so `autumn doctor` can warn on near-expiry and fail on an
//!    expired certificate, offline (no server boot, no network).
//!
//! The crypto backend is `ring`, the SAME backend the outbound Postgres TLS
//! path already uses — the workspace deliberately forbids a second TLS backend
//! (no aws-lc-rs / native-tls / openssl).

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};

use rustls::crypto::CryptoProvider;
use rustls::server::{ClientHello, ResolvesServerCert};
use rustls::sign::CertifiedKey;
use rustls_pki_types::pem::PemObject as _;
use rustls_pki_types::{CertificateDer, PrivateKeyDer};

/// Default cert/key reload poll interval, in seconds.
///
/// The running server polls the cert and key file modification times this often
/// to detect an external renewal. 60s is frequent enough to pick up a
/// `certbot`/ACME renewal promptly while imposing a negligible
/// two-`stat`-per-minute cost.
pub const DEFAULT_RELOAD_INTERVAL_SECS: u64 = 60;

/// Something went wrong loading, validating, or inspecting the configured TLS
/// material. Every variant names the offending path so the operator can act on
/// it without guesswork.
#[derive(Debug, thiserror::Error)]
pub enum TlsError {
    /// The certificate file could not be read.
    #[error("failed to read TLS certificate file `{path}`: {source}")]
    ReadCert {
        /// Path that failed to read.
        path: PathBuf,
        /// Underlying I/O error.
        source: std::io::Error,
    },
    /// The private key file could not be read.
    #[error("failed to read TLS private key file `{path}`: {source}")]
    ReadKey {
        /// Path that failed to read.
        path: PathBuf,
        /// Underlying I/O error.
        source: std::io::Error,
    },
    /// The certificate PEM could not be parsed.
    #[error("failed to parse a PEM certificate in `{path}`: {source}")]
    ParseCert {
        /// Path whose PEM failed to parse.
        path: PathBuf,
        /// Underlying PEM parse error.
        source: rustls_pki_types::pem::Error,
    },
    /// No `CERTIFICATE` PEM block was present in the certificate file.
    #[error("no certificates found in `{path}` (expected at least one PEM CERTIFICATE block)")]
    NoCertificates {
        /// Path that contained no certificate.
        path: PathBuf,
    },
    /// The private key PEM could not be parsed.
    #[error("failed to parse a PEM private key in `{path}`: {source}")]
    ParseKey {
        /// Path whose PEM failed to parse.
        path: PathBuf,
        /// Underlying PEM parse error.
        source: rustls_pki_types::pem::Error,
    },
    /// The private key is unusable, or it does not match the leaf certificate.
    #[error(
        "the TLS private key `{key}` is invalid or does not match the leaf certificate `{cert}`: \
         {source}"
    )]
    InvalidKeyPair {
        /// Certificate path.
        cert: PathBuf,
        /// Key path.
        key: PathBuf,
        /// Underlying rustls error.
        source: Box<rustls::Error>,
    },
    /// The leaf certificate DER could not be parsed for expiry inspection.
    #[error("failed to parse the leaf certificate in `{path}` for expiry inspection: {detail}")]
    ParseLeaf {
        /// Certificate path.
        path: PathBuf,
        /// Human-readable parse detail.
        detail: String,
    },
    /// A certificate in the chain (at 1-based `position`) is not valid DER.
    ///
    /// The PEM block decoded, but the bytes are not a parseable X.509
    /// certificate — a malformed intermediate that rustls would store unparsed
    /// and serve to clients that reject the chain.
    #[error("certificate #{position} in the chain in `{path}` is malformed: {detail}")]
    ParseChainCert {
        /// Certificate path.
        path: PathBuf,
        /// 1-based position of the offending certificate in the chain.
        position: usize,
        /// Human-readable parse detail.
        detail: String,
    },
    /// The leaf certificate has already expired.
    #[error("the leaf certificate in `{path}` expired at {not_after} (UNIX {not_after_unix})")]
    Expired {
        /// Certificate path.
        path: PathBuf,
        /// RFC 2822-ish rendering of `notAfter`.
        not_after: String,
        /// `notAfter` as a UNIX timestamp.
        not_after_unix: i64,
    },
    /// The leaf certificate is not yet valid — its `notBefore` is in the future.
    #[error(
        "the leaf certificate in `{path}` is not valid until {not_before} (UNIX {not_before_unix})"
    )]
    NotYetValid {
        /// Certificate path.
        path: PathBuf,
        /// RFC 2822-ish rendering of `notBefore`.
        not_before: String,
        /// `notBefore` as a UNIX timestamp.
        not_before_unix: i64,
    },
    /// A non-leaf certificate in the chain (at 1-based `position`) has already
    /// expired. Normal TLS clients reject the whole chain during path
    /// validation, so refuse it at load/inspection time rather than boot a
    /// listener that serves it. Symmetric to the leaf's [`Self::Expired`].
    #[error(
        "certificate #{position} in the chain in `{path}` expired at {not_after} \
         (UNIX {not_after_unix})"
    )]
    ExpiredChainCert {
        /// Certificate path.
        path: PathBuf,
        /// 1-based position of the offending certificate in the chain.
        position: usize,
        /// RFC 2822-ish rendering of `notAfter`.
        not_after: String,
        /// `notAfter` as a UNIX timestamp.
        not_after_unix: i64,
    },
    /// A non-leaf certificate in the chain (at 1-based `position`) is not yet
    /// valid — its `notBefore` is in the future. Symmetric to the leaf's
    /// [`Self::NotYetValid`].
    #[error(
        "certificate #{position} in the chain in `{path}` is not yet valid until {not_before} \
         (UNIX {not_before_unix})"
    )]
    NotYetValidChainCert {
        /// Certificate path.
        path: PathBuf,
        /// 1-based position of the offending certificate in the chain.
        position: usize,
        /// RFC 2822-ish rendering of `notBefore`.
        not_before: String,
        /// `notBefore` as a UNIX timestamp.
        not_before_unix: i64,
    },
    /// Building the rustls `ServerConfig` failed.
    #[error("failed to build the rustls server configuration: {source}")]
    BuildConfig {
        /// Underlying rustls error.
        source: rustls::Error,
    },
}

/// The `ring` crypto provider used for all inbound TLS. Built once per call;
/// callers that build many configs should cache the returned `Arc`.
#[must_use]
pub fn crypto_provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// Read and PEM-decode the certificate chain at `cert_path`.
fn read_cert_chain(cert_path: &Path) -> Result<Vec<CertificateDer<'static>>, TlsError> {
    let pem = std::fs::read(cert_path).map_err(|source| TlsError::ReadCert {
        path: cert_path.to_path_buf(),
        source,
    })?;
    let mut chain = Vec::new();
    for cert in CertificateDer::pem_slice_iter(&pem) {
        let cert = cert.map_err(|source| TlsError::ParseCert {
            path: cert_path.to_path_buf(),
            source,
        })?;
        chain.push(cert);
    }
    if chain.is_empty() {
        return Err(TlsError::NoCertificates {
            path: cert_path.to_path_buf(),
        });
    }
    Ok(chain)
}

/// Read and PEM-decode the private key at `key_path`.
fn read_private_key(key_path: &Path) -> Result<PrivateKeyDer<'static>, TlsError> {
    let pem = std::fs::read(key_path).map_err(|source| TlsError::ReadKey {
        path: key_path.to_path_buf(),
        source,
    })?;
    PrivateKeyDer::from_pem_slice(&pem).map_err(|source| TlsError::ParseKey {
        path: key_path.to_path_buf(),
        source,
    })
}

/// The `notBefore` and `notAfter` of the leaf certificate, as UNIX timestamps
/// (seconds), returned as `(not_before, not_after)`.
///
/// Parses just enough of the DER to read the validity window; a parse failure
/// is surfaced rather than silently ignored so a corrupt certificate is caught
/// at load time.
fn leaf_validity_unix(cert_path: &Path, leaf: &CertificateDer<'_>) -> Result<(i64, i64), TlsError> {
    use x509_parser::prelude::FromDer as _;

    let (_, parsed) =
        x509_parser::certificate::X509Certificate::from_der(leaf.as_ref()).map_err(|e| {
            TlsError::ParseLeaf {
                path: cert_path.to_path_buf(),
                detail: e.to_string(),
            }
        })?;
    let validity = parsed.validity();
    Ok((
        validity.not_before.timestamp(),
        validity.not_after.timestamp(),
    ))
}

/// Parse and lifetime-check every NON-leaf certificate DER in `chain`, failing
/// fast (naming the 1-based chain position) if any block is not a parseable
/// X.509 certificate OR is outside its validity window at `now_unix`.
///
/// [`read_cert_chain`] only PEM-decodes each block, and rustls'
/// [`CertifiedKey::from_der`] validates only the leaf (`chain[0]`). A malformed
/// — or an expired / not-yet-valid — INTERMEDIATE would therefore be stored
/// unchecked and served, so startup and `autumn doctor` pass while normal TLS
/// clients reject the chain during path validation. Parsing and lifetime-
/// checking each intermediate here turns that into an actionable fail-fast at
/// load/inspection time. The leaf (`chain[0]`) is parsed and lifetime-checked
/// separately (by [`leaf_validity_unix`] and its callers).
///
/// `now_unix` is the current UNIX time; it is a parameter (rather than read
/// internally) so tests can pin "now" deterministically.
fn validate_chain_certs(
    cert_path: &Path,
    chain: &[CertificateDer<'_>],
    now_unix: i64,
) -> Result<(), TlsError> {
    use x509_parser::prelude::FromDer as _;

    // Skip index 0 (the leaf); its DER and validity window are checked by the
    // callers via `leaf_validity_unix`.
    for (idx, cert) in chain.iter().enumerate().skip(1) {
        // 1-based, matching how operators count "certificate #2" in a bundle.
        let position = idx + 1;
        let (_, parsed) = x509_parser::certificate::X509Certificate::from_der(cert.as_ref())
            .map_err(|e| TlsError::ParseChainCert {
                path: cert_path.to_path_buf(),
                position,
                detail: e.to_string(),
            })?;

        // Reject an intermediate outside its validity window, mirroring the
        // leaf's not-yet-valid / expired handling: `notBefore` in the future is
        // as fatal as a past `notAfter`, since either makes clients reject the
        // served chain.
        let validity = parsed.validity();
        let not_before = validity.not_before.timestamp();
        let not_after = validity.not_after.timestamp();
        if now_unix < not_before {
            return Err(TlsError::NotYetValidChainCert {
                path: cert_path.to_path_buf(),
                position,
                not_before: render_unix(not_before),
                not_before_unix: not_before,
            });
        }
        if not_after < now_unix {
            return Err(TlsError::ExpiredChainCert {
                path: cert_path.to_path_buf(),
                position,
                not_after: render_unix(not_after),
                not_after_unix: not_after,
            });
        }
    }
    Ok(())
}

/// Load, validate, and return the certificate + key as a rustls [`CertifiedKey`].
///
/// Fails fast on any of: missing/unreadable file, unparseable PEM, an empty
/// certificate file, a private key that does not match the leaf certificate, or
/// an already-expired leaf certificate.
///
/// `now_unix` is the current UNIX time; it is a parameter (rather than read
/// internally) so tests can pin "now" deterministically.
///
/// # Errors
///
/// Returns a [`TlsError`] describing the first problem encountered.
pub fn load_certified_key(
    cert_path: &Path,
    key_path: &Path,
    provider: &CryptoProvider,
    now_unix: i64,
) -> Result<Arc<CertifiedKey>, TlsError> {
    let chain = read_cert_chain(cert_path)?;
    let key = read_private_key(key_path)?;

    // Reject a leaf outside its validity window before we even build the key:
    // an expired OR not-yet-valid certificate fails every client handshake, so
    // refuse at startup with a clear message rather than booting into a broken
    // listener. Symmetric bounds: `notBefore` in the future is as fatal as a
    // past `notAfter`.
    let (not_before, not_after) = leaf_validity_unix(cert_path, &chain[0])?;
    if now_unix < not_before {
        return Err(TlsError::NotYetValid {
            path: cert_path.to_path_buf(),
            not_before: render_unix(not_before),
            not_before_unix: not_before,
        });
    }
    if not_after < now_unix {
        return Err(TlsError::Expired {
            path: cert_path.to_path_buf(),
            not_after: render_unix(not_after),
            not_after_unix: not_after,
        });
    }

    // Parse and lifetime-check every intermediate before accepting the chain:
    // rustls validates only the leaf, so a malformed — or expired / not-yet-valid
    // — intermediate would otherwise boot a listener that serves a chain normal
    // clients reject during path validation.
    validate_chain_certs(cert_path, &chain, now_unix)?;

    // `from_der` loads the key with the crypto provider (rejecting an invalid
    // key) and compares the key's SubjectPublicKeyInfo against the leaf
    // certificate's, so a cert/key mismatch is caught here.
    let certified = CertifiedKey::from_der(chain, key, provider).map_err(|source| {
        TlsError::InvalidKeyPair {
            cert: cert_path.to_path_buf(),
            key: key_path.to_path_buf(),
            source: Box::new(source),
        }
    })?;

    Ok(Arc::new(certified))
}

/// Render a UNIX timestamp as a UTC string for error messages. Falls back to
/// the raw timestamp if it is out of range for the formatter.
fn render_unix(secs: i64) -> String {
    x509_parser::time::ASN1Time::from_timestamp(secs)
        .ok()
        .map_or_else(|| format!("UNIX {secs}"), |t| t.to_string())
}

/// A [`ResolvesServerCert`] whose certificate can be swapped at runtime.
///
/// Every TLS handshake takes a short read lock to clone the current
/// `Arc<CertifiedKey>`; a reload swaps in a new `Arc` under a brief write lock.
/// Readers never block each other, and a reload never interrupts an in-flight
/// handshake — it only affects handshakes that start after the swap.
#[derive(Debug)]
pub struct ReloadableCertResolver {
    current: RwLock<Arc<CertifiedKey>>,
}

impl ReloadableCertResolver {
    /// Create a resolver serving `initial`.
    #[must_use]
    pub const fn new(initial: Arc<CertifiedKey>) -> Self {
        Self {
            current: RwLock::new(initial),
        }
    }

    /// Atomically replace the served certificate.
    pub fn store(&self, next: Arc<CertifiedKey>) {
        let mut guard = self
            .current
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = next;
    }

    /// Snapshot the currently served certificate.
    #[must_use]
    pub fn current(&self) -> Arc<CertifiedKey> {
        let guard = self
            .current
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        Arc::clone(&guard)
    }
}

impl ResolvesServerCert for ReloadableCertResolver {
    fn resolve(&self, _client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
        Some(self.current())
    }
}

/// Build the rustls [`ServerConfig`](rustls::ServerConfig) that terminates
/// inbound TLS, backed by `resolver` so the certificate stays swappable.
///
/// # Errors
///
/// Returns [`TlsError::BuildConfig`] if rustls rejects the chosen protocol
/// versions for the provider.
pub fn build_server_config(
    provider: Arc<CryptoProvider>,
    resolver: Arc<ReloadableCertResolver>,
) -> Result<Arc<rustls::ServerConfig>, TlsError> {
    let config = rustls::ServerConfig::builder_with_provider(provider)
        .with_safe_default_protocol_versions()
        .map_err(|source| TlsError::BuildConfig { source })?
        .with_no_client_auth()
        .with_cert_resolver(resolver);
    Ok(Arc::new(config))
}

/// Upper bound on TLS handshakes running concurrently at any instant.
///
/// The background acceptor task acquires a permit from a semaphore of this size
/// before spawning each handshake, so a flood of connecting clients can never
/// spawn an unbounded number of handshake tasks: once this many are in flight
/// the acceptor parks on the next permit (still draining the kernel accept
/// queue's backlog as permits free up). 256 is a generous default for a single
/// listener; it could be made configurable later if a deployment needs to tune
/// the in-flight-handshake ceiling.
const MAX_CONCURRENT_HANDSHAKES: usize = 256;

/// Capacity of the channel carrying completed TLS streams to `accept`.
///
/// Bounded so a burst of successful handshakes that outruns axum's consumption
/// applies backpressure (a completed-handshake task parks on `send` while still
/// holding its semaphore permit) rather than buffering without limit.
const READY_CONN_CHANNEL_CAPACITY: usize = 1024;

/// A TLS-terminating [`axum::serve::Listener`] wrapping a
/// [`tokio::net::TcpListener`].
///
/// [`accept`](axum::serve::Listener::accept) yields a decrypted
/// [`TlsStream`](tokio_rustls::server::TlsStream) plus the peer's
/// [`SocketAddr`](std::net::SocketAddr). Because the peer address is a real TCP
/// `SocketAddr`, the rest of the serve stack — connect-info, trusted-proxy
/// resolution, graceful shutdown, SSE/WebSocket streaming — is identical to the
/// plain-TCP path; the only difference is the handshake performed here.
///
/// TCP-accept and the rustls handshake are **decoupled**: a background acceptor
/// task drains `tcp.accept()` and, for each connection, spawns a bounded
/// handshake task (see [`MAX_CONCURRENT_HANDSHAKES`]) that performs the rustls
/// handshake and forwards the finished stream over a channel to `accept`. This
/// means a flood of silent or stalled clients cannot serialize the accept loop:
/// a client that opens TCP but never completes (or even starts) the handshake
/// occupies only its own handshake task (bounded by `handshake_timeout`), never
/// head-of-line-blocking the acceptance of the next connection. A failed
/// handshake (a plaintext or malformed client, an unsupported cipher, a dropped
/// connection) is logged at debug and dropped; it never affects the acceptor.
pub struct TlsListener {
    /// Completed `(stream, peer)` pairs from the background handshake tasks.
    rx: tokio::sync::mpsc::Receiver<(
        tokio_rustls::server::TlsStream<tokio::net::TcpStream>,
        std::net::SocketAddr,
    )>,
    /// The bound address, captured before `tcp` moved into the acceptor task.
    local_addr: std::net::SocketAddr,
    /// Shutdown signal; also used to park `accept` once the acceptor has ended
    /// (channel closed) so axum's own graceful-shutdown future drives teardown.
    shutdown: tokio_util::sync::CancellationToken,
}

impl TlsListener {
    /// Wrap `tcp` so accepted connections are TLS-terminated with `config`,
    /// bounding each handshake by `handshake_timeout` (a stalled or silent
    /// client is dropped rather than starving other clients). `shutdown` ties
    /// the background acceptor task's lifetime to server shutdown.
    ///
    /// # Panics
    ///
    /// Panics if `tcp.local_addr()` fails — the listener is already bound, so
    /// this is not expected in practice.
    #[must_use]
    pub fn new(
        tcp: tokio::net::TcpListener,
        config: Arc<rustls::ServerConfig>,
        handshake_timeout: std::time::Duration,
        shutdown: tokio_util::sync::CancellationToken,
    ) -> Self {
        let local_addr = tcp
            .local_addr()
            .expect("bound TLS listener must have a local address");
        let acceptor = tokio_rustls::TlsAcceptor::from(config);
        let (tx, rx) = tokio::sync::mpsc::channel(READY_CONN_CHANNEL_CAPACITY);
        let semaphore = Arc::new(tokio::sync::Semaphore::new(MAX_CONCURRENT_HANDSHAKES));
        let acceptor_shutdown = shutdown.clone();

        tokio::spawn(async move {
            run_acceptor(
                tcp,
                acceptor,
                handshake_timeout,
                semaphore,
                tx,
                acceptor_shutdown,
            )
            .await;
        });

        Self {
            rx,
            local_addr,
            shutdown,
        }
    }
}

/// Background accept loop: drain `tcp`, and for each connection spawn a bounded
/// handshake task that forwards the completed TLS stream over `tx`. Breaks (and
/// drops `tx`) on `shutdown`.
async fn run_acceptor(
    tcp: tokio::net::TcpListener,
    acceptor: tokio_rustls::TlsAcceptor,
    handshake_timeout: std::time::Duration,
    semaphore: Arc<tokio::sync::Semaphore>,
    tx: tokio::sync::mpsc::Sender<(
        tokio_rustls::server::TlsStream<tokio::net::TcpStream>,
        std::net::SocketAddr,
    )>,
    shutdown: tokio_util::sync::CancellationToken,
) {
    loop {
        let (stream, peer) = tokio::select! {
            () = shutdown.cancelled() => break,
            result = tcp.accept() => match result {
                Ok(pair) => pair,
                Err(e) => {
                    // Never break the loop on a per-connection error — that
                    // would tear down the whole listener. Retry transient
                    // per-connection errors immediately; back off briefly on
                    // anything else so we do not spin (e.g. on the process's
                    // open-file limit).
                    if is_transient_connection_error(&e) {
                        continue;
                    }
                    tracing::error!(error = %e, "TLS listener accept error");
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    continue;
                }
            },
        };

        // Acquire a permit BEFORE spawning so in-flight handshakes are bounded:
        // once `MAX_CONCURRENT_HANDSHAKES` are running, this parks (applying
        // backpressure to accept) until one finishes. The only error is a closed
        // semaphore, which we never close, so the `else` is unreachable in
        // practice; stop the loop defensively if it ever happens.
        let Ok(permit) = Arc::clone(&semaphore).acquire_owned().await else {
            break;
        };
        let acceptor = acceptor.clone();
        let tx = tx.clone();
        tokio::spawn(async move {
            // The permit is released when this task ends (handshake done,
            // failed, or timed out), freeing a slot for the next connection.
            let _permit = permit;
            // Bound the handshake so a client that connects but never sends a
            // ClientHello (or stalls mid-handshake) releases its permit instead
            // of holding it for the process lifetime.
            match tokio::time::timeout(handshake_timeout, acceptor.accept(stream)).await {
                Ok(Ok(tls)) => {
                    // A closed receiver means the listener is gone; drop.
                    let _ = tx.send((tls, peer)).await;
                }
                Ok(Err(e)) => {
                    tracing::debug!(
                        peer = %peer,
                        error = %e,
                        "TLS handshake failed; dropping connection"
                    );
                }
                Err(_elapsed) => {
                    tracing::debug!(peer = %peer, "TLS handshake timed out");
                }
            }
        });
    }
}

/// Whether an accept error is a per-connection condition (the client went away
/// between the kernel accept and ours) rather than a listener-wide one. These
/// are retried immediately; other errors (e.g. `EMFILE`) get a short backoff.
/// Mirrors axum's own built-in `TcpListener` accept behavior.
fn is_transient_connection_error(e: &std::io::Error) -> bool {
    matches!(
        e.kind(),
        std::io::ErrorKind::ConnectionRefused
            | std::io::ErrorKind::ConnectionAborted
            | std::io::ErrorKind::ConnectionReset
    )
}

impl axum::serve::Listener for TlsListener {
    type Io = tokio_rustls::server::TlsStream<tokio::net::TcpStream>;
    type Addr = std::net::SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        if let Some(conn) = self.rx.recv().await {
            return conn;
        }
        // The acceptor task has ended and the channel is drained (shutdown, or
        // an unrecoverable acceptor exit). `Listener::accept` returns no
        // `Result` and axum loops on it, so we must neither panic nor busy-loop:
        // park here forever (awaiting the shutdown token, then a never-resolving
        // future) so axum's own graceful-shutdown path drives teardown instead
        // of us spinning.
        self.shutdown.cancelled().await;
        std::future::pending().await
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        Ok(self.local_addr)
    }
}

/// The outcome of inspecting a configured certificate + key pair, offline.
#[derive(Debug, Clone, Copy)]
pub struct LeafInspection {
    /// The leaf certificate's `notBefore`, as a UNIX timestamp (seconds).
    pub not_before_unix: i64,
    /// The leaf certificate's `notAfter`, as a UNIX timestamp (seconds).
    pub not_after_unix: i64,
}

impl LeafInspection {
    /// Whether the leaf certificate is not yet valid at `now_unix` — its
    /// `notBefore` lies in the future. Such a certificate fails every handshake
    /// until its validity window opens, so the runtime rejects it at startup and
    /// doctor grades it a failure, symmetric to [`Self::is_expired`].
    #[must_use]
    pub const fn is_not_yet_valid(&self, now_unix: i64) -> bool {
        now_unix < self.not_before_unix
    }

    /// Whole days from `now_unix` until `notAfter`. Negative once expired.
    ///
    /// Note: this truncates toward zero, so a certificate that expired less
    /// than 24h ago still reports `0` days. Callers grading expiry must use
    /// [`Self::is_expired`] for the pass/fail decision rather than relying on
    /// this day bucket.
    #[must_use]
    pub const fn days_until_expiry(&self, now_unix: i64) -> i64 {
        (self.not_after_unix - now_unix) / 86_400
    }

    /// Whether the leaf certificate has already expired at `now_unix`.
    ///
    /// Decided from the raw `notAfter` timestamp (not the truncated day
    /// bucket), so a certificate expired even seconds ago reports `true` —
    /// matching the runtime, which rejects such a pair at startup.
    #[must_use]
    pub const fn is_expired(&self, now_unix: i64) -> bool {
        self.not_after_unix <= now_unix
    }
}

/// Validate the configured certificate + key and report the leaf's expiry,
/// WITHOUT booting a server or touching the network. Used by `autumn doctor`.
///
/// This performs the same parsing and cert/key-match validation as
/// [`load_certified_key`] (so a broken pair is reported), but tolerates a LEAF
/// outside its validity window — already-expired OR not-yet-valid — leaving the
/// caller to decide how to grade it, by still returning the `notBefore`/
/// `notAfter` bounds. Non-leaf (intermediate) certificates are NOT tolerated:
/// an expired or not-yet-valid intermediate is rejected here (mirroring the
/// runtime), since clients reject the served chain during path validation.
///
/// `now_unix` is the current UNIX time; it is a parameter (rather than read
/// internally) so tests can pin "now" deterministically. It is used only to
/// lifetime-check the intermediates — the leaf's window is returned ungraded.
///
/// # Errors
///
/// Returns a [`TlsError`] for a missing/unreadable file, unparseable PEM, an
/// empty certificate file, a key that does not match the leaf certificate, or a
/// malformed / expired / not-yet-valid intermediate certificate.
pub fn inspect_leaf(
    cert_path: &Path,
    key_path: &Path,
    now_unix: i64,
) -> Result<LeafInspection, TlsError> {
    let chain = read_cert_chain(cert_path)?;
    let key = read_private_key(key_path)?;
    let (not_before, not_after) = leaf_validity_unix(cert_path, &chain[0])?;

    // Parse and lifetime-check every intermediate too, so doctor fails a
    // malformed — or expired / not-yet-valid — intermediate chain instead of
    // greenlighting one the runtime would boot but clients reject (rustls
    // validates only the leaf).
    validate_chain_certs(cert_path, &chain, now_unix)?;

    // Validate the key matches the leaf even though we do not need the key
    // material — a mismatched pair would fail every handshake at runtime, so
    // doctor should surface it too.
    let provider = crypto_provider();
    CertifiedKey::from_der(chain, key, &provider).map_err(|source| TlsError::InvalidKeyPair {
        cert: cert_path.to_path_buf(),
        key: key_path.to_path_buf(),
        source: Box::new(source),
    })?;

    Ok(LeafInspection {
        not_before_unix: not_before,
        not_after_unix: not_after,
    })
}

/// Build a rustls [`CertifiedKey`] from an in-memory PEM certificate chain and
/// private key, WITHOUT touching the filesystem.
///
/// Used by the ACME path (issue #1608) to hot-swap a freshly issued certificate
/// into a [`ReloadableCertResolver`] without a round-trip through disk. Like
/// [`load_certified_key`], `from_der` validates the key and checks it matches
/// the leaf; unlike it, this does not reject an expired leaf (the caller — the
/// renewal task — decides how to react to a stale cert, and the self-signed
/// placeholder it also loads is deliberately short-lived).
///
/// # Errors
///
/// Returns a human-readable message if the PEM cannot be parsed or the key does
/// not match the leaf certificate.
#[cfg(feature = "acme")]
pub fn certified_key_from_pem(
    chain_pem: &[u8],
    key_pem: &[u8],
    provider: &CryptoProvider,
) -> Result<Arc<CertifiedKey>, String> {
    let mut chain = Vec::new();
    for cert in CertificateDer::pem_slice_iter(chain_pem) {
        chain.push(cert.map_err(|e| format!("failed to parse ACME certificate PEM: {e}"))?);
    }
    if chain.is_empty() {
        return Err("ACME certificate chain contained no PEM CERTIFICATE blocks".to_owned());
    }
    let key = PrivateKeyDer::from_pem_slice(key_pem)
        .map_err(|e| format!("failed to parse ACME private key PEM: {e}"))?;
    let certified = CertifiedKey::from_der(chain, key, provider)
        .map_err(|e| format!("ACME private key does not match the issued certificate: {e}"))?;
    Ok(Arc::new(certified))
}

/// The leaf certificate's `notAfter` (UNIX seconds) from an in-memory PEM chain.
///
/// Used by the ACME renewal loop and health indicator to decide when a stored
/// certificate is due for renewal. Returns an error message if the PEM has no
/// certificate or the leaf cannot be parsed.
///
/// # Errors
///
/// Returns a human-readable message on a missing or unparseable leaf.
#[cfg(feature = "acme")]
pub fn leaf_not_after_from_pem(chain_pem: &[u8]) -> Result<i64, String> {
    let leaf = CertificateDer::pem_slice_iter(chain_pem)
        .next()
        .ok_or_else(|| "certificate chain contained no PEM CERTIFICATE blocks".to_owned())?
        .map_err(|e| format!("failed to parse certificate PEM: {e}"))?;
    leaf_validity_unix(Path::new("<memory>"), &leaf)
        .map(|(_, not_after)| not_after)
        .map_err(|e| e.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write as _;

    // Self-signed `CN=localhost`, valid until 2126. Test fixture only — the key
    // pair is intentionally public (mirrors `tests/integration/pg_tls.rs`).
    const CERT_PEM: &str = include_str!("../tests/fixtures/tls/localhost.cert.pem");
    const KEY_PEM: &str = include_str!("../tests/fixtures/tls/localhost.key.pem");
    // A second, unrelated self-signed key (does not match `CERT_PEM`).
    const MISMATCHED_KEY_PEM: &str = include_str!("../tests/fixtures/tls/other.key.pem");
    // Self-signed `CN=localhost` that expired in 2021.
    const EXPIRED_CERT_PEM: &str = include_str!("../tests/fixtures/tls/expired.cert.pem");
    const EXPIRED_KEY_PEM: &str = include_str!("../tests/fixtures/tls/expired.key.pem");

    // A well-formed PEM CERTIFICATE block whose body is valid base64 but NOT a
    // parseable X.509 DER (all-zero bytes). PEM-decodes fine, so it survives
    // `read_cert_chain`, but `X509Certificate::from_der` rejects it — standing in
    // for a malformed intermediate in a chain.
    const MALFORMED_CERT_PEM: &str = "\
-----BEGIN CERTIFICATE-----
AAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA
-----END CERTIFICATE-----
";

    fn write_temp(dir: &Path, name: &str, contents: &str) -> PathBuf {
        let path = dir.join(name);
        let mut f = std::fs::File::create(&path).unwrap();
        f.write_all(contents.as_bytes()).unwrap();
        path
    }

    fn now() -> i64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            .try_into()
            .unwrap()
    }

    #[test]
    fn load_valid_pair_succeeds() {
        let dir = tempfile::tempdir().unwrap();
        let cert = write_temp(dir.path(), "c.pem", CERT_PEM);
        let key = write_temp(dir.path(), "k.pem", KEY_PEM);
        let provider = crypto_provider();
        let ck = load_certified_key(&cert, &key, &provider, now()).expect("valid pair loads");
        assert!(!ck.cert.is_empty());
    }

    // Regression (#1603, Codex): rustls validates only the leaf, so a malformed
    // INTERMEDIATE would boot a listener serving a chain normal clients reject.
    // `load_certified_key` must parse every cert in the chain and fail fast,
    // naming the offending 1-based position.
    #[test]
    fn malformed_intermediate_in_chain_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        // [valid leaf, malformed intermediate].
        let chain_pem = format!("{CERT_PEM}\n{MALFORMED_CERT_PEM}");
        let cert = write_temp(dir.path(), "chain.pem", &chain_pem);
        let key = write_temp(dir.path(), "k.pem", KEY_PEM);
        let provider = crypto_provider();
        let err = load_certified_key(&cert, &key, &provider, now()).unwrap_err();
        assert!(
            matches!(err, TlsError::ParseChainCert { position: 2, .. }),
            "a malformed intermediate must fail fast naming its chain position, got {err:?}"
        );
        assert!(
            err.to_string().contains("#2"),
            "error must name the offending position, got: {err}"
        );
    }

    // A chain of [valid leaf, well-formed valid intermediate] still loads.
    // Chain-cert validation checks DER parseability and the validity window
    // (not chain-of-trust), so a second currently-valid certificate stands in
    // for a real intermediate here.
    #[test]
    fn valid_leaf_and_intermediate_chain_loads() {
        let dir = tempfile::tempdir().unwrap();
        // Both blocks are the far-future `CERT_PEM` fixture, so the intermediate
        // is within its validity window (an expired intermediate is now rejected).
        let chain_pem = format!("{CERT_PEM}\n{CERT_PEM}");
        let cert = write_temp(dir.path(), "chain.pem", &chain_pem);
        let key = write_temp(dir.path(), "k.pem", KEY_PEM);
        let provider = crypto_provider();
        let ck = load_certified_key(&cert, &key, &provider, now())
            .expect("a valid leaf + well-formed intermediate must load");
        assert_eq!(ck.cert.len(), 2, "both chain certificates are retained");
    }

    // Regression (#1603, Codex): an EXPIRED intermediate is rejected by clients
    // during path validation, so `validate_chain_certs` must fail fast at
    // load/inspection time, naming the 1-based position and that it expired.
    #[test]
    fn expired_intermediate_in_chain_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        // [valid leaf (far-future), expired intermediate (2021)].
        let chain_pem = format!("{CERT_PEM}\n{EXPIRED_CERT_PEM}");
        let path = write_temp(dir.path(), "chain.pem", &chain_pem);
        let chain = read_cert_chain(&path).unwrap();
        // "Now" is inside the leaf's window but well past the intermediate's
        // 2021 notAfter.
        let err = validate_chain_certs(&path, &chain, now()).unwrap_err();
        assert!(
            matches!(err, TlsError::ExpiredChainCert { position: 2, .. }),
            "an expired intermediate must fail fast at position 2, got {err:?}"
        );
        assert!(
            err.to_string().contains("#2"),
            "error must name the offending position, got: {err}"
        );
        assert!(
            err.to_string().contains("expired"),
            "error must report expiry, got: {err}"
        );
    }

    // Regression (#1603, Codex): a NOT-YET-VALID intermediate is likewise
    // rejected by clients, so it must fail fast at position 2. `now` is pinned
    // before the intermediate's notBefore; the leaf (`chain[0]`) is skipped by
    // `validate_chain_certs`, so only the intermediate is graded.
    #[test]
    fn not_yet_valid_intermediate_in_chain_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let leaf = write_temp(dir.path(), "leaf.pem", CERT_PEM);
        let leaf_key = write_temp(dir.path(), "leaf.key.pem", KEY_PEM);
        // The intermediate is a second copy of `CERT_PEM`; pin "now" one hour
        // before its notBefore so its validity window has not opened yet.
        let not_before = inspect_leaf(&leaf, &leaf_key, now())
            .unwrap()
            .not_before_unix;
        let chain_pem = format!("{CERT_PEM}\n{CERT_PEM}");
        let path = write_temp(dir.path(), "chain.pem", &chain_pem);
        let chain = read_cert_chain(&path).unwrap();
        let err = validate_chain_certs(&path, &chain, not_before - 3600).unwrap_err();
        assert!(
            matches!(err, TlsError::NotYetValidChainCert { position: 2, .. }),
            "a not-yet-valid intermediate must fail fast at position 2, got {err:?}"
        );
        assert!(
            err.to_string().contains("#2"),
            "error must name the offending position, got: {err}"
        );
        assert!(
            err.to_string().contains("not yet valid"),
            "error must report the not-yet-valid window, got: {err}"
        );
    }

    // A chain whose intermediate is currently within its validity window passes.
    #[test]
    fn valid_intermediate_in_chain_is_accepted() {
        let dir = tempfile::tempdir().unwrap();
        let chain_pem = format!("{CERT_PEM}\n{CERT_PEM}");
        let path = write_temp(dir.path(), "chain.pem", &chain_pem);
        let chain = read_cert_chain(&path).unwrap();
        validate_chain_certs(&path, &chain, now())
            .expect("a valid, in-window intermediate must be accepted");
    }

    #[test]
    fn missing_cert_file_names_the_path() {
        let dir = tempfile::tempdir().unwrap();
        let key = write_temp(dir.path(), "k.pem", KEY_PEM);
        let missing = dir.path().join("does-not-exist.pem");
        let provider = crypto_provider();
        let err = load_certified_key(&missing, &key, &provider, now()).unwrap_err();
        assert!(matches!(err, TlsError::ReadCert { .. }));
        assert!(err.to_string().contains("does-not-exist.pem"));
    }

    #[test]
    fn unparseable_cert_pem_is_reported() {
        let dir = tempfile::tempdir().unwrap();
        let cert = write_temp(dir.path(), "c.pem", "not a pem file");
        let key = write_temp(dir.path(), "k.pem", KEY_PEM);
        let provider = crypto_provider();
        let err = load_certified_key(&cert, &key, &provider, now()).unwrap_err();
        // An input with no PEM blocks yields "no certificates"; a malformed
        // block yields a parse error. Either is an actionable, path-named error.
        assert!(matches!(
            err,
            TlsError::NoCertificates { .. } | TlsError::ParseCert { .. }
        ));
    }

    #[test]
    fn mismatched_key_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let cert = write_temp(dir.path(), "c.pem", CERT_PEM);
        let key = write_temp(dir.path(), "k.pem", MISMATCHED_KEY_PEM);
        let provider = crypto_provider();
        let err = load_certified_key(&cert, &key, &provider, now()).unwrap_err();
        assert!(
            matches!(err, TlsError::InvalidKeyPair { .. }),
            "got {err:?}"
        );
    }

    #[test]
    fn expired_leaf_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let cert = write_temp(dir.path(), "c.pem", EXPIRED_CERT_PEM);
        let key = write_temp(dir.path(), "k.pem", EXPIRED_KEY_PEM);
        let provider = crypto_provider();
        let err = load_certified_key(&cert, &key, &provider, now()).unwrap_err();
        assert!(matches!(err, TlsError::Expired { .. }), "got {err:?}");
    }

    #[test]
    fn not_yet_valid_leaf_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let cert = write_temp(dir.path(), "c.pem", CERT_PEM);
        let key = write_temp(dir.path(), "k.pem", KEY_PEM);
        let provider = crypto_provider();

        // Pin "now" to one hour BEFORE the fixture's notBefore (equivalent to a
        // cert whose notBefore is now + 3600): the leaf is not yet valid, so
        // load must fail fast — symmetric to the expired-leaf rejection.
        let not_before = inspect_leaf(&cert, &key, now()).unwrap().not_before_unix;
        let err = load_certified_key(&cert, &key, &provider, not_before - 3600).unwrap_err();
        assert!(matches!(err, TlsError::NotYetValid { .. }), "got {err:?}");

        // A currently-valid cert is unaffected: one second into the validity
        // window (and well before notAfter) the same pair loads cleanly.
        load_certified_key(&cert, &key, &provider, not_before + 1)
            .expect("cert loads once notBefore has passed");
    }

    #[test]
    fn inspect_reports_not_yet_valid_window() {
        let dir = tempfile::tempdir().unwrap();
        let cert = write_temp(dir.path(), "c.pem", CERT_PEM);
        let key = write_temp(dir.path(), "k.pem", KEY_PEM);
        // inspect_leaf tolerates a not-yet-valid leaf (doctor grades it) but
        // returns notBefore so the caller can see the window has not opened.
        let inspection = inspect_leaf(&cert, &key, now()).expect("pair inspects");
        assert!(inspection.is_not_yet_valid(inspection.not_before_unix - 1));
        assert!(!inspection.is_not_yet_valid(inspection.not_before_unix));
        assert!(!inspection.is_not_yet_valid(inspection.not_after_unix));
    }

    #[test]
    fn inspect_reports_future_expiry() {
        let dir = tempfile::tempdir().unwrap();
        let cert = write_temp(dir.path(), "c.pem", CERT_PEM);
        let key = write_temp(dir.path(), "k.pem", KEY_PEM);
        let inspection = inspect_leaf(&cert, &key, now()).expect("valid pair inspects");
        assert!(
            inspection.days_until_expiry(now()) > 30,
            "fixture should be valid far into the future"
        );
    }

    #[test]
    fn inspect_still_reports_expiry_for_expired_cert() {
        let dir = tempfile::tempdir().unwrap();
        let cert = write_temp(dir.path(), "c.pem", EXPIRED_CERT_PEM);
        let key = write_temp(dir.path(), "k.pem", EXPIRED_KEY_PEM);
        // inspect_leaf tolerates expiry (doctor grades it) but still returns
        // the notAfter so the caller can see it is in the past.
        let inspection = inspect_leaf(&cert, &key, now()).expect("expired pair still inspects");
        assert!(inspection.days_until_expiry(now()) < 0);
    }

    #[test]
    fn reloadable_resolver_swaps_certificate() {
        let dir = tempfile::tempdir().unwrap();
        let cert = write_temp(dir.path(), "c.pem", CERT_PEM);
        let key = write_temp(dir.path(), "k.pem", KEY_PEM);
        let provider = crypto_provider();
        let first = load_certified_key(&cert, &key, &provider, now()).unwrap();
        let resolver = ReloadableCertResolver::new(Arc::clone(&first));
        assert!(Arc::ptr_eq(&resolver.current(), &first));

        let second = load_certified_key(&cert, &key, &provider, now()).unwrap();
        resolver.store(Arc::clone(&second));
        assert!(Arc::ptr_eq(&resolver.current(), &second));
    }
}
