//! Mutual-TLS client-certificate verification (issue #1640).
//!
//! Builds on #1603's listener: when `[server.tls.client_auth]` names a PEM
//! bundle of client CAs, the handshake requests — and verifies — a client
//! certificate, and the verified identity flows to handlers and policies.
//!
//! (`lib.rs` carries an outer doc comment on `pub mod tls`, which is merged
//! with this block, so every link below is fully qualified — a bare item name
//! would resolve in the crate root instead of here.)
//!
//! **The trust store**
//!
//! 1. **Fail-fast loading** —
//!    [`load_client_roots`](crate::tls::client_auth::load_client_roots) and
//!    [`load_crls`](crate::tls::client_auth::load_crls) reject a missing,
//!    unparseable, or empty bundle, naming the path.
//! 2. **A swappable verifier** —
//!    [`ReloadableClientVerifier`](crate::tls::client_auth::ReloadableClientVerifier)
//!    holds the current `rustls` verifier behind an `RwLock`, so a CA rotation
//!    swaps atomically without dropping the listener or established
//!    connections.
//! 3. **Rotation** —
//!    [`ClientTrustReloader`](crate::tls::client_auth::ClientTrustReloader)
//!    polls the bundle and CRL mtimes, mirroring
//!    [`CertReloader`](crate::tls::CertReloader).
//!
//! **The request surface**
//!
//! 4. **Identity** —
//!    [`ClientIdentity`](crate::tls::client_auth::ClientIdentity) is the parsed
//!    subject DN, issuer DN, SANs, fingerprint and serial.
//! 5. **Plumbing** —
//!    [`ClientIdentityLayer`](crate::tls::client_auth::ClientIdentityLayer)
//!    turns the listener's
//!    [`TlsConnectInfo`](crate::tls::TlsConnectInfo) into the per-request
//!    surface: a re-stamped `ConnectInfo<SocketAddr>`, the identity as a
//!    request extension, and the ambient scope
//!    [`current_client_identity`](crate::tls::client_auth::current_client_identity)
//!    (and so `PolicyContext`) reads.
//! 6. **Extractors** —
//!    [`ClientCert`](crate::tls::client_auth::ClientCert) and
//!    [`OptionalClientCert`](crate::tls::client_auth::OptionalClientCert).
//! 7. **Per-route enforcement** —
//!    [`RequireClientCertLayer`](crate::tls::client_auth::RequireClientCertLayer),
//!    which the framework applies with the configured `required_paths`.
//!
//! **Operations**
//!
//! 8. **Diagnostics** —
//!    [`RejectionReason`](crate::tls::client_auth::RejectionReason) classifies a
//!    rejected handshake so the operator sees *why*, rate-limited, while the
//!    client sees only the standard TLS alert.
//! 9. **Offline inspection** —
//!    [`inspect_client_ca_bundle`](crate::tls::client_auth::inspect_client_ca_bundle)
//!    and [`inspect_crl`](crate::tls::client_auth::inspect_crl) back the
//!    `autumn doctor` checks.
//!
//! Revocation is a static CRL file only; OCSP is out of scope.

// autumn-determinism-gate: production code in this module must read time and
// mint identifiers through the framework's injected seams (ClockSource /
// Entropy), never `Instant::now()` / `Utc::now()` / `SystemTime::now()` /
// `Uuid::new_v4()` directly. See CONTRIBUTING.md "Determinism seam gate"
// (issue #1797). Certificate validity and the rejection-log rate limiter are
// judged against real wall time by design, and read it through
// `super::now_unix`, which carries the module's one documented
// #[allow(clippy::disallowed_methods, reason = "…")].
#![cfg_attr(not(test), deny(clippy::disallowed_methods))]

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicI64, AtomicU64, Ordering};
use std::sync::{Arc, RwLock};

use rustls::RootCertStore;
use rustls::crypto::CryptoProvider;
use rustls::server::WebPkiClientVerifier;
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls_pki_types::pem::PemObject as _;
use rustls_pki_types::{CertificateDer, CertificateRevocationListDer, UnixTime};

use super::TlsError;
use crate::config::ClientAuthMode;

/// Metric counting handshakes rejected by client-certificate verification,
/// labelled `reason`.
///
/// Registered through [`crate::metrics::counter`], so the actuator's Prometheus
/// exporter renders it alongside every other application metric. No `autumn_`
/// prefix: that namespace is reserved for the built-ins the actuator
/// hand-renders from its own snapshot, and `metrics::counter` returns an inert
/// handle for a name in it.
pub const REJECTED_METRIC: &str = "tls_client_auth_rejected_total";

/// Minimum seconds between two operator log lines for the same rejection
/// reason. Rejections are attacker-triggerable, so the log is rate limited
/// while the counter above stays exact.
const LOG_INTERVAL_SECS: i64 = 1;

// ── Identity ────────────────────────────────────────────────────────────────

/// The verified identity of a peer that presented a client certificate.
///
/// Constructed only from a certificate rustls has already path-validated
/// against the configured trust store, so every field describes a *verified*
/// peer, never a claim.
///
/// Minted only by [`from_der`](Self::from_der) from a certificate rustls has
/// already verified, hence `#[non_exhaustive]`: an identity that did not come
/// from a handshake is not one. Tests build one with
/// [`new_for_test`](Self::new_for_test).
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct ClientIdentity {
    /// Subject distinguished name, rendered with the attributes in
    /// **certificate (DER) order** — e.g. `O=Acme, OU=Services, CN=svc-orders`,
    /// not the reversed order `openssl -nameopt rfc2253` prints.
    ///
    /// For display and logging. Attribute values are **not** escaped, so a
    /// value containing `,` or `=` is ambiguous once rendered: never parse a
    /// field back out of this string. Match on
    /// [`common_name`](Self::common_name) or [`has_san`](Self::has_san)
    /// instead, both of which read the parsed certificate.
    pub subject: String,
    /// Distinguished name of the issuing CA, with the same ordering and
    /// escaping caveats as [`subject`](Self::subject).
    pub issuer: String,
    /// Subject alternative names, each prefixed by its kind: `DNS:`, `URI:`,
    /// `IP:`, or `email:`. Other SAN kinds are omitted.
    pub sans: Vec<String>,
    /// Lowercase hex SHA-256 of the certificate DER, prefixed `sha256:`.
    pub fingerprint: String,
    /// Certificate serial number, uppercase hex.
    pub serial: String,
    /// `notAfter` as a UNIX timestamp (seconds).
    pub not_after_unix: i64,
    /// The subject's common name (`CN`), read from the parsed certificate.
    ///
    /// Private, and exposed through [`common_name`](Self::common_name): it is
    /// derived from the certificate rather than supplied, so a caller cannot
    /// construct an identity whose `CN` disagrees with its `subject`.
    common_name: Option<String>,
}

impl ClientIdentity {
    /// Parse a verified peer certificate into an identity.
    ///
    /// # Errors
    ///
    /// Returns [`TlsError::ParsePeerCert`] if the DER does not parse. rustls has
    /// already validated the chain by this point, so a failure here means a
    /// certificate that webpki accepts and `x509-parser` does not.
    pub fn from_der(der: &CertificateDer<'_>) -> Result<Self, TlsError> {
        use x509_parser::prelude::FromDer as _;

        let (_, cert) =
            x509_parser::certificate::X509Certificate::from_der(der.as_ref()).map_err(|e| {
                TlsError::ParsePeerCert {
                    detail: e.to_string(),
                }
            })?;

        Ok(Self {
            subject: cert.subject().to_string(),
            issuer: cert.issuer().to_string(),
            sans: collect_sans(&cert),
            fingerprint: sha256_fingerprint(der.as_ref()),
            serial: cert.raw_serial_as_string().replace(':', ""),
            not_after_unix: cert.validity().not_after.timestamp(),
            // From the parsed name, never from the rendered `subject`. The
            // parser does not escape attribute values, so an enrolment service
            // that copies `O`/`OU` from a CSR would otherwise let a caller
            // smuggle `CN=svc-payments` into another attribute and have a
            // string-splitting reader believe it.
            //
            // The LAST `CN`, matching how a CA names the end entity when a
            // subject carries several.
            common_name: cert
                .subject()
                .iter_common_name()
                .filter_map(|cn| cn.as_str().ok())
                .last()
                .map(str::to_owned),
        })
    }

