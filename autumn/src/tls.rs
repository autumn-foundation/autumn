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

/// Number of days before `notAfter` at which `autumn doctor` starts warning
/// about an approaching certificate expiry.
pub const NEAR_EXPIRY_WARN_DAYS: i64 = 30;

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
        source: rustls::Error,
    },
    /// The leaf certificate DER could not be parsed for expiry inspection.
    #[error("failed to parse the leaf certificate in `{path}` for expiry inspection: {detail}")]
    ParseLeaf {
        /// Certificate path.
        path: PathBuf,
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

/// The `notAfter` of the leaf certificate, as a UNIX timestamp (seconds).
///
/// Parses just enough of the DER to read the validity window; a parse failure
/// is surfaced rather than silently ignored so a corrupt certificate is caught
/// at load time.
fn leaf_not_after_unix(cert_path: &Path, leaf: &CertificateDer<'_>) -> Result<i64, TlsError> {
    use x509_parser::prelude::FromDer as _;

    let (_, parsed) =
        x509_parser::certificate::X509Certificate::from_der(leaf.as_ref()).map_err(|e| {
            TlsError::ParseLeaf {
                path: cert_path.to_path_buf(),
                detail: e.to_string(),
            }
        })?;
    Ok(parsed.validity().not_after.timestamp())
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

    // Reject an already-expired leaf before we even build the key: serving an
    // expired certificate fails every client handshake, so refuse at startup
    // with a clear message rather than booting into a broken listener.
    let not_after = leaf_not_after_unix(cert_path, &chain[0])?;
    if not_after < now_unix {
        return Err(TlsError::Expired {
            path: cert_path.to_path_buf(),
            not_after: render_unix(not_after),
            not_after_unix: not_after,
        });
    }

    // `from_der` loads the key with the crypto provider (rejecting an invalid
    // key) and compares the key's SubjectPublicKeyInfo against the leaf
    // certificate's, so a cert/key mismatch is caught here.
    let certified = CertifiedKey::from_der(chain, key, provider).map_err(|source| {
        TlsError::InvalidKeyPair {
            cert: cert_path.to_path_buf(),
            key: key_path.to_path_buf(),
            source,
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

/// A TLS-terminating [`axum::serve::Listener`] wrapping a
/// [`tokio::net::TcpListener`].
///
/// [`accept`](axum::serve::Listener::accept) accepts a TCP connection and
/// completes the rustls handshake before yielding a decrypted
/// [`TlsStream`](tokio_rustls::server::TlsStream) plus the peer's
/// [`SocketAddr`](std::net::SocketAddr). Because the peer address is a real TCP
/// `SocketAddr`, the rest of the serve stack — connect-info, trusted-proxy
/// resolution, graceful shutdown, SSE/WebSocket streaming — is identical to the
/// plain-TCP path; the only difference is the handshake performed here.
///
/// A failed handshake (a plaintext or malformed client, an unsupported cipher,
/// a dropped connection) is logged at debug and skipped — it must never take
/// down the accept loop. Note the tradeoff: the handshake runs inline with
/// `accept`, so one slow handshake briefly head-of-line-blocks new accepts.
pub struct TlsListener {
    tcp: tokio::net::TcpListener,
    acceptor: tokio_rustls::TlsAcceptor,
}

impl TlsListener {
    /// Wrap `tcp` so accepted connections are TLS-terminated with `config`.
    #[must_use]
    pub fn new(tcp: tokio::net::TcpListener, config: Arc<rustls::ServerConfig>) -> Self {
        Self {
            tcp,
            acceptor: tokio_rustls::TlsAcceptor::from(config),
        }
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
        loop {
            let (stream, peer) = match self.tcp.accept().await {
                Ok(pair) => pair,
                Err(e) => {
                    // Never return an error from here — that terminates the
                    // whole serve loop. Retry transient per-connection errors
                    // immediately; back off briefly on anything else so we do
                    // not spin (e.g. on the process's open-file limit).
                    if is_transient_connection_error(&e) {
                        continue;
                    }
                    tracing::error!(error = %e, "TLS listener accept error");
                    tokio::time::sleep(std::time::Duration::from_secs(1)).await;
                    continue;
                }
            };
            match self.acceptor.accept(stream).await {
                Ok(tls) => return (tls, peer),
                Err(e) => {
                    tracing::debug!(
                        peer = %peer,
                        error = %e,
                        "TLS handshake failed; dropping connection"
                    );
                }
            }
        }
    }

    fn local_addr(&self) -> std::io::Result<Self::Addr> {
        self.tcp.local_addr()
    }
}

/// The outcome of inspecting a configured certificate + key pair, offline.
#[derive(Debug, Clone, Copy)]
pub struct LeafInspection {
    /// The leaf certificate's `notAfter`, as a UNIX timestamp (seconds).
    pub not_after_unix: i64,
}

impl LeafInspection {
    /// Whole days from `now_unix` until `notAfter`. Negative once expired.
    #[must_use]
    pub const fn days_until_expiry(&self, now_unix: i64) -> i64 {
        (self.not_after_unix - now_unix) / 86_400
    }
}

/// Validate the configured certificate + key and report the leaf's expiry,
/// WITHOUT booting a server or touching the network. Used by `autumn doctor`.
///
/// This performs the same parsing and cert/key-match validation as
/// [`load_certified_key`] (so a broken pair is reported), but tolerates an
/// already-expired certificate — the caller decides how to grade expiry — by
/// still returning the `notAfter`.
///
/// # Errors
///
/// Returns a [`TlsError`] for a missing/unreadable file, unparseable PEM, an
/// empty certificate file, or a key that does not match the leaf certificate.
pub fn inspect_leaf(cert_path: &Path, key_path: &Path) -> Result<LeafInspection, TlsError> {
    let chain = read_cert_chain(cert_path)?;
    let key = read_private_key(key_path)?;
    let not_after = leaf_not_after_unix(cert_path, &chain[0])?;

    // Validate the key matches the leaf even though we do not need the key
    // material — a mismatched pair would fail every handshake at runtime, so
    // doctor should surface it too.
    let provider = crypto_provider();
    CertifiedKey::from_der(chain, key, &provider).map_err(|source| TlsError::InvalidKeyPair {
        cert: cert_path.to_path_buf(),
        key: key_path.to_path_buf(),
        source,
    })?;

    Ok(LeafInspection {
        not_after_unix: not_after,
    })
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
    fn inspect_reports_future_expiry() {
        let dir = tempfile::tempdir().unwrap();
        let cert = write_temp(dir.path(), "c.pem", CERT_PEM);
        let key = write_temp(dir.path(), "k.pem", KEY_PEM);
        let inspection = inspect_leaf(&cert, &key).expect("valid pair inspects");
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
        let inspection = inspect_leaf(&cert, &key).expect("expired pair still inspects");
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