    /// Whether `san` is one of the certificate's subject alternative names.
    /// Compare with the kind prefix, e.g. `id.has_san("DNS:svc-orders.internal")`.
    ///
    /// `DNS:` names compare ASCII-case-insensitively, because DNS names are.
    /// Every other kind (`URI:`, `IP:`, `email:`) compares exactly: a SPIFFE
    /// path and a mailbox local-part are case-sensitive.
    #[must_use]
    pub fn has_san(&self, san: &str) -> bool {
        if let Some(host) = san.strip_prefix("DNS:") {
            return self.sans.iter().any(|s| {
                s.strip_prefix("DNS:")
                    .is_some_and(|have| have.eq_ignore_ascii_case(host))
            });
        }
        self.sans.iter().any(|s| s == san)
    }

    /// The subject's common name (`CN`), when present.
    ///
    /// Read from the parsed certificate, so it is safe to authorize on — unlike
    /// scanning [`subject`](Self::subject), which cannot tell an attribute
    /// separator from a comma inside an attribute value.
    #[must_use]
    pub fn common_name(&self) -> Option<&str> {
        self.common_name.as_deref()
    }

    /// Build an identity by hand, for a test that exercises a policy or handler
    /// without a TLS handshake.
    ///
    /// `subject` is rendered from `common_name`, so the two cannot disagree the
    /// way a free-form literal could. Pair it with
    /// [`with_client_identity`] to establish the ambient scope a
    /// [`PolicyContext`](crate::authorization::PolicyContext) reads.
    #[must_use]
    pub fn new_for_test(common_name: &str, sans: Vec<String>) -> Self {
        Self {
            subject: format!("CN={common_name}"),
            issuer: "CN=test".to_owned(),
            sans,
            fingerprint: "sha256:test".to_owned(),
            serial: "01".to_owned(),
            not_after_unix: i64::MAX,
            common_name: Some(common_name.to_owned()),
        }
    }
}

/// Every supported SAN in a certificate, prefixed by kind.
fn collect_sans(cert: &x509_parser::certificate::X509Certificate<'_>) -> Vec<String> {
    use x509_parser::extensions::{GeneralName, ParsedExtension};

    let mut out = Vec::new();
    for ext in cert.extensions() {
        let ParsedExtension::SubjectAlternativeName(san) = ext.parsed_extension() else {
            continue;
        };
        for name in &san.general_names {
            match name {
                GeneralName::DNSName(v) => out.push(format!("DNS:{v}")),
                GeneralName::URI(v) => out.push(format!("URI:{v}")),
                GeneralName::RFC822Name(v) => out.push(format!("email:{v}")),
                GeneralName::IPAddress(bytes) => {
                    if let Some(ip) = render_ip(bytes) {
                        out.push(format!("IP:{ip}"));
                    }
                }
                // Directory, registered-ID and othername SANs have no
                // unambiguous text form worth matching a policy on.
                _ => {}
            }
        }
    }
    out
}

/// Render a SAN `iPAddress` octet string as v4 or v6 text; `None` for any other
/// length (which is not a valid `iPAddress`).
fn render_ip(bytes: &[u8]) -> Option<String> {
    match bytes.len() {
        4 => {
            let octets: [u8; 4] = bytes.try_into().ok()?;
            Some(std::net::Ipv4Addr::from(octets).to_string())
        }
        16 => {
            let octets: [u8; 16] = bytes.try_into().ok()?;
            Some(std::net::Ipv6Addr::from(octets).to_string())
        }
        _ => None,
    }
}

/// Lowercase hex SHA-256 of `der`, prefixed `sha256:`.
fn sha256_fingerprint(der: &[u8]) -> String {
    use sha2::{Digest as _, Sha256};

    let digest = Sha256::digest(der);
    let mut out = String::with_capacity(7 + digest.len() * 2);
    out.push_str("sha256:");
    for byte in digest {
        use std::fmt::Write as _;
        // Writing to a String is infallible.
        let _ = write!(out, "{byte:02x}");
    }
    out
}

// ── Trust store loading ─────────────────────────────────────────────────────

/// Read and PEM-decode the client-CA bundle at `path` into a rustls root store.
///
/// Fails fast — naming the path — on a missing/unreadable file, unparseable
/// PEM, a bundle with no CERTIFICATE block, or a certificate rustls refuses as
/// a trust anchor.
///
/// # Errors
///
/// Returns a [`TlsError`] describing the first problem encountered.
pub fn load_client_roots(path: &Path) -> Result<RootCertStore, TlsError> {
    let mut roots = RootCertStore::empty();
    let mut count = 0usize;
    for cert in CertificateDer::pem_file_iter(path).map_err(|source| map_pem_err(path, source))? {
        let cert = cert.map_err(|source| map_pem_err(path, source))?;
        count += 1;
        roots
            .add(cert)
            .map_err(|source| TlsError::InvalidClientCa {
                path: path.to_path_buf(),
                position: count,
                source: Box::new(source),
            })?;
    }
    if count == 0 {
        return Err(TlsError::NoClientCas {
            path: path.to_path_buf(),
        });
    }
    Ok(roots)
}

/// Read and PEM-decode the certificate revocation list at `path`.
///
/// Fails fast — naming the path — on a missing/unreadable file, unparseable
/// PEM, or a file with no `X509 CRL` block.
///
/// # Errors
///
/// Returns a [`TlsError`] describing the first problem encountered.
pub fn load_crls(path: &Path) -> Result<Vec<CertificateRevocationListDer<'static>>, TlsError> {
    let mut crls = Vec::new();
    for crl in CertificateRevocationListDer::pem_file_iter(path)
        .map_err(|source| map_crl_pem_err(path, source))?
    {
        crls.push(crl.map_err(|source| map_crl_pem_err(path, source))?);
    }
    if crls.is_empty() {
        return Err(TlsError::NoCrls {
            path: path.to_path_buf(),
        });
    }
    Ok(crls)
}

/// Map a PEM error over the CA bundle, distinguishing "file missing" from
/// "bytes are not PEM" so the operator knows which to fix.
fn map_pem_err(path: &Path, source: rustls_pki_types::pem::Error) -> TlsError {
    match source {
        rustls_pki_types::pem::Error::Io(source) => TlsError::ReadClientCa {
            path: path.to_path_buf(),
            source,
        },
        other => TlsError::ParseClientCa {
            path: path.to_path_buf(),
            source: other,
        },
    }
}

/// [`map_pem_err`] for the CRL file.
fn map_crl_pem_err(path: &Path, source: rustls_pki_types::pem::Error) -> TlsError {
    match source {
        rustls_pki_types::pem::Error::Io(source) => TlsError::ReadCrl {
            path: path.to_path_buf(),
            source,
        },
        other => TlsError::ParseCrl {
            path: path.to_path_buf(),
            source: other,
        },
    }
}

/// Build the rustls client-certificate verifier for `mode` over `roots`.
///
/// Revocation is checked for the presented client certificate only
/// (`only_check_end_entity_revocation`): a single CRL from the issuing CA
/// cannot speak for intermediates, and chain-depth checking would reject every
/// certificate under an intermediate as "unknown status". CRL `nextUpdate` is
/// deliberately NOT enforced — a stale CRL keeps revoking the certificates it
/// lists (fail-closed) instead of failing every handshake; `autumn doctor`
/// grades the staleness instead.
///
/// # Errors
///
/// Returns [`TlsError::BuildClientVerifier`] if rustls rejects the trust
/// anchors or the CRLs.
pub fn build_client_verifier(
    roots: RootCertStore,
    crls: Vec<CertificateRevocationListDer<'static>>,
    mode: ClientAuthMode,
    provider: Arc<CryptoProvider>,
) -> Result<Arc<dyn ClientCertVerifier>, TlsError> {
    let mut builder = WebPkiClientVerifier::builder_with_provider(Arc::new(roots), provider);
    if !crls.is_empty() {
        builder = builder.with_crls(crls).only_check_end_entity_revocation();
    }
    if !mode.mandates_certificate() {
        builder = builder.allow_unauthenticated();
    }
    builder
        .build()
        .map_err(|source| TlsError::BuildClientVerifier { source })
}

// ── Reloadable verifier ─────────────────────────────────────────────────────

/// A [`ClientCertVerifier`] whose trust store can be swapped at runtime.
///
/// Every handshake takes a short read lock to clone the current verifier; a
/// rotation swaps in a new one under a brief write lock. Readers never block
/// each other, and a swap never interrupts an in-flight handshake or an
/// established connection — rustls verifies once, at handshake time.
#[derive(Debug)]
pub struct ReloadableClientVerifier {
    inner: RwLock<Arc<dyn ClientCertVerifier>>,
    mode: ClientAuthMode,
}

impl ReloadableClientVerifier {
    /// Wrap `initial`, remembering the configured `mode`.
    #[must_use]
    pub fn new(initial: Arc<dyn ClientCertVerifier>, mode: ClientAuthMode) -> Self {
        Self {
            inner: RwLock::new(initial),
            mode,
        }
    }

    /// Swap in a freshly built verifier. Handshakes started after this use it.
    pub fn store(&self, next: Arc<dyn ClientCertVerifier>) {
        // A poisoned lock means a previous holder panicked while swapping; the
        // stored value is still a valid verifier, so recover rather than
        // propagate a panic into every subsequent handshake.
        let mut guard = self
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        *guard = next;
    }

    /// The verifier currently in force.
    #[must_use]
    pub fn current(&self) -> Arc<dyn ClientCertVerifier> {
        Arc::clone(
            &self
                .inner
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        )
    }

    /// The configured listener mode.
    #[must_use]
    pub const fn mode(&self) -> ClientAuthMode {
        self.mode
    }
}

impl ClientCertVerifier for ReloadableClientVerifier {
    fn root_hint_subjects(&self) -> &[rustls::DistinguishedName] {
        // rustls takes this by reference, so it cannot be forwarded from a
        // temporarily-locked `Arc`. Sending no hint is valid (RFC 8446 §4.4.2.4
        // makes `certificate_authorities` optional): the client picks from the
        // certificates it holds. Hinting a rotating store is the wrong trade —
        // a borrowed snapshot would have to outlive the swap it exists to
        // describe.
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        // Deliberately does not count or log a rejection. rustls returns this
        // error out of the handshake, where `record_handshake_rejection` sees
        // it WITH the peer address — and also sees the rejections that never
        // reach a verifier at all (a `required` listener refusing a client that
        // presented nothing). Counting here too would double every reason but
        // that one.
        self.current()
            .verify_client_cert(end_entity, intermediates, now)
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.current().verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        self.current().verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.current().supported_verify_schemes()
    }

    fn requires_raw_public_keys(&self) -> bool {
        // Forwarded rather than left at the trait default: `new`/`store` accept
        // any `ClientCertVerifier`, so a caller wrapping a raw-public-key
        // verifier must not have it silently reported as X.509.
        self.current().requires_raw_public_keys()
    }

    fn offer_client_auth(&self) -> bool {
        self.mode.requests_certificate()
    }

    fn client_auth_mandatory(&self) -> bool {
        self.mode.mandates_certificate()
    }
}

// ── Rotation ────────────────────────────────────────────────────────────────

/// The background client-trust-store hot-reloader.
///
/// Polls the CA bundle and CRL mtimes and swaps the verifier when either
/// changes, so a CA rotation — ship old+new in one bundle, later drop old — is
/// picked up **without a restart and without dropping connections**.
///
/// Never breaks the listener: a failed reload logs an error, keeps the previous
/// trust store, and retries on the next tick. The baseline mtimes only advance
/// on a *successful* load, so a bundle observed mid-write is retried rather
/// than skipped.
pub struct ClientTrustReloader {
    verifier: Arc<ReloadableClientVerifier>,
    provider: Arc<CryptoProvider>,
    ca_bundle_path: PathBuf,
    crl_path: Option<PathBuf>,
    mode: ClientAuthMode,
    interval: std::time::Duration,
    /// Mtimes as of just *before* the load whose verifier is in force, so a
    /// rotation landing between the load and the first poll still reads as a
    /// change.
    baseline: (Option<std::time::SystemTime>, Option<std::time::SystemTime>),
}

impl ClientTrustReloader {
    /// Load the trust store, build the verifier that will enforce it, and build
    /// the reloader that watches it — deliberately one operation, in that
    /// order, so the baseline mtimes are stat'd *before* the load.
    ///
    /// # Errors
    ///
    /// Returns a [`TlsError`] if the bundle or CRL cannot be loaded.
    pub fn load(
        ca_bundle_path: PathBuf,
        crl_path: Option<PathBuf>,
        mode: ClientAuthMode,
        provider: Arc<CryptoProvider>,
        interval: std::time::Duration,
    ) -> Result<(Arc<ReloadableClientVerifier>, Self), TlsError> {
        let baseline = trust_mtimes(&ca_bundle_path, crl_path.as_deref());
        let built = build_from_paths(&ca_bundle_path, crl_path.as_deref(), mode, &provider)?;
        let verifier = Arc::new(ReloadableClientVerifier::new(built, mode));
        Ok((
            Arc::clone(&verifier),
            Self {
                verifier,
                provider,
                ca_bundle_path,
                crl_path,
                mode,
                interval,
                baseline,
            },
        ))
    }

    /// Poll until `shutdown`, swapping the verifier whenever the bundle or CRL
    /// changes on disk.
    pub async fn run(mut self, shutdown: tokio_util::sync::CancellationToken) {
        // A zero interval would spin the loop as fast as the runtime allows.
        // `app.rs` clamps before constructing this, but the type is public, so
        // enforce the invariant where it belongs — and `tokio::time::interval`
        // panics on a zero period, which is a poor way to learn it.
        let interval = if self.interval.is_zero() {
            std::time::Duration::from_secs(super::DEFAULT_RELOAD_INTERVAL_SECS)
        } else {
            self.interval
        };
        loop {
            tokio::select! {
                () = tokio::time::sleep(interval) => {}
                () = shutdown.cancelled() => break,
            }
            self.poll_once().await;
        }
    }

    /// One poll: reload and swap iff a watched file's mtime moved.
    ///
    /// The `stat` and the PEM read + parse both run on a blocking thread —
    /// they touch the filesystem and must not run on a tokio worker, and a
    /// large CA bundle makes the parse itself non-trivial. On a `JoinError`
    /// (the blocking pool shutting down) the tick is skipped and retried.
    async fn poll_once(&mut self) {
        let bundle = self.ca_bundle_path.clone();
        let crl = self.crl_path.clone();
        let seen = match tokio::task::spawn_blocking(move || trust_mtimes(&bundle, crl.as_deref()))
            .await
        {
            Ok(mtimes) => mtimes,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "mTLS trust reload: mtime read task failed; skipping tick"
                );
                return;
            }
        };
        if seen == self.baseline {
            return;
        }

        let bundle = self.ca_bundle_path.clone();
        let crl = self.crl_path.clone();
        let mode = self.mode;
        let provider = Arc::clone(&self.provider);
        let built = tokio::task::spawn_blocking(move || {
            build_from_paths(&bundle, crl.as_deref(), mode, &provider)
        })
        .await;
        let built = match built {
            Ok(result) => result,
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    "mTLS trust reload: load task failed; skipping tick"
                );
                return;
            }
        };

        match built {
            Ok(next) => {
                self.verifier.store(next);
                // Advance the baseline only on success, so a partial write is
                // retried on the next tick rather than skipped.
                self.baseline = seen;
                tracing::info!(
                    ca_bundle = %self.ca_bundle_path.display(),
                    "reloaded the mTLS client trust store"
                );
            }
            Err(e) => tracing::error!(
                ca_bundle = %self.ca_bundle_path.display(),
                error = %e,
                "failed to reload the mTLS client trust store; keeping the previous one"
            ),
        }
    }
}

/// Load both files and build a verifier from them.
fn build_from_paths(
    ca_bundle_path: &Path,
    crl_path: Option<&Path>,
    mode: ClientAuthMode,
    provider: &Arc<CryptoProvider>,
) -> Result<Arc<dyn ClientCertVerifier>, TlsError> {
    let roots = load_client_roots(ca_bundle_path)?;
    let crls = match crl_path {
        Some(path) => load_crls(path)?,
        None => Vec::new(),
    };
    build_client_verifier(roots, crls, mode, Arc::clone(provider))
}

/// Modification times of the bundle and (optional) CRL, `None` for a file that
/// could not be stat'd.
fn trust_mtimes(
    ca_bundle: &Path,
    crl: Option<&Path>,
) -> (Option<std::time::SystemTime>, Option<std::time::SystemTime>) {
    let mtime = |p: &Path| std::fs::metadata(p).and_then(|m| m.modified()).ok();
    (mtime(ca_bundle), crl.and_then(mtime))
}

// ── Diagnostics ─────────────────────────────────────────────────────────────

/// Why a handshake was rejected by client-certificate verification.
///
/// Operator-side only: the client always sees the standard TLS alert, never
/// this reason.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum RejectionReason {
    /// No certificate was presented on a `required` listener.
    NoCertificate,
    /// The certificate does not chain to any CA in the bundle.
    UntrustedCa,
    /// The certificate has expired.
    Expired,
    /// The certificate's validity window has not opened yet — usually a clock
    /// skew between the client's issuer and this host.
    NotYetValid,
    /// The certificate is listed in the CRL.
    Revoked,
    /// The certificate's revocation status could not be determined.
    UnknownRevocation,
    /// The certificate is otherwise unacceptable (malformed, bad signature, …).
    Invalid,
}

impl RejectionReason {
    /// The lowercase tag used as the metric label and log field.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::NoCertificate => "no_certificate",
            Self::UntrustedCa => "untrusted_ca",
            Self::Expired => "expired",
            Self::NotYetValid => "not_yet_valid",
            Self::Revoked => "revoked",
            Self::UnknownRevocation => "unknown_revocation",
            Self::Invalid => "invalid",
        }
    }

    /// Every reason, in slot order. The rate limiter sizes its per-reason
    /// arrays from this, and tests enumerate it.
    pub const ALL: [Self; 7] = [
        Self::NoCertificate,
        Self::UntrustedCa,
        Self::Expired,
        Self::NotYetValid,
        Self::Revoked,
        Self::UnknownRevocation,
        Self::Invalid,
    ];

    /// Index into the per-reason rate-limiter slots.
    const fn slot(self) -> usize {
        match self {
            Self::NoCertificate => 0,
            Self::UntrustedCa => 1,
            Self::Expired => 2,
            Self::NotYetValid => 3,
            Self::Revoked => 4,
            Self::UnknownRevocation => 5,
            Self::Invalid => 6,
        }
    }

    /// Classify a handshake error as a client-certificate rejection.
    ///
    /// Returns `None` for every other handshake failure (a plaintext client, an
    /// unsupported cipher, a dropped connection), which keeps server-only TLS
    /// logging byte-for-byte #1603's.
    #[must_use]
    pub const fn classify(error: &rustls::Error) -> Option<Self> {
        use rustls::CertificateError;

        match error {
            rustls::Error::NoCertificatesPresented => Some(Self::NoCertificate),
            rustls::Error::InvalidCertificate(cert_error) => Some(match cert_error {
                CertificateError::UnknownIssuer => Self::UntrustedCa,
                CertificateError::Expired | CertificateError::ExpiredContext { .. } => {
                    Self::Expired
                }
                CertificateError::NotValidYet | CertificateError::NotValidYetContext { .. } => {
                    Self::NotYetValid
                }
                CertificateError::Revoked => Self::Revoked,
                CertificateError::UnknownRevocationStatus
                | CertificateError::ExpiredRevocationList
                | CertificateError::ExpiredRevocationListContext { .. } => Self::UnknownRevocation,
                _ => Self::Invalid,
            }),
            _ => None,
        }
    }

    /// Count the rejection and log it, rate-limited to one line per second per
    /// reason. Suppressed lines are folded into the next emitted one's
    /// `suppressed` field, so a flood is visible without being verbose.
    pub fn record(self, peer: Option<std::net::SocketAddr>) {
        crate::metrics::counter(REJECTED_METRIC)
            .with_label("reason", self.as_str())
            .increment(1);

        if let Some(suppressed) = self.take_log_slot() {
            tracing::warn!(
                reason = self.as_str(),
                peer = peer.map(|p| p.to_string()).unwrap_or_default(),
                suppressed,
                "rejected an mTLS client certificate"
            );
        }
    }

    /// Claim this reason's log slot for the current second, returning how many
    /// lines were suppressed since the last emitted one. `None` means stay
    /// quiet.
    fn take_log_slot(self) -> Option<u64> {
        /// Last second at which each reason logged; `i64::MIN` means never.
        static LAST_LOG_UNIX: [AtomicI64; RejectionReason::ALL.len()] =
            [const { AtomicI64::new(i64::MIN) }; RejectionReason::ALL.len()];
        /// Rejections suppressed since each reason last logged.
        static SUPPRESSED: [AtomicU64; RejectionReason::ALL.len()] =
            [const { AtomicU64::new(0) }; RejectionReason::ALL.len()];

        let slot = self.slot();
        let now = super::now_unix();
        let last = LAST_LOG_UNIX[slot].load(Ordering::Relaxed);
        if now.saturating_sub(last) < LOG_INTERVAL_SECS {
            SUPPRESSED[slot].fetch_add(1, Ordering::Relaxed);
            return None;
        }
        // A compare-exchange, not a store: two threads crossing the interval
        // together must not both log. The loser counts as suppressed.
        if LAST_LOG_UNIX[slot]
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_err()
        {
            SUPPRESSED[slot].fetch_add(1, Ordering::Relaxed);
            return None;
        }
        Some(SUPPRESSED[slot].swap(0, Ordering::Relaxed))
    }
}

// ── Handler surface ─────────────────────────────────────────────────────────

/// Status returned when a route requires mTLS and the connection carries no
/// verified client certificate.
///
/// `403`, not `401`: there is no credential the client could re-send on this
/// connection, so a `WWW-Authenticate` challenge would be meaningless.
pub const REQUIRED_REJECTION_STATUS: http::StatusCode = http::StatusCode::FORBIDDEN;

/// Message in the JSON error envelope of an mTLS-required rejection.
pub const REQUIRED_REJECTION_MESSAGE: &str =
    "this route requires a verified client certificate (mTLS)";

/// The verified client identity of the current request.
///
/// Extracts the identity the TLS handshake established, rejecting with
/// [`REQUIRED_REJECTION_STATUS`] and the standard JSON error envelope when the
/// connection carries none — so a handler taking `ClientCert` is itself an
/// mTLS-only route.
///
/// Composes with [`Auth<T>`](crate::auth::Auth) and
/// [`RequireAuth`](crate::auth::RequireAuth) on the same router: machine
/// identity comes from the connection, session identity from the request, and
/// neither consults the other.
///
/// ```ignore
/// async fn rotate_keys(ClientCert(client): ClientCert) -> String {
///     format!("caller: {}", client.subject)
/// }
/// ```
#[derive(Debug, Clone)]
pub struct ClientCert(pub Arc<ClientIdentity>);

impl<S: Send + Sync> axum::extract::FromRequestParts<S> for ClientCert {
    type Rejection = crate::error::AutumnError;

    async fn from_request_parts(
        parts: &mut http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        parts
            .extensions
            .get::<Arc<ClientIdentity>>()
            .map(|id| Self(Arc::clone(id)))
            .ok_or_else(|| crate::error::AutumnError::forbidden_msg(REQUIRED_REJECTION_MESSAGE))
    }
}

/// The verified client identity, when there is one.
///
/// Never rejects, so a route can serve both authenticated machines and ordinary
/// callers on an `optional` listener.
#[derive(Debug, Clone)]
pub struct OptionalClientCert(pub Option<Arc<ClientIdentity>>);

impl<S: Send + Sync> axum::extract::FromRequestParts<S> for OptionalClientCert {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        Ok(Self(parts.extensions.get::<Arc<ClientIdentity>>().cloned()))
    }
}

// ── Per-request plumbing ────────────────────────────────────────────────────

tokio::task_local! {
    /// The verified client identity of the request being served on this task.
    ///
    /// Set by [`ClientIdentityLayer`] for the whole downstream call, so a
    /// [`PolicyContext`](crate::authorization::PolicyContext) built anywhere
    /// inside the handler — by `#[authorize]`, by a `#[repository(policy =
    /// ...)]` auto-API, or by hand — sees the machine identity without every
    /// call site having to thread it.
    static CURRENT_CLIENT_IDENTITY: Option<Arc<ClientIdentity>>;
}

/// The verified client identity of the request being served, if any.
///
/// Returns `None` outside a request scope, so hand-rolled unit tests and
/// background jobs read it safely.
#[must_use]
pub fn current_client_identity() -> Option<Arc<ClientIdentity>> {
    CURRENT_CLIENT_IDENTITY
        .try_with(Clone::clone)
        .unwrap_or_default()
}

/// Run `future` with `identity` as the ambient verified client identity.
///
/// The seam a test uses to exercise a policy that reads
/// [`PolicyContext::client_identity`](crate::authorization::PolicyContext::client_identity)
/// without booting a TLS listener. In production the same scope is established
/// by [`ClientIdentityLayer`] for the whole downstream call.
pub async fn with_client_identity<F: Future>(
    identity: Option<Arc<ClientIdentity>>,
    future: F,
) -> F::Output {
    CURRENT_CLIENT_IDENTITY.scope(identity, future).await
}

/// Tower [`Layer`](tower::Layer) that turns the HTTPS listener's
/// [`TlsConnectInfo`](crate::tls::TlsConnectInfo) into the per-request surface
/// the rest of the stack expects.
///
/// Three jobs, in order:
/// 1. Re-stamp `ConnectInfo<SocketAddr>` from the peer address, so
///    trusted-proxy resolution, [`ClientAddr`](crate::extract::ClientAddr),
///    rate limiting and everything else behave exactly as on plain TCP.
/// 2. Insert the verified identity as an `Arc<ClientIdentity>` extension, for
///    [`ClientCert`] / [`OptionalClientCert`] and
///    [`RequireClientCertLayer`].
/// 3. Run the downstream call inside a task-local scope carrying that identity,
///    so policies see it without a signature change.
#[derive(Clone, Debug, Default)]
pub struct ClientIdentityLayer;

impl<S> tower::Layer<S> for ClientIdentityLayer {
    type Service = ClientIdentityService<S>;

    fn layer(&self, inner: S) -> Self::Service {
        ClientIdentityService { inner }
    }
}

/// The [`ClientIdentityLayer`] service.
#[derive(Clone, Debug)]
pub struct ClientIdentityService<S> {
    inner: S,
}

impl<S, ReqBody> tower::Service<http::Request<ReqBody>> for ClientIdentityService<S>
where
    S: tower::Service<http::Request<ReqBody>> + Clone + Send + 'static,
    S::Future: Send + 'static,
    ReqBody: Send + 'static,
{
    type Response = S::Response;
    type Error = S::Error;
    type Future = std::pin::Pin<
        Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>,
    >;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut req: http::Request<ReqBody>) -> Self::Future {
        let connect_info = req
            .extensions()
            .get::<axum::extract::ConnectInfo<super::TlsConnectInfo>>()
            .map(|info| info.0.clone());
        let identity = match connect_info {
            Some(info) => {
                // Re-stamp the plain peer address so everything downstream —
                // trusted-proxy resolution, `ClientAddr`, rate limiting — sees
                // exactly what it sees on the plain-TCP path.
                req.extensions_mut()
                    .insert(axum::extract::ConnectInfo(info.peer));
                info.client
            }
            None => None,
        };

        if let Some(identity) = identity.clone() {
            req.extensions_mut().insert(identity);
        }

        // `poll_ready` was called on `self.inner`, so clone-and-swap to keep the
        // readiness with the service that is about to be driven.
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);
        Box::pin(CURRENT_CLIENT_IDENTITY.scope(identity, async move { inner.call(req).await }))
    }
}

// ── Per-route enforcement ───────────────────────────────────────────────────

/// Tower [`Layer`](tower::Layer) that rejects requests reaching mTLS-only
/// routes over a connection with no verified client certificate.
///
/// Two ways in:
/// - `[server.tls.client_auth] required_paths` — the framework applies this
///   layer with those prefixes, so a route is declared mTLS-only in config and
///   the requirement shows up in the security-posture manifest.
/// - [`RequireClientCertLayer::new`] on a sub-router — every route under it
///   requires a certificate.
///
/// Prefixes match the **normalized** request path (the same `clean_path` CSRF
/// exemptions use), so `/internal/../public` cannot slip past — nor into — a
/// requirement.
#[derive(Clone, Debug)]
pub struct RequireClientCertLayer {
    scope: RequirementScope,
}

/// Which requests a [`RequireClientCertLayer`] demands a certificate for.
#[derive(Clone, Debug)]
enum RequirementScope {
    /// Every request through the layer.
    AllRoutes,
    /// Only requests whose normalized path matches one of these prefixes. An
    /// empty list matches nothing, so a config with no `required_paths` leaves
    /// every route open rather than locking the whole app.
    Paths(Arc<Vec<String>>),
}

impl RequireClientCertLayer {
    /// Require a verified client certificate for every route under this layer.
    #[must_use]
    pub const fn new() -> Self {
        Self {
            scope: RequirementScope::AllRoutes,
        }
    }

    /// Require a verified client certificate only for requests whose normalized
    /// path matches one of `paths`. An empty list requires it nowhere.
    #[must_use]
    pub fn for_paths(paths: Vec<String>) -> Self {
        Self {
            scope: RequirementScope::Paths(Arc::new(paths)),
        }
    }

    /// Whether `path` (already normalized) falls under this layer's requirement.
    #[must_use]
    pub fn requires(&self, path: &str) -> bool {
        match &self.scope {
            RequirementScope::AllRoutes => true,
            RequirementScope::Paths(paths) => path_matches_any(path, paths),
        }
    }
}

impl Default for RequireClientCertLayer {
    fn default() -> Self {
        Self::new()
    }
}

/// Whether `path` equals or sits under one of `prefixes`.
///
/// Segment-boundary matching, so `/internal` never captures `/internal-tools`.
/// Unlike the CSRF *exemption* rule this mirrors, a prefix written with a
/// trailing slash ALSO covers the bare path: `/internal/` covers `/internal`.
/// The two rules fail in opposite directions — an exemption that matches too
/// little still validates the request, while a requirement that matches too
/// little leaves a route open — and an operator who writes `/internal/` and
/// mounts an index handler at `/internal` means to protect it.
#[must_use]
pub fn path_matches_any(path: &str, prefixes: &[String]) -> bool {
    prefixes.iter().any(|prefix| {
        let bare = prefix.strip_suffix('/').unwrap_or(prefix);
        if path == bare {
            true
        } else if let Some(rest) = path.strip_prefix(bare) {
            rest.starts_with('/')
        } else {
            false
        }
    })
}

impl<S> tower::Layer<S> for RequireClientCertLayer {
    type Service = RequireClientCert<S>;

    fn layer(&self, inner: S) -> Self::Service {
        RequireClientCert {
            inner,
            scope: self.scope.clone(),
        }
    }
}

/// The [`RequireClientCertLayer`] service.
#[derive(Clone, Debug)]
pub struct RequireClientCert<S> {
    inner: S,
    scope: RequirementScope,
}

impl<S, ResBody> tower::Service<http::Request<axum::body::Body>> for RequireClientCert<S>
where
    S: tower::Service<http::Request<axum::body::Body>, Response = http::Response<ResBody>>
        + Clone
        + Send
        + 'static,
    S::Future: Send + 'static,
    ResBody: Default + Send + 'static,
    axum::body::Body: From<ResBody>,
{
    type Response = http::Response<axum::body::Body>;
    type Error = S::Error;
    type Future = std::pin::Pin<
        Box<dyn Future<Output = Result<Self::Response, Self::Error>> + Send + 'static>,
    >;

    fn poll_ready(
        &mut self,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, req: http::Request<axum::body::Body>) -> Self::Future {
        let required = match &self.scope {
            RequirementScope::AllRoutes => true,
            // Skip the path normalization for the common no-op case: a
            // `[server.tls.client_auth]` with no `required_paths` never has to
            // clean a path. The layer itself is still in the stack.
            RequirementScope::Paths(paths) if paths.is_empty() => false,
            // Match the RAW path as well as the normalized one. Normalization
            // resolves dot segments, so `/internal/%2e%2e` cleans to `/` and
            // stops matching `/internal` — but axum's router matches the raw
            // path, so it still dispatches that request to `/internal/{id}`.
            // A requirement must fail closed: match either spelling.
            RequirementScope::Paths(paths) => {
                let raw = req.uri().path();
                path_matches_any(raw, paths)
                    || path_matches_any(crate::security::path::clean_path(raw).as_str(), paths)
            }
        };
        let verified = req.extensions().get::<Arc<ClientIdentity>>().is_some();

        if required && !verified {
            // The handshake itself succeeded (an `optional` listener, or a
            // plain-HTTP listener), so this is a route-level refusal, not a
            // transport one — count it so the rejection is observable next to
            // the handshake-level ones.
            crate::metrics::counter(ROUTE_REJECTED_METRIC).increment(1);
            let response = axum::response::IntoResponse::into_response(
                crate::error::AutumnError::forbidden_msg(REQUIRED_REJECTION_MESSAGE),
            );
            return Box::pin(async move { Ok(response) });
        }

        let future = self.inner.call(req);
        Box::pin(async move {
            let response = future.await?;
            Ok(response.map(axum::body::Body::from))
        })
    }
}

/// Metric counting requests rejected for reaching an mTLS-only route without a
/// verified client certificate. Named for the same reason
/// [`REJECTED_METRIC`] is.
pub const ROUTE_REJECTED_METRIC: &str = "tls_client_auth_route_rejected_total";

/// Classify a failed handshake as a client-certificate rejection and record it.
///
/// Returns `true` when it was one, so the listener can leave every other
/// handshake failure (a plaintext client, an unsupported cipher, a dropped
/// connection) on the quiet debug path #1603 already had.
///
/// Called from the listener's accept loop rather than from the verifier,
/// because that is the one place that sees BOTH the peer address and the
/// rejections rustls raises without ever consulting a verifier — chiefly a
/// `required` listener refusing a client that presented no certificate.
#[must_use]
pub fn record_handshake_rejection(error: &std::io::Error, peer: std::net::SocketAddr) -> bool {
    // `tokio_rustls` surfaces a handshake failure as an `io::Error` whose inner
    // error is the `rustls::Error`; anything else (a dropped socket, a timeout)
    // is not a certificate rejection.
    let Some(rustls_error) = error
        .get_ref()
        .and_then(|inner| inner.downcast_ref::<rustls::Error>())
    else {
        return false;
    };
    RejectionReason::classify(rustls_error).is_some_and(|reason| {
        reason.record(Some(peer));
        true
    })
}

// ── Offline inspection (`autumn doctor`) ────────────────────────────────────

/// One CA in the bundle, as inspected offline.
#[derive(Debug, Clone, PartialEq, Eq)]
#[non_exhaustive]
pub struct CaInspection {
    /// Subject distinguished name.
    pub subject: String,
    /// `notBefore` as a UNIX timestamp (seconds).
    pub not_before_unix: i64,
    /// `notAfter` as a UNIX timestamp (seconds).
    pub not_after_unix: i64,
}

impl CaInspection {
    /// Whole days from `now_unix` until `notAfter`; negative once expired.
    #[must_use]
    pub const fn days_until_expiry(&self, now_unix: i64) -> i64 {
        (self.not_after_unix - now_unix) / 86_400
    }

    /// Whether this CA has already expired at `now_unix`.
    #[must_use]
    pub const fn is_expired(&self, now_unix: i64) -> bool {
        self.not_after_unix <= now_unix
    }
}

/// The configured CRL file, as inspected offline.
///
/// A file may hold several CRLs — one per issuing CA — and rustls enforces all
/// of them, so this describes the file as a whole rather than its first entry.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[non_exhaustive]
pub struct CrlInspection {
    /// Issuer distinguished names, one per CRL in the file, in file order.
    pub issuers: Vec<String>,
    /// The EARLIEST `nextUpdate` across the file, as a UNIX timestamp; `None`
    /// when no CRL in it carries one.
    ///
    /// The earliest, not the first: one stale CRL in a file makes the file
    /// stale, because the revocations that CRL stopped publishing are the ones
    /// no longer enforced.
    pub next_update_unix: Option<i64>,
    /// How many certificates the file revokes in total.
    pub revoked_count: usize,
}

impl CrlInspection {
    /// Whether any CRL in the file has passed its `nextUpdate` at `now_unix`. A
    /// file whose CRLs carry no `nextUpdate` is never stale (there is nothing
    /// to have passed).
    #[must_use]
    pub fn is_stale(&self, now_unix: i64) -> bool {
        self.next_update_unix.is_some_and(|next| next <= now_unix)
    }
}

/// Parse the CA bundle and report each contained CA, WITHOUT booting a server.
/// Used by `autumn doctor`.
///
/// Performs the same load and trust-anchor validation as
/// [`load_client_roots`], then parses each certificate for its subject and
/// validity window, so an expired or near-expiry CA is gradeable.
///
/// # Errors
///
/// Returns a [`TlsError`] for a missing/unreadable file, unparseable PEM, an
/// empty bundle, or a certificate rustls refuses as a trust anchor.
pub fn inspect_client_ca_bundle(path: &Path) -> Result<Vec<CaInspection>, TlsError> {
    use x509_parser::prelude::FromDer as _;

    // Load through the runtime path first, so doctor rejects exactly what the
    // server rejects rather than grading a bundle that will not boot.
    load_client_roots(path)?;

    let mut out = Vec::new();
    for (idx, cert) in CertificateDer::pem_file_iter(path)
        .map_err(|source| map_pem_err(path, source))?
        .enumerate()
    {
        let cert = cert.map_err(|source| map_pem_err(path, source))?;
        let (_, parsed) = x509_parser::certificate::X509Certificate::from_der(cert.as_ref())
            .map_err(|e| TlsError::ParseChainCert {
                path: path.to_path_buf(),
                position: idx + 1,
                detail: e.to_string(),
            })?;
        out.push(CaInspection {
            subject: parsed.subject().to_string(),
            not_before_unix: parsed.validity().not_before.timestamp(),
            not_after_unix: parsed.validity().not_after.timestamp(),
        });
    }
    Ok(out)
}

/// Parse the CRL and report its issuer, `nextUpdate` and revocation count,
/// WITHOUT booting a server. Used by `autumn doctor`.
///
/// # Errors
///
/// Returns a [`TlsError`] for a missing/unreadable file, unparseable PEM, an
/// empty file, or a block that is not a parseable X.509 CRL.
pub fn inspect_crl(path: &Path) -> Result<CrlInspection, TlsError> {
    use x509_parser::prelude::FromDer as _;

    let ders = load_crls(path)?;
    let mut inspection = CrlInspection::default();
    for (idx, der) in ders.iter().enumerate() {
        let (_, crl) =
            x509_parser::revocation_list::CertificateRevocationList::from_der(der.as_ref())
                .map_err(|e| TlsError::ParseCrlDer {
                    path: path.to_path_buf(),
                    position: idx + 1,
                    detail: e.to_string(),
                })?;
        inspection.revoked_count += crl.iter_revoked_certificates().count();
        inspection.issuers.push(crl.issuer().to_string());
        if let Some(next) = crl.next_update().map(|t| t.timestamp()) {
            // The earliest wins: one stale CRL in the file is a stale file.
            inspection.next_update_unix = Some(
                inspection
                    .next_update_unix
                    .map_or(next, |cur| cur.min(next)),
            );
        }
    }
    Ok(inspection)
}

#[cfg(test)]
mod tests {
    use super::*;

    const CA_PEM: &str = include_str!("../../tests/fixtures/tls/client/ca.cert.pem");
    const OTHER_CA_PEM: &str = include_str!("../../tests/fixtures/tls/client/other-ca.cert.pem");
    const CLIENT_PEM: &str = include_str!("../../tests/fixtures/tls/client/client.cert.pem");
    const CRL_PEM: &str = include_str!("../../tests/fixtures/tls/client/crl.pem");
    const CRL_EMPTY_PEM: &str = include_str!("../../tests/fixtures/tls/client/crl-empty.pem");
    /// A certificate whose `O` attribute contains a literal `, CN=svc-payments`
    /// — the DN-injection shape `common_name()` must not fall for.
    const SPOOFED_CN_PEM: &str =
        include_str!("../../tests/fixtures/tls/client/spoofed-cn.cert.pem");

    /// Write `contents` into a fresh tempdir as `name`, returning the dir (kept
    /// alive by the caller) and the path.
    fn write_temp(name: &str, contents: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join(name);
        std::fs::write(&path, contents).expect("write fixture");
        (dir, path)
    }

    /// The first DER certificate in a PEM blob.
    fn first_der(pem: &str) -> CertificateDer<'static> {
        CertificateDer::pem_slice_iter(pem.as_bytes())
            .next()
            .expect("a CERTIFICATE block")
            .expect("parseable")
    }

    // ── identity ────────────────────────────────────────────────────────────

    #[test]
    fn identity_carries_subject_issuer_sans_and_fingerprint() {
        let id = ClientIdentity::from_der(&first_der(CLIENT_PEM)).expect("parse client cert");

        assert!(
            id.subject.contains("CN=svc-orders"),
            "subject DN should name the service: {}",
            id.subject
        );
        assert!(
            id.issuer.contains("CN=Autumn Test Client CA"),
            "issuer DN should name the CA: {}",
            id.issuer
        );
        assert!(id.has_san("DNS:svc-orders.internal"), "sans: {:?}", id.sans);
        assert!(
            id.has_san("URI:spiffe://autumn.test/svc/orders"),
            "sans: {:?}",
            id.sans
        );
        assert!(
            id.has_san("email:orders@autumn.test"),
            "sans: {:?}",
            id.sans
        );
        assert_eq!(id.common_name(), Some("svc-orders"));
        // DNS SANs compare case-insensitively; other kinds do not.
        assert!(id.has_san("DNS:SVC-Orders.Internal"));
        assert!(!id.has_san("URI:SPIFFE://autumn.test/svc/orders"));

        // Fingerprint is the SHA-256 of the DER, hex, prefixed.
        let expected = {
            use std::fmt::Write as _;

            use sha2::{Digest as _, Sha256};
            let digest = Sha256::digest(first_der(CLIENT_PEM).as_ref());
            digest.iter().fold("sha256:".to_owned(), |mut acc, b| {
                let _ = write!(acc, "{b:02x}");
                acc
            })
        };
        assert_eq!(id.fingerprint, expected);
        assert!(!id.serial.is_empty());
        assert!(id.not_after_unix > 0);
    }

    #[test]
    fn common_name_cannot_be_smuggled_through_another_attribute() {
        // `X509Name`'s Display does not escape attribute values, so a subject
        // carrying `O=Acme, CN=svc-payments` renders indistinguishably from two
        // real attributes. The CN must come from the PARSED name, or an
        // enrolment service that copies `O` from a CSR lets a caller pick its
        // own identity.
        let id = ClientIdentity::from_der(&first_der(SPOOFED_CN_PEM))
            .expect("parse the spoofed certificate");
        assert!(
            id.subject.contains("CN=svc-payments"),
            "fixture should render an injected CN: {}",
            id.subject
        );
        assert_eq!(
            id.common_name(),
            Some("svc-lowpriv"),
            "the real CN must win over one smuggled into another attribute"
        );
    }

    #[test]
    fn identity_rejects_bytes_that_are_not_a_certificate() {
        let der = CertificateDer::from(vec![0x30, 0x00]);
        let err = ClientIdentity::from_der(&der).expect_err("garbage is not a certificate");
        assert!(matches!(err, TlsError::ParsePeerCert { .. }), "{err}");
    }

    // ── trust store loading ─────────────────────────────────────────────────

    #[test]
    fn loads_a_multi_ca_bundle() {
        // Old + new CA in one file: the shape a rotation ships.
        let (_dir, path) = write_temp("ca.pem", &format!("{CA_PEM}{OTHER_CA_PEM}"));
        let roots = load_client_roots(&path).expect("bundle loads");
        assert_eq!(roots.len(), 2);
    }

    #[test]
    fn rejects_a_missing_bundle() {
        let err = load_client_roots(Path::new("/nonexistent/ca.pem"))
            .expect_err("a missing bundle must fail fast");
        assert!(matches!(err, TlsError::ReadClientCa { .. }), "{err}");
        assert!(err.to_string().contains("/nonexistent/ca.pem"), "{err}");
    }

    #[test]
    fn rejects_an_empty_bundle() {
        let (_dir, path) = write_temp("ca.pem", "# no certificates here\n");
        let err = load_client_roots(&path).expect_err("an empty bundle must fail fast");
        assert!(matches!(err, TlsError::NoClientCas { .. }), "{err}");
    }

    #[test]
    fn rejects_a_malformed_bundle() {
        let (_dir, path) = write_temp(
            "ca.pem",
            "-----BEGIN CERTIFICATE-----\nnot base64 at all!\n-----END CERTIFICATE-----\n",
        );
        let err = load_client_roots(&path).expect_err("malformed PEM must fail fast");
        assert!(matches!(err, TlsError::ParseClientCa { .. }), "{err}");
    }

    #[test]
    fn rejects_a_missing_or_empty_crl() {
        let err =
            load_crls(Path::new("/nonexistent/crl.pem")).expect_err("a missing CRL must fail fast");
        assert!(matches!(err, TlsError::ReadCrl { .. }), "{err}");

        let (_dir, path) = write_temp("crl.pem", "\n");
        let err = load_crls(&path).expect_err("an empty CRL file must fail fast");
        assert!(matches!(err, TlsError::NoCrls { .. }), "{err}");
    }

    #[test]
    fn loads_a_crl() {
        let (_dir, path) = write_temp("crl.pem", CRL_PEM);
        assert_eq!(load_crls(&path).expect("CRL loads").len(), 1);
    }

    // ── verifier construction ───────────────────────────────────────────────

    fn roots_from(pem: &str) -> RootCertStore {
        let (_dir, path) = write_temp("ca.pem", pem);
        load_client_roots(&path).expect("roots load")
    }

    #[test]
    fn required_mode_mandates_a_certificate_and_optional_does_not() {
        let provider = super::super::crypto_provider();
        let required = build_client_verifier(
            roots_from(CA_PEM),
            Vec::new(),
            ClientAuthMode::Required,
            Arc::clone(&provider),
        )
        .expect("required verifier builds");
        assert!(required.client_auth_mandatory());

        let optional = build_client_verifier(
            roots_from(CA_PEM),
            Vec::new(),
            ClientAuthMode::Optional,
            provider,
        )
        .expect("optional verifier builds");
        assert!(!optional.client_auth_mandatory());
    }

    #[test]
    fn reloadable_verifier_reports_the_configured_mode() {
        let provider = super::super::crypto_provider();
        let inner = build_client_verifier(
            roots_from(CA_PEM),
            Vec::new(),
            ClientAuthMode::Optional,
            provider,
        )
        .expect("verifier builds");
        let reloadable = ReloadableClientVerifier::new(inner, ClientAuthMode::Optional);

        assert!(reloadable.offer_client_auth());
        assert!(!reloadable.client_auth_mandatory());
        assert_eq!(reloadable.mode(), ClientAuthMode::Optional);
        // No hint subjects are advertised: the store rotates, so a borrowed
        // snapshot could not outlive the swap it describes.
        assert!(reloadable.root_hint_subjects().is_empty());
    }

    #[test]
    fn reloadable_verifier_swaps_the_inner_verifier() {
        let provider = super::super::crypto_provider();
        let first = build_client_verifier(
            roots_from(CA_PEM),
            Vec::new(),
            ClientAuthMode::Required,
            Arc::clone(&provider),
        )
        .expect("first verifier");
        let reloadable =
            ReloadableClientVerifier::new(Arc::clone(&first), ClientAuthMode::Required);
        assert!(Arc::ptr_eq(&reloadable.current(), &first));

        let second = build_client_verifier(
            roots_from(OTHER_CA_PEM),
            Vec::new(),
            ClientAuthMode::Required,
            provider,
        )
        .expect("second verifier");
        reloadable.store(Arc::clone(&second));
        assert!(Arc::ptr_eq(&reloadable.current(), &second));
    }

    // ── rotation ────────────────────────────────────────────────────────────

    #[tokio::test]
    async fn reloader_swaps_when_the_bundle_changes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bundle = dir.path().join("ca.pem");
        std::fs::write(&bundle, CA_PEM).expect("write bundle");

        let (verifier, mut reloader) = ClientTrustReloader::load(
            bundle.clone(),
            None,
            ClientAuthMode::Required,
            super::super::crypto_provider(),
            std::time::Duration::from_millis(10),
        )
        .expect("initial load");
        let before = verifier.current();

        // A rotation ships old + new in one bundle.
        std::fs::write(&bundle, format!("{CA_PEM}{OTHER_CA_PEM}")).expect("rotate");
        bump_mtime(&bundle);
        reloader.poll_once().await;

        assert!(
            !Arc::ptr_eq(&verifier.current(), &before),
            "a changed bundle should swap the verifier"
        );
    }

    #[tokio::test]
    async fn reloader_keeps_the_previous_store_when_the_new_one_is_broken() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bundle = dir.path().join("ca.pem");
        std::fs::write(&bundle, CA_PEM).expect("write bundle");

        let (verifier, mut reloader) = ClientTrustReloader::load(
            bundle.clone(),
            None,
            ClientAuthMode::Required,
            super::super::crypto_provider(),
            std::time::Duration::from_millis(10),
        )
        .expect("initial load");
        let before = verifier.current();

        std::fs::write(&bundle, "-----BEGIN CERTIFICATE-----\ngarbage\n").expect("corrupt");
        bump_mtime(&bundle);
        reloader.poll_once().await;

        assert!(
            Arc::ptr_eq(&verifier.current(), &before),
            "a broken reload must keep the previous trust store"
        );
    }

    #[tokio::test]
    async fn reloader_swaps_when_only_the_crl_changes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bundle = dir.path().join("ca.pem");
        let crl = dir.path().join("crl.pem");
        std::fs::write(&bundle, CA_PEM).expect("write bundle");
        std::fs::write(&crl, CRL_EMPTY_PEM).expect("write crl");

        let (verifier, mut reloader) = ClientTrustReloader::load(
            bundle,
            Some(crl.clone()),
            ClientAuthMode::Required,
            super::super::crypto_provider(),
            std::time::Duration::from_millis(10),
        )
        .expect("initial load");
        let before = verifier.current();

        std::fs::write(&crl, CRL_PEM).expect("publish a revocation");
        bump_mtime(&crl);
        reloader.poll_once().await;

        assert!(
            !Arc::ptr_eq(&verifier.current(), &before),
            "a newly published revocation should swap the verifier"
        );
    }

    #[tokio::test]
    async fn reloader_is_quiet_when_nothing_changed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let bundle = dir.path().join("ca.pem");
        std::fs::write(&bundle, CA_PEM).expect("write bundle");

        let (verifier, mut reloader) = ClientTrustReloader::load(
            bundle,
            None,
            ClientAuthMode::Required,
            super::super::crypto_provider(),
            std::time::Duration::from_millis(10),
        )
        .expect("initial load");
        let before = verifier.current();

        reloader.poll_once().await;
        assert!(
            Arc::ptr_eq(&verifier.current(), &before),
            "an unchanged bundle must not swap"
        );
    }

    /// Stamp a strictly later mtime, so a rewrite inside one filesystem
    /// timestamp tick is still visible to the mtime poller.
    fn bump_mtime(path: &Path) {
        let later = filetime::FileTime::from_unix_time(super::super::now_unix() + 60, 0);
        filetime::set_file_mtime(path, later).expect("set mtime");
    }

    // ── diagnostics ─────────────────────────────────────────────────────────

    #[test]
    fn a_required_prefix_covers_the_bare_path_but_not_a_sibling() {
        let prefixes = vec!["/internal/".to_owned()];
        // The regression: an index handler mounted at exactly `/internal` was
        // left open by a prefix written with a trailing slash.
        assert!(path_matches_any("/internal", &prefixes));
        assert!(path_matches_any("/internal/", &prefixes));
        assert!(path_matches_any("/internal/keys", &prefixes));
        assert!(!path_matches_any("/internal-tools", &prefixes));
        assert!(!path_matches_any("/internals", &prefixes));
        assert!(!path_matches_any("/", &prefixes));

        // Written without the slash, the same three hold.
        let bare = vec!["/internal".to_owned()];
        assert!(path_matches_any("/internal", &bare));
        assert!(path_matches_any("/internal/keys", &bare));
        assert!(!path_matches_any("/internal-tools", &bare));
    }

    #[test]
    fn a_multi_crl_file_reports_the_earliest_next_update() {
        // rustls enforces every CRL in the file, so one stale entry makes the
        // file stale — reporting only the first CRL's window would hide it.
        let (_dir, path) = write_temp("crl.pem", &format!("{CRL_EMPTY_PEM}{CRL_PEM}"));
        let crl = inspect_crl(&path).expect("multi-CRL file inspects");
        assert_eq!(crl.issuers.len(), 2);
        assert_eq!(crl.revoked_count, 1, "counts across every CRL in the file");
        let earliest = crl.next_update_unix.expect("both fixtures carry one");
        assert!(crl.is_stale(earliest + 1));
        assert!(!crl.is_stale(earliest - 1));
    }

    #[test]
    fn classifies_every_documented_rejection_reason() {
        use rustls::CertificateError;

        assert_eq!(
            RejectionReason::classify(&rustls::Error::NoCertificatesPresented),
            Some(RejectionReason::NoCertificate)
        );
        assert_eq!(
            RejectionReason::classify(&rustls::Error::InvalidCertificate(
                CertificateError::UnknownIssuer
            )),
            Some(RejectionReason::UntrustedCa)
        );
        assert_eq!(
            RejectionReason::classify(&rustls::Error::InvalidCertificate(
                CertificateError::Expired
            )),
            Some(RejectionReason::Expired)
        );
        assert_eq!(
            RejectionReason::classify(&rustls::Error::InvalidCertificate(
                CertificateError::NotValidYet
            )),
            Some(RejectionReason::NotYetValid)
        );
        assert_eq!(
            RejectionReason::classify(&rustls::Error::InvalidCertificate(
                CertificateError::Revoked
            )),
            Some(RejectionReason::Revoked)
        );
        assert_eq!(
            RejectionReason::classify(&rustls::Error::InvalidCertificate(
                CertificateError::UnknownRevocationStatus
            )),
            Some(RejectionReason::UnknownRevocation)
        );
        assert_eq!(
            RejectionReason::classify(&rustls::Error::InvalidCertificate(
                CertificateError::BadSignature
            )),
            Some(RejectionReason::Invalid)
        );
    }

    #[test]
    fn leaves_ordinary_handshake_failures_unclassified() {
        // Server-only TLS keeps #1603's logging: only client-certificate
        // failures are re-reported as mTLS rejections.
        assert_eq!(
            RejectionReason::classify(&rustls::Error::DecryptError),
            None
        );
        assert_eq!(
            RejectionReason::classify(&rustls::Error::NoApplicationProtocol),
            None
        );
    }

    #[test]
    fn every_reason_has_a_distinct_tag_and_slot() {
        let mut tags: Vec<&str> = RejectionReason::ALL.iter().map(|r| r.as_str()).collect();
        tags.sort_unstable();
        let count = tags.len();
        tags.dedup();
        assert_eq!(tags.len(), count, "reason tags must be distinct");

        let mut slots: Vec<usize> = RejectionReason::ALL.iter().map(|r| r.slot()).collect();
        slots.sort_unstable();
        assert_eq!(slots, (0..count).collect::<Vec<_>>());
    }

    // ── offline inspection ──────────────────────────────────────────────────

    #[test]
    fn inspects_every_ca_in_the_bundle() {
        let (_dir, path) = write_temp("ca.pem", &format!("{CA_PEM}{OTHER_CA_PEM}"));
        let cas = inspect_client_ca_bundle(&path).expect("bundle inspects");
        assert_eq!(cas.len(), 2);
        assert!(cas[0].subject.contains("CN=Autumn Test Client CA"));
        assert!(!cas[0].is_expired(super::super::now_unix()));
        assert!(cas[0].days_until_expiry(super::super::now_unix()) > 3650);
    }

    #[test]
    fn inspects_a_crl_with_its_next_update_and_revocations() {
        let (_dir, path) = write_temp("crl.pem", CRL_PEM);
        let crl = inspect_crl(&path).expect("CRL inspects");
        assert!(
            crl.issuers[0].contains("CN=Autumn Test Client CA"),
            "{crl:?}"
        );
        assert_eq!(crl.revoked_count, 1);
        assert!(!crl.is_stale(super::super::now_unix()));
        // A CRL whose nextUpdate has passed is stale.
        assert!(crl.is_stale(crl.next_update_unix.expect("fixture has nextUpdate") + 1));
    }

    #[test]
    fn inspects_an_empty_crl_as_zero_revocations() {
        let (_dir, path) = write_temp("crl.pem", CRL_EMPTY_PEM);
        let crl = inspect_crl(&path).expect("CRL inspects");
        assert_eq!(crl.revoked_count, 0);
    }

    #[test]
    fn inspection_rejects_what_the_runtime_rejects() {
        let (_dir, path) = write_temp("ca.pem", "");
        assert!(matches!(
            inspect_client_ca_bundle(&path),
            Err(TlsError::NoClientCas { .. })
        ));
    }
}
