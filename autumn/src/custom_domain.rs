//! Tenant custom domains with per-domain certificates (issue #1635).
//!
//! A B2B tenant connects its own hostname (`app.clientco.com`) instead of a
//! subdomain of the operator's base domain. This module owns the parts that do
//! not need a CA:
//!
//! - the hostname → tenant [registry](CustomDomainRegistry) and its persistence
//!   ([`CustomDomainStore`]),
//! - the tenant-facing [DNS instructions](DnsInstructions) — CNAME for a
//!   subdomain, A/AAAA for an apex, which cannot carry a CNAME,
//! - the **verification gate** ([`grade_dns_verification`]): no ACME order is
//!   created until the hostname is independently observed to point at this
//!   deployment,
//! - the [issuance budget](IssuanceLimiter) — per-domain and global caps plus
//!   exponential backoff — that keeps a misconfigured tenant from burning the
//!   CA's rate limits.
//!
//! Certificate selection by SNI ([`SniCertResolver`]) is behind the `tls`
//! feature; the ACME orchestration that drives this state machine is in
//! `crate::acme::tenant_domains`, behind the `acme` feature.
//!
//! # State machine
//!
//! ```text
//!   register ──▶ PendingDns ──verified──▶ Verified ──order──▶ Issuing ──▶ Active
//!                    ▲                                            │          │
//!                    └──────────── failure (reason + backoff) ◀───┘          │
//!                                                                            │
//!                              renewal failure keeps Active (still serving) ─┘
//! ```
//!
//! A failure never destroys a working certificate: a domain that is already
//! `Active` stays `Active` with a `failure_reason` set, so one tenant's failed
//! renewal cannot stop it — or anyone else — being served.

use std::collections::HashMap;
use std::future::Future;
use std::io;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::{Arc, RwLock};

use serde::{Deserialize, Serialize};

/// Boxed future returned by the async [`CustomDomainStore`] methods.
pub type StoreFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

/// Longest hostname the DNS wire format allows, minus the root label.
const MAX_HOSTNAME_LEN: usize = 253;
/// Longest single DNS label.
const MAX_LABEL_LEN: usize = 63;

// ── Hostname normalisation ───────────────────────────────────────────────

/// Normalise a hostname to the form the registry, SNI, and ACME all key on:
/// lowercase, no trailing dot, no port.
///
/// DNS names are case-insensitive and SNI arrives lowercase, so normalising at
/// the registration boundary is what makes a `Host:` header, a TLS SNI value,
/// and a stored record compare equal.
///
/// # Errors
///
/// Returns a message naming the problem for anything that cannot be a
/// certificate subject: an empty name, a wildcard (a tenant proves control of
/// one name, not a whole zone), an IP literal, a single-label name, or a label
/// that breaks DNS syntax.
pub fn normalize_hostname(raw: &str) -> Result<String, String> {
    let trimmed = raw.trim().trim_end_matches('.');
    // Strip a port: `Host:` carries one, a certificate subject never does.
    let without_port = trimmed.split(':').next().unwrap_or(trimmed);
    let host = without_port.trim().to_ascii_lowercase();

    if host.is_empty() {
        return Err("hostname must not be empty".to_owned());
    }
    if host.len() > MAX_HOSTNAME_LEN {
        return Err(format!(
            "hostname is {} bytes; DNS allows at most {MAX_HOSTNAME_LEN}",
            host.len()
        ));
    }
    if host.contains('*') {
        return Err(
            "wildcard hostnames are not supported for custom domains: a tenant proves control of \
             one name, not a whole zone"
                .to_owned(),
        );
    }
    if host.parse::<IpAddr>().is_ok() {
        return Err("an IP address cannot be a custom domain".to_owned());
    }
    let labels: Vec<&str> = host.split('.').collect();
    if labels.len() < 2 {
        return Err(format!(
            "{host} is a single label; a custom domain needs at least one dot"
        ));
    }
    for label in &labels {
        if label.is_empty() {
            return Err(format!("{host} has an empty DNS label"));
        }
        if label.len() > MAX_LABEL_LEN {
            return Err(format!("label {label} exceeds {MAX_LABEL_LEN} bytes"));
        }
        if label.starts_with('-') || label.ends_with('-') {
            return Err(format!("label {label} must not start or end with '-'"));
        }
        if !label
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        {
            return Err(format!(
                "label {label} may contain only letters, digits and '-' (punycode an \
                 internationalised name before registering it)"
            ));
        }
    }
    Ok(host)
}

/// Is `hostname` an apex (registrable) name, which cannot carry a CNAME?
///
/// Approximated as "exactly two labels". A public-suffix list would be exact
/// (`co.uk` needs three), but the cost of being wrong is only which record type
/// the instructions suggest — and [`grade_dns_verification`] accepts either
/// shape, so a tenant who follows a CNAME suggestion on a three-label apex and
/// gets a DNS-provider error can still use A/AAAA and verify.
#[must_use]
pub fn is_apex(hostname: &str) -> bool {
    hostname.split('.').filter(|l| !l.is_empty()).count() <= 2
}

// ── Tenant-facing DNS instructions ───────────────────────────────────────

/// Where this deployment's ingress lives, as the tenant must point DNS at it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ExpectedIngress {
    /// The operator's ingress hostname, the CNAME target for subdomains.
    pub hostname: Option<String>,
    /// The ingress IPv4 addresses, for apex domains.
    pub ipv4: Vec<Ipv4Addr>,
    /// The ingress IPv6 addresses, for apex domains.
    pub ipv6: Vec<Ipv6Addr>,
}

impl ExpectedIngress {
    /// Is there anything a tenant could be told to point at?
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.hostname.is_none() && self.ipv4.is_empty() && self.ipv6.is_empty()
    }

    /// The normalised CNAME target, if one is configured.
    #[must_use]
    fn cname_target(&self) -> Option<String> {
        self.hostname
            .as_ref()
            .map(|h| h.trim().trim_end_matches('.').to_ascii_lowercase())
    }
}

/// The DNS record a tenant must publish for one custom domain.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DnsInstructions {
    /// A subdomain: one CNAME at the operator's ingress hostname.
    Cname {
        /// The record name the tenant creates.
        name: String,
        /// The CNAME target.
        value: String,
    },
    /// An apex: A and/or AAAA records, because RFC 1034 forbids a CNAME
    /// alongside the SOA/NS records an apex must carry.
    Address {
        /// The record name the tenant creates.
        name: String,
        /// A-record values.
        ipv4: Vec<String>,
        /// AAAA-record values.
        ipv6: Vec<String>,
    },
}

impl DnsInstructions {
    /// Build the instructions for `hostname` against this deployment's ingress.
    ///
    /// # Errors
    ///
    /// Returns a message when the hostname is invalid, or when the ingress
    /// carries nothing of the kind this hostname needs (no CNAME target for a
    /// subdomain, no addresses for an apex).
    pub fn for_hostname(hostname: &str, ingress: &ExpectedIngress) -> Result<Self, String> {
        let host = normalize_hostname(hostname)?;
        if is_apex(&host) {
            if ingress.ipv4.is_empty() && ingress.ipv6.is_empty() {
                return Err(format!(
                    "{host} is an apex domain, which cannot carry a CNAME, but no ingress \
                     addresses are configured; set [server.tls.acme.custom_domains] ingress_ipv4 \
                     / ingress_ipv6"
                ));
            }
            return Ok(Self::Address {
                name: host,
                ipv4: ingress.ipv4.iter().map(ToString::to_string).collect(),
                ipv6: ingress.ipv6.iter().map(ToString::to_string).collect(),
            });
        }
        let value = ingress.cname_target().ok_or_else(|| {
            format!(
                "{host} needs a CNAME target, but no ingress hostname is configured; set \
                 [server.tls.acme.custom_domains] ingress_hostname"
            )
        })?;
        Ok(Self::Cname { name: host, value })
    }

    /// The instructions as a line a tenant-facing screen can print verbatim.
    #[must_use]
    pub fn render(&self) -> String {
        match self {
            Self::Cname { name, value } => format!("{name}\tCNAME\t{value}"),
            Self::Address { name, ipv4, ipv6 } => {
                use std::fmt::Write as _;
                let mut out = String::new();
                for (kind, addrs) in [("A", ipv4), ("AAAA", ipv6)] {
                    for addr in addrs {
                        let _ = writeln!(out, "{name}\t{kind}\t{addr}");
                    }
                }
                out.trim_end().to_owned()
            }
        }
    }
}

// ── Verification ─────────────────────────────────────────────────────────

/// What a DNS lookup actually observed for a custom domain.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ObservedTarget {
    /// The name is a CNAME pointing at this value.
    Cname(String),
    /// The name resolves to these addresses.
    Addresses(Vec<IpAddr>),
    /// The name does not resolve.
    None,
}

/// Whether a custom domain points at this deployment.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VerificationOutcome {
    /// Confirmed: the ACME order may proceed.
    PointsHere,
    /// The name resolves, but not here. Naming what was observed is what makes
    /// a stuck domain self-service for the tenant.
    PointsElsewhere {
        /// What was seen, for the status surface.
        detail: String,
    },
    /// The name does not resolve yet — the ordinary state right after a tenant
    /// is handed the instructions.
    Unresolved,
}

impl VerificationOutcome {
    /// Did verification confirm the hostname points here?
    #[must_use]
    pub const fn is_verified(&self) -> bool {
        matches!(self, Self::PointsHere)
    }

    /// The reason to show a tenant whose domain is stuck.
    #[must_use]
    pub fn reason(&self) -> Option<String> {
        match self {
            Self::PointsHere => None,
            Self::PointsElsewhere { detail } => Some(format!(
                "DNS does not point at this deployment yet: {detail}"
            )),
            Self::Unresolved => Some(
                "DNS does not resolve yet. Publish the record above; propagation can take up to \
                 the record's TTL."
                    .to_owned(),
            ),
        }
    }
}

/// Grade an observation against the configured ingress (pure; injectable).
///
/// A CNAME matches when it names the ingress hostname (trailing dot and case
/// ignored). Addresses match when **every** resolved address is a configured
/// ingress address: a partial match still lets a request — and the CA's
/// HTTP-01 fetch — land on a host that serves neither the token nor the
/// tenant, so it is not a pass.
#[must_use]
pub fn grade_dns_verification(
    observed: &ObservedTarget,
    expected: &ExpectedIngress,
) -> VerificationOutcome {
    match observed {
        ObservedTarget::None => VerificationOutcome::Unresolved,
        ObservedTarget::Cname(target) => {
            let target_norm = target.trim().trim_end_matches('.').to_ascii_lowercase();
            if expected.cname_target().as_deref() == Some(target_norm.as_str()) {
                VerificationOutcome::PointsHere
            } else {
                VerificationOutcome::PointsElsewhere {
                    detail: format!("CNAME points at {target_norm}"),
                }
            }
        }
        ObservedTarget::Addresses(addrs) if addrs.is_empty() => VerificationOutcome::Unresolved,
        // Nothing to compare against: an ingress with no addresses (and whose
        // hostname could not be resolved to any) cannot confirm anything, and
        // an empty expected set must never read as "every address matches".
        ObservedTarget::Addresses(_) if expected.ipv4.is_empty() && expected.ipv6.is_empty() => {
            VerificationOutcome::PointsElsewhere {
                detail: "this deployment's ingress addresses are unknown, so the hostname cannot \
                         be confirmed to point here"
                    .to_owned(),
            }
        }
        ObservedTarget::Addresses(addrs) => {
            let unmatched: Vec<String> = addrs
                .iter()
                .filter(|addr| !ingress_contains(expected, **addr))
                .map(ToString::to_string)
                .collect();
            if unmatched.is_empty() {
                VerificationOutcome::PointsHere
            } else {
                VerificationOutcome::PointsElsewhere {
                    detail: format!("resolves to {}", unmatched.join(", ")),
                }
            }
        }
    }
}

fn ingress_contains(expected: &ExpectedIngress, addr: IpAddr) -> bool {
    match addr {
        IpAddr::V4(v4) => expected.ipv4.contains(&v4),
        IpAddr::V6(v6) => expected.ipv6.contains(&v6),
    }
}

/// Apply a verification result to `hostname`'s registry record.
///
/// Success promotes `PendingDns` → `Verified`, which is the only thing that
/// makes a domain eligible for an ACME order. Failure records the reason and a
/// backoff so a tenant who has not published the record yet is retried at a
/// decaying rate rather than every tick.
///
/// # Errors
///
/// Propagates the store's write error.
pub async fn apply_verification(
    registry: &CustomDomainRegistry,
    hostname: &str,
    outcome: &VerificationOutcome,
    now_unix: i64,
    backoff_secs: i64,
) -> io::Result<()> {
    if outcome.is_verified() {
        registry.record_verified(hostname, now_unix).await
    } else {
        let reason = outcome
            .reason()
            .unwrap_or_else(|| "verification failed".to_owned());
        registry
            .record_failure(hostname, now_unix, reason, backoff_secs)
            .await
    }
}

// ── The domain record ────────────────────────────────────────────────────

/// Where a custom domain is in its lifecycle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DomainStatus {
    /// Registered; DNS has not been confirmed to point here.
    PendingDns,
    /// DNS confirmed; eligible for an ACME order.
    Verified,
    /// An ACME order is in flight.
    Issuing,
    /// A certificate is stored and served.
    Active,
}

impl DomainStatus {
    /// The stable string an app renders and a status API returns.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::PendingDns => "pending_dns",
            Self::Verified => "verified",
            Self::Issuing => "issuing",
            Self::Active => "active",
        }
    }
}

impl std::fmt::Display for DomainStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

/// One tenant-connected hostname and everything the status surface renders.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CustomDomain {
    /// Normalised hostname (see [`normalize_hostname`]).
    pub hostname: String,
    /// The tenant id requests on this hostname resolve to.
    pub tenant: String,
    /// Current lifecycle state.
    pub status: DomainStatus,
    /// Why the domain is stuck, if it is. Cleared on the next success.
    pub failure_reason: Option<String>,
    /// When the app registered the hostname.
    pub registered_at_unix: i64,
    /// When DNS was first confirmed.
    pub verified_at_unix: Option<i64>,
    /// When the first certificate started being served.
    pub activated_at_unix: Option<i64>,
    /// `notAfter` of the served certificate, for renewal scheduling.
    pub cert_not_after_unix: Option<i64>,
    /// Consecutive failures, the exponent in [`backoff_secs`].
    pub consecutive_failures: u32,
    /// Earliest time the orchestrator may retry this domain.
    pub next_attempt_unix: Option<i64>,
}

impl CustomDomain {
    /// A freshly registered domain, awaiting DNS.
    #[must_use]
    const fn new(hostname: String, tenant: String, now_unix: i64) -> Self {
        Self {
            hostname,
            tenant,
            status: DomainStatus::PendingDns,
            failure_reason: None,
            registered_at_unix: now_unix,
            verified_at_unix: None,
            activated_at_unix: None,
            cert_not_after_unix: None,
            consecutive_failures: 0,
            next_attempt_unix: None,
        }
    }

    /// May this domain be worked on at `now_unix`, or is it still in backoff?
    #[must_use]
    pub fn is_due(&self, now_unix: i64) -> bool {
        self.next_attempt_unix.is_none_or(|at| now_unix >= at)
    }

    /// Does a request or handshake for this hostname get served?
    ///
    /// Only `Active`: a domain whose certificate has not been issued has
    /// nothing to serve, and routing it would leak the existence of a tenant
    /// to whoever pointed DNS here first.
    #[must_use]
    pub const fn is_servable(&self) -> bool {
        matches!(self.status, DomainStatus::Active)
    }
}

// ── Persistence ──────────────────────────────────────────────────────────

/// Durable storage for the domain registry.
///
/// Records must survive a restart: a lost registry silently stops routing
/// every tenant's domain and re-orders every certificate.
pub trait CustomDomainStore: Send + Sync + std::fmt::Debug {
    /// Every stored record. Called once at boot to hydrate the index.
    fn load_all(&self) -> StoreFuture<'_, io::Result<Vec<CustomDomain>>>;
    /// Insert or replace one record.
    fn save<'a>(&'a self, domain: &'a CustomDomain) -> StoreFuture<'a, io::Result<()>>;
    /// Delete one record. Deleting an absent record succeeds.
    fn delete<'a>(&'a self, hostname: &'a str) -> StoreFuture<'a, io::Result<()>>;
}

/// In-memory [`CustomDomainStore`], for tests and for a deployment that
/// re-registers its domains from the app's own database at boot.
#[derive(Debug, Default)]
pub struct MemoryCustomDomainStore {
    records: RwLock<HashMap<String, CustomDomain>>,
}

impl MemoryCustomDomainStore {
    /// An empty store.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Every stored record, without an await — for assertions.
    #[cfg(any(test, feature = "test-support"))]
    #[must_use]
    pub fn load_all_blocking(&self) -> Vec<CustomDomain> {
        read_lock(&self.records).values().cloned().collect()
    }
}

impl CustomDomainStore for MemoryCustomDomainStore {
    fn load_all(&self) -> StoreFuture<'_, io::Result<Vec<CustomDomain>>> {
        Box::pin(async move { Ok(read_lock(&self.records).values().cloned().collect()) })
    }

    fn save<'a>(&'a self, domain: &'a CustomDomain) -> StoreFuture<'a, io::Result<()>> {
        Box::pin(async move {
            write_lock(&self.records).insert(domain.hostname.clone(), domain.clone());
            Ok(())
        })
    }

    fn delete<'a>(&'a self, hostname: &'a str) -> StoreFuture<'a, io::Result<()>> {
        Box::pin(async move {
            write_lock(&self.records).remove(hostname);
            Ok(())
        })
    }
}

/// Filesystem [`CustomDomainStore`]: one `0600` JSON file per domain under a
/// `0700` directory, mirroring `crate::acme::store::FsAcmeStore`.
///
/// A file per domain (rather than one index file) keeps a 1,000-domain
/// deployment's writes independent: registering or offboarding one tenant
/// rewrites one small file instead of the whole registry.
#[derive(Debug, Clone)]
pub struct FsCustomDomainStore {
    dir: PathBuf,
}

impl FsCustomDomainStore {
    /// A store rooted at `dir`.
    #[must_use]
    pub fn new(dir: impl Into<PathBuf>) -> Self {
        Self { dir: dir.into() }
    }

    /// The file holding `hostname`'s record.
    ///
    /// The hostname is hashed rather than used verbatim so a name that is
    /// legal in DNS but awkward on a filesystem cannot escape `dir`.
    fn path_for(&self, hostname: &str) -> PathBuf {
        self.dir.join(format!("{}.json", file_stem(hostname)))
    }
}

/// A short, filesystem-safe digest of `hostname`.
fn file_stem(hostname: &str) -> String {
    use sha2::{Digest as _, Sha256};
    let digest = Sha256::digest(hostname.as_bytes());
    let mut out = String::with_capacity(32);
    for byte in &digest[..16] {
        use std::fmt::Write as _;
        let _ = write!(out, "{byte:02x}");
    }
    out
}

impl CustomDomainStore for FsCustomDomainStore {
    fn load_all(&self) -> StoreFuture<'_, io::Result<Vec<CustomDomain>>> {
        Box::pin(async move {
            let mut entries = match tokio::fs::read_dir(&self.dir).await {
                Ok(entries) => entries,
                Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
                Err(e) => return Err(e),
            };
            let mut out = Vec::new();
            while let Some(entry) = entries.next_entry().await? {
                let path = entry.path();
                if path.extension().is_none_or(|ext| ext != "json") {
                    continue;
                }
                let bytes = tokio::fs::read(&path).await?;
                match serde_json::from_slice::<CustomDomain>(&bytes) {
                    Ok(domain) => out.push(domain),
                    // A corrupt record must not stop the other 999 domains from
                    // being served; it is logged and skipped, and the app can
                    // re-register the hostname to repair it.
                    Err(e) => tracing::warn!(
                        path = %path.display(),
                        "skipping unreadable custom-domain record: {e}"
                    ),
                }
            }
            Ok(out)
        })
    }

    fn save<'a>(&'a self, domain: &'a CustomDomain) -> StoreFuture<'a, io::Result<()>> {
        let dir = self.dir.clone();
        let path = self.path_for(&domain.hostname);
        Box::pin(async move {
            let bytes = serde_json::to_vec_pretty(domain)
                .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))?;
            crate::fs_atomic::blocking(move || {
                crate::fs_atomic::ensure_owner_only_dir(&dir)?;
                crate::fs_atomic::write_owner_only(&path, &bytes)
            })
            .await
        })
    }

    fn delete<'a>(&'a self, hostname: &'a str) -> StoreFuture<'a, io::Result<()>> {
        Box::pin(async move {
            match tokio::fs::remove_file(self.path_for(hostname)).await {
                Ok(()) => Ok(()),
                Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(()),
                Err(e) => Err(e),
            }
        })
    }
}

// ── The registry ─────────────────────────────────────────────────────────

/// Why a registration was refused.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RegisterError {
    /// The hostname is not a usable certificate subject.
    Invalid(String),
    /// The deployment's `max_domains` cap is reached.
    LimitReached {
        /// The configured cap.
        max: usize,
    },
    /// Another tenant already owns this hostname.
    Conflict {
        /// The tenant that owns it.
        tenant: String,
    },
    /// The deployment already owns this hostname, so no tenant may claim it.
    Reserved {
        /// The reserved pattern the hostname matched.
        pattern: String,
    },
    /// The record could not be persisted.
    Store(String),
}

impl std::fmt::Display for RegisterError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Invalid(msg) => write!(f, "invalid custom domain: {msg}"),
            Self::LimitReached { max } => write!(
                f,
                "custom-domain limit reached ({max}); raise \
                 [server.tls.acme.custom_domains] max_domains"
            ),
            Self::Conflict { tenant } => {
                write!(f, "hostname is already connected to tenant {tenant}")
            }
            Self::Reserved { pattern } => write!(
                f,
                "hostname is reserved by this deployment ({pattern}); a tenant cannot connect a \
                 name the operator already serves"
            ),
            Self::Store(msg) => write!(f, "failed to persist the custom domain: {msg}"),
        }
    }
}

impl std::error::Error for RegisterError {}

/// The hostname → tenant registry: an in-memory index over a durable store.
///
/// Reads (the ones on the request and handshake paths) take a read lock on a
/// `HashMap` and never touch the store, so 1,000 domains cost one hash lookup
/// per request. Writes go to the store first, then the index, so a persisted
/// registry never disagrees with what is being served.
#[derive(Debug)]
pub struct CustomDomainRegistry {
    store: Arc<dyn CustomDomainStore>,
    index: RwLock<HashMap<String, CustomDomain>>,
    /// Serialises the STORE half of every write, per hostname.
    ///
    /// Each write drops the index lock before awaiting the store — holding a
    /// `std` lock across an await is not an option — so without this two
    /// writers for the same hostname can commit out of order. The damaging
    /// order is a delete overtaken by an in-flight save: the index entry is
    /// gone, but the file is recreated, and the next restart hydrates a domain
    /// the app offboarded, which can then be re-issued and served.
    ///
    /// Keyed by hostname rather than global so a slow write for one tenant
    /// cannot stall every other tenant's.
    writes: tokio::sync::Mutex<HashMap<String, Arc<tokio::sync::Mutex<()>>>>,
    max_domains: usize,
    /// Hostnames and wildcard patterns no tenant may claim.
    reserved: Vec<String>,
    /// Whether [`load`](Self::load) has completed successfully.
    ///
    /// The retention prune deletes certificates for hostnames the registry
    /// does not know, so it must never run over an index that failed to
    /// hydrate: that index knows nothing, and every tenant certificate would
    /// read as an orphan.
    hydrated: std::sync::atomic::AtomicBool,
}

impl CustomDomainRegistry {
    /// A registry over `store`, capped at `max_domains`.
    #[must_use]
    pub fn new(store: Arc<dyn CustomDomainStore>, max_domains: usize) -> Self {
        Self {
            store,
            index: RwLock::new(HashMap::new()),
            writes: tokio::sync::Mutex::new(HashMap::new()),
            max_domains,
            reserved: Vec::new(),
            hydrated: std::sync::atomic::AtomicBool::new(false),
        }
    }

    /// Refuse registration of these hostnames and wildcard patterns.
    ///
    /// The deployment's own certificate names and tenancy base domain belong
    /// here. Without them a tenant can connect another tenant's subdomain —
    /// which the operator's own wildcard already points at this deployment, so
    /// it verifies and issues — and from then on every request for that host
    /// resolves to the tenant that registered it.
    ///
    /// A pattern reserves the name itself AND every name below it, wildcard or
    /// not: `myapp.com` and `*.myapp.com` each reserve `myapp.com` and
    /// `acme.myapp.com` alike.
    ///
    /// A wildcard covering its own apex is deliberate, and deliberately wider
    /// than the certificate: `*.myapp.com` does not cover `myapp.com` in TLS,
    /// but an operator who put that pattern here owns the zone, and letting a
    /// tenant claim the apex under it would hand away the operator's own name.
    /// Over-reserving costs an operator nothing — no tenant has a legitimate
    /// claim on a host the deployment already answers for — while
    /// under-reserving is a tenant-isolation hole.
    #[must_use]
    pub fn with_reserved(mut self, patterns: impl IntoIterator<Item = String>) -> Self {
        self.reserved = patterns
            .into_iter()
            .map(|p| p.trim().trim_end_matches('.').to_ascii_lowercase())
            .filter(|p| !p.is_empty())
            .collect();
        self
    }

    /// The reserved pattern `host` matches, if any.
    fn reserved_match(&self, host: &str) -> Option<String> {
        self.reserved
            .iter()
            .find(|pattern| {
                pattern.strip_prefix("*.").map_or_else(
                    // A bare name reserves itself AND everything under it: an
                    // operator listing `myapp.com` means the whole zone, not
                    // just the apex.
                    || host == *pattern || host.ends_with(&format!(".{pattern}")),
                    // `host == suffix` is intentional: a wildcard reserves its
                    // own apex too. Wider than the certificate `*.myapp.com`
                    // covers, and that is the point — the operator owns the
                    // zone, so a tenant must not be able to claim `myapp.com`
                    // just because only the wildcard was listed.
                    |suffix| host == suffix || host.ends_with(&format!(".{suffix}")),
                )
            })
            .cloned()
    }

    /// The registry an app is serving with, if custom domains are enabled.
    ///
    /// The one supported way for application code to reach it — registering a
    /// tenant hostname, rendering status, offboarding.
    #[must_use]
    pub fn from_state(state: &crate::AppState) -> Option<Arc<Self>> {
        state
            .extension::<Arc<Self>>()
            .map(|outer| Arc::clone(&*outer))
    }

    /// Whether the index has been hydrated from the store.
    #[must_use]
    pub fn is_hydrated(&self) -> bool {
        self.hydrated.load(std::sync::atomic::Ordering::Acquire)
    }

    /// Hydrate the index from the store. Returns how many records loaded.
    ///
    /// # Errors
    ///
    /// Propagates the store's read error.
    pub async fn load(&self) -> io::Result<usize> {
        let records = self.store.load_all().await?;
        let mut index = write_lock(&self.index);
        index.clear();
        for record in records {
            // A reservation can appear AFTER a domain was connected: the
            // operator adds a tenancy base domain, another name to the
            // deployment certificate, or an ingress hostname. `register`
            // refuses a reserved name, but the record written before that
            // change is still in the store, and hydrating it hands the
            // deployment's own hostname to whoever registered it first —
            // custom-domain lookup runs ahead of ordinary subdomain tenancy,
            // so the stale record wins every request from the restart on.
            //
            // Quarantined, not deleted: the record stays in the store, so
            // reverting the configuration restores the domain, while the
            // retention prune clears the certificate of a hostname the
            // registry no longer knows.
            if let Some(pattern) = self.reserved_match(&record.hostname) {
                tracing::error!(
                    hostname = %record.hostname,
                    tenant = %record.tenant,
                    pattern = %pattern,
                    "refusing to load a custom domain the configuration now reserves; offboard it \
                     or drop the reservation"
                );
                continue;
            }
            index.insert(record.hostname.clone(), record);
        }
        self.hydrated
            .store(true, std::sync::atomic::Ordering::Release);
        Ok(index.len())
    }

    /// Connect `hostname` to `tenant`, starting it at `PendingDns`.
    ///
    /// Re-registering the same hostname for the *same* tenant is idempotent —
    /// it returns the existing record rather than resetting a live domain back
    /// to pending, so an app that re-registers on every boot does not
    /// re-order every certificate.
    ///
    /// # Errors
    ///
    /// [`RegisterError`] for an unusable hostname, a full registry, a hostname
    /// another tenant owns, or a store write failure.
    pub async fn register(
        &self,
        hostname: &str,
        tenant: &str,
        now_unix: i64,
    ) -> Result<CustomDomain, RegisterError> {
        let host = normalize_hostname(hostname).map_err(RegisterError::Invalid)?;
        if tenant.trim().is_empty() {
            return Err(RegisterError::Invalid(
                "tenant id must not be empty".to_owned(),
            ));
        }
        if let Some(pattern) = self.reserved_match(&host) {
            return Err(RegisterError::Reserved { pattern });
        }

        // Refuse a full registry BEFORE creating this hostname's write gate.
        // The authoritative check is inside the gate below; this one exists so
        // that a tenant-facing endpoint hammered with unique hostnames cannot
        // mint a gate per attempt while every attempt is refused anyway.
        let full = {
            let index = read_lock(&self.index);
            index.len() >= self.max_domains && !index.contains_key(&host)
        };
        if full {
            return Err(RegisterError::LimitReached {
                max: self.max_domains,
            });
        }

        // Taken BEFORE the index is inspected, so the whole registration —
        // claim and save — is serialised against an offboard of the same
        // hostname. Claiming first and gating afterwards let a concurrent
        // `remove` erase the fresh entry and delete nothing, after which this
        // save still landed: the domain was absent from the running process but
        // came back at the next restart.
        let gate = self.write_gate(&host).await;
        let _write = gate.lock().await;

        // Claim the hostname in the index FIRST, under one write lock, and only
        // then persist. Writing the store first let a losing racer's record
        // reach disk anyway: the winner served in memory while the loser's
        // tenant came back after a restart, silently taking the hostname over.
        let record = {
            let mut index = write_lock(&self.index);
            if let Some(existing) = index.get(&host) {
                if existing.tenant == tenant {
                    return Ok(existing.clone());
                }
                return Err(RegisterError::Conflict {
                    tenant: existing.tenant.clone(),
                });
            }
            if index.len() >= self.max_domains {
                return Err(RegisterError::LimitReached {
                    max: self.max_domains,
                });
            }
            let record = CustomDomain::new(host.clone(), tenant.to_owned(), now_unix);
            index.insert(host.clone(), record.clone());
            record
        };

        if let Err(e) = self.store.save(&record).await {
            // Roll the claim back: a hostname that is not durable must not
            // route, or it would disappear on the next restart with no record
            // of why.
            write_lock(&self.index).remove(&host);
            return Err(RegisterError::Store(e.to_string()));
        }
        Ok(record)
    }

    /// The write gate for one hostname, creating it on first use.
    ///
    /// Gates outlive the operations that used them, so the map is swept once it
    /// grows past the registry's own cap: an `Arc` nobody but the map holds has
    /// no operation behind it and cannot be the one this caller is about to
    /// take. Without the sweep every hostname ever passed here — an offboarded
    /// domain, a refused registration — kept an entry for the life of the
    /// process, so a tenant-facing endpoint could grow this map without bound.
    async fn write_gate(&self, host: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut gates = self.writes.lock().await;
        if gates.len() >= self.gate_high_water() && !gates.contains_key(host) {
            gates.retain(|_, gate| Arc::strong_count(gate) > 1);
        }
        Arc::clone(gates.entry(host.to_owned()).or_default())
    }

    /// When to sweep idle write gates: the registry's cap plus room for the
    /// operations in flight over hostnames that are not registered (an
    /// offboard, a refused registration), so the sweep is rare rather than
    /// per-call.
    const fn gate_high_water(&self) -> usize {
        self.max_domains.saturating_add(64)
    }

    /// The record for `hostname` (normalising the lookup key), if registered.
    #[must_use]
    pub fn get(&self, hostname: &str) -> Option<CustomDomain> {
        let host = normalize_hostname(hostname).ok()?;
        read_lock(&self.index).get(&host).cloned()
    }

    /// The tenant a *servable* hostname belongs to.
    ///
    /// Returns `None` for an unregistered hostname and for one that has not
    /// reached `Active`, so a half-connected domain never routes.
    #[must_use]
    pub fn tenant_for_host(&self, hostname: &str) -> Option<String> {
        let host = normalize_hostname(hostname).ok()?;
        read_lock(&self.index)
            .get(&host)
            .filter(|d| d.is_servable())
            .map(|d| d.tenant.clone())
    }

    /// Is this hostname registered and serving?
    #[must_use]
    pub fn is_servable(&self, hostname: &str) -> bool {
        self.tenant_for_host(hostname).is_some()
    }

    /// Every record, sorted by hostname for a stable status listing.
    #[must_use]
    pub fn list(&self) -> Vec<CustomDomain> {
        let mut all: Vec<CustomDomain> = read_lock(&self.index).values().cloned().collect();
        all.sort_by(|a, b| a.hostname.cmp(&b.hostname));
        all
    }

    /// One tenant's domains, sorted by hostname.
    #[must_use]
    pub fn list_for_tenant(&self, tenant: &str) -> Vec<CustomDomain> {
        let mut all: Vec<CustomDomain> = read_lock(&self.index)
            .values()
            .filter(|d| d.tenant == tenant)
            .cloned()
            .collect();
        all.sort_by(|a, b| a.hostname.cmp(&b.hostname));
        all
    }

    /// How many domains are registered.
    #[must_use]
    pub fn len(&self) -> usize {
        read_lock(&self.index).len()
    }

    /// Is the registry empty?
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Domains still awaiting DNS confirmation and out of backoff.
    #[must_use]
    pub fn pending_verification(&self, now_unix: i64) -> Vec<CustomDomain> {
        self.filter(|d| d.status == DomainStatus::PendingDns && d.is_due(now_unix))
    }

    /// Verified domains with no certificate yet, out of backoff.
    ///
    /// `Issuing` is deliberately included: an order that crashed mid-flight
    /// would otherwise strand the domain forever.
    #[must_use]
    pub fn due_for_issuance(&self, now_unix: i64) -> Vec<CustomDomain> {
        self.filter(|d| {
            matches!(d.status, DomainStatus::Verified | DomainStatus::Issuing) && d.is_due(now_unix)
        })
    }

    /// Active domains inside their renew-before window, out of backoff.
    #[must_use]
    pub fn due_for_renewal(&self, now_unix: i64, renew_before_days: u32) -> Vec<CustomDomain> {
        self.filter(|d| {
            d.status == DomainStatus::Active
                && d.is_due(now_unix)
                && d.cert_not_after_unix
                    .is_some_and(|not_after| needs_renewal(not_after, renew_before_days, now_unix))
        })
    }

    fn filter(&self, predicate: impl Fn(&CustomDomain) -> bool) -> Vec<CustomDomain> {
        let mut out: Vec<CustomDomain> = read_lock(&self.index)
            .values()
            .filter(|d| predicate(d))
            .cloned()
            .collect();
        out.sort_by(|a, b| a.hostname.cmp(&b.hostname));
        out
    }

    /// Promote a domain to `Verified` and clear any failure.
    ///
    /// # Errors
    ///
    /// Propagates the store's write error.
    pub async fn record_verified(&self, hostname: &str, now_unix: i64) -> io::Result<()> {
        self.mutate(hostname, |d| {
            // A domain that is already serving stays serving: a re-verification
            // must never demote a live certificate back to `Verified` and
            // trigger a fresh order.
            if d.status == DomainStatus::PendingDns {
                d.status = DomainStatus::Verified;
            }
            if d.verified_at_unix.is_none() {
                d.verified_at_unix = Some(now_unix);
            }
            d.failure_reason = None;
            d.consecutive_failures = 0;
            d.next_attempt_unix = None;
        })
        .await
    }

    /// Mark an ACME order in flight.
    ///
    /// # Errors
    ///
    /// Propagates the store's write error.
    pub async fn record_issuing(&self, hostname: &str) -> io::Result<()> {
        self.mutate(hostname, |d| {
            if d.status == DomainStatus::Verified {
                d.status = DomainStatus::Issuing;
            }
        })
        .await
    }

    /// [`record_issuing`](Self::record_issuing), but only while `tenant` owns a
    /// hostname that is actually orderable. Returns whether it applied.
    ///
    /// Ordering waits for a lease, and the registry can move underneath that
    /// wait. An unconditional transition let the order run anyway: for a
    /// hostname that had been offboarded it spent a slot of the deployment's
    /// budget and the CA's rate limit on a certificate `install` would then
    /// discard, and for one re-registered by someone else it moved the NEW
    /// tenant's record to `Issuing` for an order that was never theirs.
    ///
    /// `Active` still applies — that is a renewal, and its record legitimately
    /// stays `Active` while the order runs — but `PendingDns` does not: a
    /// re-registration has not proven its own DNS yet.
    ///
    /// # Errors
    ///
    /// Propagates the store's write error.
    pub async fn record_issuing_for(&self, hostname: &str, tenant: &str) -> io::Result<bool> {
        self.mutate_if(
            hostname,
            |d| {
                d.tenant == tenant
                    && matches!(d.status, DomainStatus::Verified | DomainStatus::Active)
            },
            |d| {
                if d.status == DomainStatus::Verified {
                    d.status = DomainStatus::Issuing;
                }
            },
        )
        .await
    }

    /// Record a successfully issued certificate: the domain is now served.
    ///
    /// # Errors
    ///
    /// Propagates the store's write error.
    pub async fn record_active(
        &self,
        hostname: &str,
        now_unix: i64,
        cert_not_after_unix: i64,
    ) -> io::Result<()> {
        self.activate(hostname, None, now_unix, cert_not_after_unix)
            .await
            .map(|_| ())
    }

    /// [`record_active`](Self::record_active), but only while `tenant` still
    /// owns the hostname. Returns whether it applied.
    ///
    /// An issuance suspends at every `.await` between checking who owns a
    /// hostname and installing the certificate. If the owner is offboarded and
    /// the hostname re-registered in that window, an unconditional activation
    /// would stamp `Active` onto the NEW tenant's record — carrying it from
    /// `pending_dns` straight to serving, on a certificate issued for someone
    /// else and without its own DNS ever being verified. The owner is therefore
    /// re-asserted inside the write lock, not before the await.
    ///
    /// # Errors
    ///
    /// Propagates the store's write error.
    pub async fn record_active_for(
        &self,
        hostname: &str,
        tenant: &str,
        now_unix: i64,
        cert_not_after_unix: i64,
    ) -> io::Result<bool> {
        self.activate(hostname, Some(tenant), now_unix, cert_not_after_unix)
            .await
    }

    async fn activate(
        &self,
        hostname: &str,
        tenant: Option<&str>,
        now_unix: i64,
        cert_not_after_unix: i64,
    ) -> io::Result<bool> {
        self.mutate_if(
            hostname,
            |d| tenant.is_none_or(|t| d.tenant == t),
            |d| {
                d.status = DomainStatus::Active;
                if d.verified_at_unix.is_none() {
                    d.verified_at_unix = Some(now_unix);
                }
                if d.activated_at_unix.is_none() {
                    d.activated_at_unix = Some(now_unix);
                }
                d.cert_not_after_unix = Some(cert_not_after_unix);
                d.failure_reason = None;
                d.consecutive_failures = 0;
                d.next_attempt_unix = None;
            },
        )
        .await
    }

    /// Record a failure with the reason to show and how long to wait.
    ///
    /// An `Active` domain **keeps** its status: its certificate is still valid
    /// and still served, and a failed renewal must not take a tenant offline.
    ///
    /// # Errors
    ///
    /// Propagates the store's write error.
    pub async fn record_failure(
        &self,
        hostname: &str,
        now_unix: i64,
        reason: impl Into<String>,
        backoff_secs: i64,
    ) -> io::Result<()> {
        self.fail(hostname, None, now_unix, reason, backoff_secs)
            .await
            .map(|_| ())
    }

    /// [`record_failure`](Self::record_failure), but only while `tenant` still
    /// owns the hostname. Returns whether it applied.
    ///
    /// An order runs across several `.await`s. If the owner is offboarded and
    /// the hostname re-registered in that window, an unconditional failure
    /// would stamp one tenant's error and backoff onto the NEW tenant's record:
    /// a domain that has done nothing wrong would show someone else's reason
    /// and wait out a backoff it did not earn.
    ///
    /// # Errors
    ///
    /// Propagates the store's write error.
    pub async fn record_failure_for(
        &self,
        hostname: &str,
        tenant: &str,
        now_unix: i64,
        reason: impl Into<String>,
        backoff_secs: i64,
    ) -> io::Result<bool> {
        self.fail(hostname, Some(tenant), now_unix, reason, backoff_secs)
            .await
    }

    async fn fail(
        &self,
        hostname: &str,
        tenant: Option<&str>,
        now_unix: i64,
        reason: impl Into<String>,
        backoff_secs: i64,
    ) -> io::Result<bool> {
        let reason = reason.into();
        self.mutate_if(
            hostname,
            |d| tenant.is_none_or(|t| d.tenant == t),
            move |d| {
                if d.status == DomainStatus::Issuing {
                    // An order that failed goes back to `Verified`, not to
                    // `PendingDns`: DNS was already proven, and re-verifying
                    // would add a needless round trip to every retry.
                    d.status = DomainStatus::Verified;
                }
                d.failure_reason = Some(reason.clone());
                d.consecutive_failures = d.consecutive_failures.saturating_add(1);
                d.next_attempt_unix = Some(now_unix.saturating_add(backoff_secs));
            },
        )
        .await
    }

    /// Offboard one hostname: stop routing, stop serving, stop renewing.
    /// Returns whether a record was actually removed.
    ///
    /// # Errors
    ///
    /// Propagates the store's delete error.
    pub async fn remove(&self, hostname: &str) -> io::Result<bool> {
        self.remove_if(hostname, |_| true).await
    }

    /// [`remove`](Self::remove), but only while `guard` still holds for the
    /// stored record. Returns whether a record was actually removed.
    ///
    /// The guard runs inside the per-hostname write gate, so a caller that
    /// decided to offboard a domain BEFORE an `.await` re-asserts that decision
    /// against the record as it is now. The retention sweep needs exactly this:
    /// it picks its candidates from a `list()` snapshot, and a domain that
    /// finished verifying and issuing while the sweep was awaiting an earlier
    /// offboard would otherwise be deleted — certificate and all — moments
    /// after it came up.
    ///
    /// # Errors
    ///
    /// Propagates the store's delete error.
    pub async fn remove_if(
        &self,
        hostname: &str,
        guard: impl FnOnce(&CustomDomain) -> bool,
    ) -> io::Result<bool> {
        let Ok(host) = normalize_hostname(hostname) else {
            return Ok(false);
        };
        // Delete from the store first: a crash between the two leaves a record
        // that `load` re-hydrates, which is recoverable — the reverse leaves a
        // hostname that routes but has no durable record.
        //
        // Under the same gate as every other write for this hostname, so an
        // in-flight save cannot land after the delete and resurrect the record
        // at the next restart.
        let gate = self.write_gate(&host).await;
        let _write = gate.lock().await;
        if !read_lock(&self.index).get(&host).is_none_or(guard) {
            return Ok(false);
        }
        self.store.delete(&host).await?;
        Ok(write_lock(&self.index).remove(&host).is_some())
    }

    /// Offboard every domain a tenant owns. Returns how many were removed.
    ///
    /// # Errors
    ///
    /// Propagates the store's delete error.
    pub async fn remove_tenant(&self, tenant: &str) -> io::Result<usize> {
        let hostnames: Vec<String> = self
            .list_for_tenant(tenant)
            .into_iter()
            .map(|d| d.hostname)
            .collect();
        let mut removed = 0;
        for hostname in hostnames {
            // Re-assert the tenant per hostname: the list is a snapshot and
            // each removal awaits, so a hostname freed early can be
            // re-registered by ANOTHER tenant before a later one runs. An
            // unconditional removal would delete that tenant's record and stop
            // routing a domain that was never part of this teardown.
            if self
                .remove_if(&hostname, |current| current.tenant == tenant)
                .await?
            {
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// A one-line-per-domain report of everything unhealthy, naming the
    /// hostname AND its tenant so an operator can act without a lookup.
    ///
    /// Empty when every domain is healthy.
    #[must_use]
    pub fn health_report(&self, now_unix: i64) -> String {
        let mut lines: Vec<String> = Vec::new();
        for domain in self.list() {
            let expiring = domain.cert_not_after_unix.is_some_and(|na| na <= now_unix);
            if let Some(reason) = &domain.failure_reason {
                lines.push(format!(
                    "{} (tenant {}, {}): {reason}",
                    domain.hostname, domain.tenant, domain.status
                ));
            } else if expiring {
                lines.push(format!(
                    "{} (tenant {}): certificate expired",
                    domain.hostname, domain.tenant
                ));
            }
        }
        lines.join("; ")
    }

    /// Apply `f` to a record and persist it. A missing hostname is a no-op.
    async fn mutate(&self, hostname: &str, f: impl FnOnce(&mut CustomDomain)) -> io::Result<()> {
        self.mutate_if(hostname, |_| true, f).await.map(|_| ())
    }

    /// Apply `f` to a record and persist it, but only while `guard` accepts the
    /// record as it stands under the write lock. Returns whether it applied.
    ///
    /// The guard is what makes a check-then-write atomic against the registry:
    /// a caller that verified something about the record BEFORE an `.await`
    /// re-asserts it here, inside the lock, rather than trusting a fact that
    /// may have expired while it was suspended.
    async fn mutate_if(
        &self,
        hostname: &str,
        guard: impl FnOnce(&CustomDomain) -> bool,
        f: impl FnOnce(&mut CustomDomain),
    ) -> io::Result<bool> {
        let Ok(host) = normalize_hostname(hostname) else {
            return Ok(false);
        };
        let gate = self.write_gate(&host).await;
        let _write = gate.lock().await;
        // Apply to a COPY, persist, and only then publish. Mutating the live
        // index first left a state the store had rejected routing and serving
        // anyway: `record_active` reported failure while the domain went on
        // being served from it, and the state then vanished at the next
        // restart with nothing to explain either half. Every writer for this
        // hostname holds this same gate, so nothing can change the record
        // between the copy and the publish.
        let Some(current) = read_lock(&self.index).get(&host).cloned() else {
            return Ok(false);
        };
        if !guard(&current) {
            return Ok(false);
        }
        let mut updated = current;
        f(&mut updated);
        self.store.save(&updated).await?;
        let mut index = write_lock(&self.index);
        // An offboard cannot interleave under the gate, but the index is the
        // routing table: if the record is gone, leave it gone rather than
        // resurrecting a hostname nothing owns.
        Ok(index.get_mut(&host).is_some_and(|slot| {
            *slot = updated;
            true
        }))
    }
}

// ── Issuance budget ──────────────────────────────────────────────────────

/// Is a certificate expiring at `not_after_unix` inside its renew-before
/// window at `now_unix`?
///
/// Mirrors `crate::acme::renewal::needs_renewal` but compiles without the
/// `acme` feature, so the registry's renewal scheduling is testable in a
/// default build.
#[must_use]
pub const fn needs_renewal(not_after_unix: i64, renew_before_days: u32, now_unix: i64) -> bool {
    let window = (renew_before_days as i64).saturating_mul(86_400);
    not_after_unix.saturating_sub(now_unix) < window
}

/// The retry delay after `consecutive_failures` failures: `base * 2^(n-1)`,
/// capped at `max`.
///
/// Saturating throughout, so an absurd failure count returns the cap rather
/// than overflowing.
#[must_use]
pub const fn backoff_secs(consecutive_failures: u32, base: u64, max: u64) -> u64 {
    if consecutive_failures == 0 {
        return 0;
    }
    let shift = if consecutive_failures > 32 {
        32
    } else {
        consecutive_failures - 1
    };
    let scaled = base.saturating_mul(1_u64 << shift);
    if scaled > max { max } else { scaled }
}

/// Whether an issuance attempt may proceed now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum IssuanceDecision {
    /// Go ahead.
    Allow,
    /// This domain has spent its daily budget.
    PerDomainLimit {
        /// Seconds until the domain's window rolls.
        retry_after_secs: i64,
    },
    /// The deployment has spent its hourly budget.
    GlobalLimit {
        /// Seconds until the global window rolls.
        retry_after_secs: i64,
    },
}

impl IssuanceDecision {
    /// May the caller order now?
    #[must_use]
    pub const fn is_allowed(&self) -> bool {
        matches!(self, Self::Allow)
    }

    /// The reason to record on the domain, if the attempt was refused.
    #[must_use]
    pub fn reason(&self) -> Option<String> {
        match self {
            Self::Allow => None,
            Self::PerDomainLimit { retry_after_secs } => Some(format!(
                "issuance budget for this domain is spent; retrying in {retry_after_secs}s"
            )),
            Self::GlobalLimit { retry_after_secs } => Some(format!(
                "deployment-wide issuance budget is spent; retrying in {retry_after_secs}s"
            )),
        }
    }
}

/// The window a per-domain issuance budget is measured over: Let's Encrypt
/// counts duplicate certificates per week, so a daily cap is the conservative
/// sub-multiple.
const PER_DOMAIN_WINDOW_SECS: i64 = 86_400;
/// The window the deployment-wide budget is measured over.
const GLOBAL_WINDOW_SECS: i64 = 3_600;

/// Per-domain and deployment-wide caps on ACME orders.
///
/// The abuse posture this enforces (see `docs/guide/tls.md`): a tenant whose
/// domain fails repeatedly costs at most `per_domain_per_day` orders a day, and
/// the whole deployment costs at most `global_per_hour` — well inside Let's
/// Encrypt's 300-new-orders-per-account-per-3-hours limit at the defaults.
/// Failures additionally back off exponentially, so a permanently broken domain
/// converges to one attempt per `max_backoff_secs`.
#[derive(Debug)]
pub struct IssuanceLimiter {
    per_domain_per_day: u32,
    global_per_hour: u32,
    base_backoff_secs: u64,
    max_backoff_secs: u64,
    attempts: RwLock<Attempts>,
}

#[derive(Debug, Default)]
struct Attempts {
    /// Attempt timestamps per domain, within the per-domain window.
    per_domain: HashMap<String, Vec<i64>>,
    /// Attempt timestamps across all domains, within the global window.
    global: Vec<i64>,
}

impl IssuanceLimiter {
    /// A limiter with the given budgets and backoff bounds.
    #[must_use]
    pub fn new(
        per_domain_per_day: u32,
        global_per_hour: u32,
        base_backoff_secs: u64,
        max_backoff_secs: u64,
    ) -> Self {
        Self {
            per_domain_per_day,
            global_per_hour,
            base_backoff_secs,
            max_backoff_secs,
            attempts: RwLock::new(Attempts::default()),
        }
    }

    /// The longest backoff this limiter will impose.
    #[must_use]
    pub const fn max_backoff(&self) -> u64 {
        self.max_backoff_secs
    }

    /// The backoff delay for a domain with `consecutive_failures` failures.
    #[must_use]
    pub const fn backoff_for(&self, consecutive_failures: u32) -> u64 {
        backoff_secs(
            consecutive_failures,
            self.base_backoff_secs,
            self.max_backoff_secs,
        )
    }

    /// May `hostname` be ordered now, as far as the *budgets* are concerned?
    ///
    /// Deliberately says nothing about failure backoff: the retry deadline
    /// lives on the domain record ([`CustomDomain::next_attempt_unix`], set
    /// from [`Self::backoff_for`]) and is enforced by
    /// [`CustomDomain::is_due`]. A failure count alone cannot say the wait is
    /// over, so a second copy of the deadline here would refuse every retry
    /// forever.
    ///
    /// The domain's own budget is checked before the deployment's, so the
    /// reason reported is the most specific one.
    #[must_use]
    pub fn check(&self, hostname: &str, now_unix: i64) -> IssuanceDecision {
        // Both windows are read under ONE guard, then the guard is released
        // before any decision is built: a caller must not hold the lock while
        // formatting a message, and the two counts must come from the same
        // instant or a domain could pass its own budget against a global count
        // taken after another thread spent it.
        let (domain_hits, global_hits) = {
            let attempts = read_lock(&self.attempts);
            let domain_hits = attempts
                .per_domain
                .get(hostname)
                .map(|hits| within(hits, now_unix, PER_DOMAIN_WINDOW_SECS))
                .unwrap_or_default();
            let global_hits = within(&attempts.global, now_unix, GLOBAL_WINDOW_SECS);
            drop(attempts);
            (domain_hits, global_hits)
        };
        if domain_hits.len() >= self.per_domain_per_day as usize {
            return IssuanceDecision::PerDomainLimit {
                retry_after_secs: retry_after(&domain_hits, now_unix, PER_DOMAIN_WINDOW_SECS),
            };
        }
        if global_hits.len() >= self.global_per_hour as usize {
            return IssuanceDecision::GlobalLimit {
                retry_after_secs: retry_after(&global_hits, now_unix, GLOBAL_WINDOW_SECS),
            };
        }
        IssuanceDecision::Allow
    }

    /// Record that an order was placed for `hostname`.
    pub fn record_attempt(&self, hostname: &str, now_unix: i64) {
        let mut attempts = write_lock(&self.attempts);
        let global = within(&attempts.global, now_unix, GLOBAL_WINDOW_SECS);
        attempts.global = global;
        attempts.global.push(now_unix);
        let hits = attempts.per_domain.entry(hostname.to_owned()).or_default();
        let mut kept = within(hits, now_unix, PER_DOMAIN_WINDOW_SECS);
        kept.push(now_unix);
        *hits = kept;
        drop(attempts);
    }

    /// Drop a domain's attempt history — called when it is offboarded, so a
    /// re-registration of the same hostname is not charged for the old one.
    pub fn forget(&self, hostname: &str) {
        write_lock(&self.attempts).per_domain.remove(hostname);
    }
}

/// The timestamps in `hits` still inside `window` at `now_unix`.
fn within(hits: &[i64], now_unix: i64, window: i64) -> Vec<i64> {
    let cutoff = now_unix.saturating_sub(window);
    hits.iter().copied().filter(|at| *at > cutoff).collect()
}

/// Seconds until the oldest hit in `hits` leaves `window`.
fn retry_after(hits: &[i64], now_unix: i64, window: i64) -> i64 {
    hits.iter()
        .min()
        .map_or(0, |oldest| (*oldest + window - now_unix).max(1))
}

// ── Issuance seam ────────────────────────────────────────────────────────

/// A newly issued certificate: PEM chain (leaf first) plus its private key.
///
/// Deliberately independent of the `acme` feature so the orchestration and its
/// tests compile in a default build; the ACME adapter converts to and from
/// `crate::acme::store::StoredCert`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IssuedCertificate {
    /// PEM certificate chain, leaf first.
    pub chain_pem: String,
    /// PEM private key matching the leaf.
    pub key_pem: String,
}

/// Obtains a certificate for one hostname.
///
/// The seam between the orchestration in this module and the ACME client, so
/// the state machine, the rate limits and the verification gate are testable
/// without a CA.
pub trait DomainIssuer: Send + Sync {
    /// Order (or renew) a certificate covering exactly `hostname`.
    fn issue<'a>(
        &'a self,
        hostname: &'a str,
    ) -> futures::future::BoxFuture<'a, Result<IssuedCertificate, String>>;
}

/// Looks up where a hostname currently points.
///
/// Implemented by the runtime against the system resolver, and by tests
/// against a fixed table.
pub trait DomainVerifier: Send + Sync {
    /// Observe `hostname`'s current DNS target.
    fn observe<'a>(&'a self, hostname: &'a str) -> futures::future::BoxFuture<'a, ObservedTarget>;
}

/// A [`DomainVerifier`] backed by the system resolver.
///
/// Resolves addresses only: `getaddrinfo` follows CNAMEs transparently and
/// does not report them, so a tenant who publishes the suggested CNAME is
/// verified by the addresses it resolves to — which is the property that
/// actually matters.
#[derive(Debug, Default, Clone, Copy)]
pub struct SystemDomainVerifier;

impl DomainVerifier for SystemDomainVerifier {
    fn observe<'a>(&'a self, hostname: &'a str) -> futures::future::BoxFuture<'a, ObservedTarget> {
        Box::pin(async move {
            let host = hostname.to_owned();
            // `getaddrinfo` blocks; keep it off the async worker.
            let resolved = tokio::task::spawn_blocking(move || {
                use std::net::ToSocketAddrs as _;
                (host.as_str(), 0_u16)
                    .to_socket_addrs()
                    .map(|addrs| addrs.map(|s| s.ip()).collect::<Vec<_>>())
            })
            .await;
            match resolved {
                Ok(Ok(addrs)) if !addrs.is_empty() => ObservedTarget::Addresses(addrs),
                _ => ObservedTarget::None,
            }
        })
    }
}

// ── Health ───────────────────────────────────────────────────────────────

/// The `custom_domains` health indicator (AC5).
///
/// `Down` when any domain's certificate has actually expired — that domain is
/// no longer servable. A domain merely carrying a `failure_reason` (a retry is
/// pending, its certificate still valid) is `Up` with the detail attached: one
/// tenant's failed renewal is not a deployment outage, and grading it as one
/// would make the signal useless at a thousand domains.
///
/// Every unhealthy domain is named with its tenant, so an operator reading
/// `/actuator/health` can act without a registry lookup.
#[derive(Debug)]
pub struct CustomDomainHealthIndicator {
    registry: Arc<CustomDomainRegistry>,
    now_unix: fn() -> i64,
}

impl CustomDomainHealthIndicator {
    /// An indicator over `registry`.
    #[must_use]
    pub fn new(registry: Arc<CustomDomainRegistry>) -> Self {
        Self { registry, now_unix }
    }

    /// Grade the registry at `now_unix` (pure; used by `check` and tests).
    #[must_use]
    pub fn grade(&self, now_unix: i64) -> crate::actuator::HealthCheckOutput {
        use crate::actuator::{HealthCheckOutput, HealthStatus};
        let mut details = std::collections::HashMap::new();
        let all = self.registry.list();
        let active = all.iter().filter(|d| d.is_servable()).count();
        details.insert("registered".to_owned(), serde_json::json!(all.len()));
        details.insert("active".to_owned(), serde_json::json!(active));

        let expired: Vec<String> = all
            .iter()
            .filter(|d| d.cert_not_after_unix.is_some_and(|na| na <= now_unix))
            .map(|d| format!("{} (tenant {})", d.hostname, d.tenant))
            .collect();
        let failing: Vec<String> = all
            .iter()
            .filter(|d| d.failure_reason.is_some())
            .map(|d| {
                format!(
                    "{} (tenant {}): {}",
                    d.hostname,
                    d.tenant,
                    d.failure_reason.as_deref().unwrap_or_default()
                )
            })
            .collect();
        if !failing.is_empty() {
            details.insert("failing".to_owned(), serde_json::json!(failing));
        }
        if !expired.is_empty() {
            details.insert("expired".to_owned(), serde_json::json!(expired));
        }

        HealthCheckOutput {
            status: if expired.is_empty() {
                HealthStatus::Up
            } else {
                HealthStatus::Down
            },
            details,
        }
    }
}

impl crate::actuator::HealthIndicator for CustomDomainHealthIndicator {
    fn check(&self) -> futures::future::BoxFuture<'_, crate::actuator::HealthCheckOutput> {
        let now = (self.now_unix)();
        Box::pin(async move { self.grade(now) })
    }

    fn group(&self) -> crate::actuator::IndicatorGroup {
        crate::actuator::IndicatorGroup::HealthOnly
    }
}

/// Wall-clock seconds since the epoch.
///
/// The one reading in the custom-domain path; the orchestrator ticks from it
/// too, so a status and the decision that produced it never disagree by a
/// second.
pub(crate) fn now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

// ── Retention ────────────────────────────────────────────────────────────

/// Prunes the custom-domain datasets the framework owns (#1605 / #1635 AC7).
///
/// The seam lives here, in the core module, so the retention engine can drive
/// it without the `acme` feature; the implementation that actually knows about
/// certificates is `crate::acme::tenant_domains::CustomDomainTask`.
pub trait CustomDomainPruner: Send + Sync {
    /// Delete what the retention policy no longer keeps and return how many
    /// records went (or would go, when `dry_run`).
    ///
    /// Two things are pruned: registry records still awaiting DNS since before
    /// `cutoff_unix` — a tenant who was handed instructions and never followed
    /// them — and stored certificates for hostnames no longer registered,
    /// which nothing would otherwise ever delete.
    fn prune(
        &self,
        cutoff_unix: i64,
        dry_run: bool,
    ) -> futures::future::BoxFuture<'_, Result<u64, String>>;

    /// Offboard one hostname completely: stop routing and serving, halt
    /// renewal, and delete the stored certificate and its private key.
    ///
    /// [`CustomDomainRegistry::remove`] only forgets the registration; the
    /// certificate lives in the ACME store, which the registry cannot reach.
    /// This is the offboarding an application should call.
    fn offboard_domain<'a>(
        &'a self,
        hostname: &'a str,
    ) -> futures::future::BoxFuture<'a, io::Result<bool>>;

    /// Offboard every domain a tenant owns. Returns how many went.
    fn offboard_tenant_domains<'a>(
        &'a self,
        tenant: &'a str,
    ) -> futures::future::BoxFuture<'a, io::Result<usize>>;
}

impl dyn CustomDomainPruner {
    /// The offboarding surface an app is serving with, if custom domains are
    /// enabled.
    #[must_use]
    pub fn from_state(state: &crate::AppState) -> Option<Arc<Self>> {
        state
            .extension::<Arc<Self>>()
            .map(|outer| Arc::clone(&*outer))
    }
}

// ── SNI certificate selection ────────────────────────────────────────────

#[cfg(feature = "tls")]
mod sni {
    use super::{CustomDomainRegistry, read_lock, write_lock};
    use rustls::server::{ClientHello, ResolvesServerCert};
    use rustls::sign::CertifiedKey;
    use std::collections::{HashMap, VecDeque};
    use std::sync::{Arc, RwLock};

    /// A bounded, insertion-ordered cache of per-domain certificates.
    ///
    /// AC6's "certificates load incrementally": a 1,000-domain deployment
    /// holds at most `capacity` parsed certificates in memory and re-reads the
    /// rest from the store on demand, so correctness never depends on every
    /// certificate being resident at boot.
    ///
    /// Eviction is first-in-first-out rather than least-recently-used: FIFO
    /// needs no write on the read path, which is the TLS handshake path, and
    /// the cost of an eviction is one file read.
    #[derive(Debug)]
    pub struct CustomDomainCertCache {
        capacity: usize,
        inner: RwLock<CacheInner>,
    }

    #[derive(Debug, Default)]
    struct CacheInner {
        certs: HashMap<String, Arc<CertifiedKey>>,
        order: VecDeque<String>,
    }

    impl CustomDomainCertCache {
        /// A cache holding at most `capacity` certificates (minimum 1).
        #[must_use]
        pub fn new(capacity: usize) -> Self {
            Self {
                capacity: capacity.max(1),
                inner: RwLock::new(CacheInner::default()),
            }
        }

        /// The cached certificate for `hostname`, if resident.
        #[must_use]
        pub fn get(&self, hostname: &str) -> Option<Arc<CertifiedKey>> {
            read_lock(&self.inner).certs.get(hostname).cloned()
        }

        /// Cache `key` for `hostname`, evicting the oldest entry if full.
        pub fn insert(&self, hostname: &str, key: Arc<CertifiedKey>) {
            let mut inner = write_lock(&self.inner);
            if inner.certs.insert(hostname.to_owned(), key).is_none() {
                inner.order.push_back(hostname.to_owned());
            }
            while inner.order.len() > self.capacity {
                if let Some(oldest) = inner.order.pop_front() {
                    inner.certs.remove(&oldest);
                }
            }
            drop(inner);
        }

        /// Drop `hostname`'s certificate — called on offboarding, so a removed
        /// domain stops being served without waiting for eviction.
        pub fn remove(&self, hostname: &str) {
            let mut inner = write_lock(&self.inner);
            inner.certs.remove(hostname);
            inner.order.retain(|h| h != hostname);
        }

        /// The most certificates this cache will hold.
        #[must_use]
        pub const fn capacity(&self) -> usize {
            self.capacity
        }

        /// How many certificates are resident.
        #[must_use]
        pub fn len(&self) -> usize {
            read_lock(&self.inner).certs.len()
        }

        /// Is the cache empty?
        #[must_use]
        pub fn is_empty(&self) -> bool {
            self.len() == 0
        }
    }

    /// Selects the certificate for a TLS handshake by SNI.
    ///
    /// Three outcomes, in order:
    ///
    /// 1. the SNI name is covered by the operator's own certificate → serve it
    ///    (unchanged #1603/#1608 behaviour),
    /// 2. the name is a registered, `Active` custom domain with a resident
    ///    certificate → serve that,
    /// 3. anything else → **refuse the handshake**.
    ///
    /// (3) is the abuse gate AC4 asks for: an attacker pointing DNS at this
    /// deployment and opening a handshake for an unregistered name gets a TLS
    /// alert, and no code path from here reaches the ACME provider.
    ///
    /// A registered domain whose certificate is not resident is loaded through
    /// the optional [`SniCertSource`] — the other half of AC6's incremental
    /// loading. Without it, a deployment with more domains than
    /// `cert_cache_size` would stop serving every domain past the cache after a
    /// restart.
    #[derive(Debug)]
    pub struct SniCertResolver {
        base: Arc<crate::tls::ReloadableCertResolver>,
        base_names: Vec<String>,
        registry: Arc<CustomDomainRegistry>,
        cache: Arc<CustomDomainCertCache>,
        source: Option<Arc<dyn SniCertSource>>,
    }

    /// Loads one domain's certificate on the handshake path.
    ///
    /// Deliberately **synchronous**: `rustls` resolves a certificate inside the
    /// handshake, which is not an async context. The implementation reads two
    /// small files, and only on a cache miss for a hostname already proven
    /// registered — never for an attacker-supplied name — so the blocking cost
    /// is bounded by the eviction rate, not by request volume.
    pub trait SniCertSource: Send + Sync + std::fmt::Debug {
        /// The stored certificate for `hostname`, if one loads.
        fn load(&self, hostname: &str) -> Option<Arc<CertifiedKey>>;
    }

    impl SniCertResolver {
        /// A resolver over the operator's certificate (`base`, covering
        /// `base_names`) plus the tenant registry and its certificate cache.
        #[must_use]
        pub fn new(
            base: Arc<crate::tls::ReloadableCertResolver>,
            base_names: Vec<String>,
            registry: Arc<CustomDomainRegistry>,
            cache: Arc<CustomDomainCertCache>,
        ) -> Self {
            Self {
                base,
                base_names: base_names
                    .into_iter()
                    .map(|n| n.trim().to_ascii_lowercase())
                    .collect(),
                registry,
                cache,
                source: None,
            }
        }

        /// Load a registered domain's certificate from `source` on a cache miss.
        #[must_use]
        pub fn with_source(mut self, source: Arc<dyn SniCertSource>) -> Self {
            self.source = Some(source);
            self
        }

        /// The certificate to serve for `server_name`, or `None` to refuse.
        #[must_use]
        pub fn certificate_for(&self, server_name: &str) -> Option<Arc<CertifiedKey>> {
            let name = server_name.trim_end_matches('.').to_ascii_lowercase();
            if self.base_covers(&name) {
                return Some(self.base.current());
            }
            // The registry check comes BEFORE any I/O: an unregistered name
            // must cost nothing but a hash lookup, however many are thrown at
            // the listener.
            if !self.registry.is_servable(&name) {
                return None;
            }
            if let Some(cached) = self.cache.get(&name) {
                return Some(cached);
            }
            let loaded = self.source.as_ref()?.load(&name)?;
            self.cache.insert(&name, Arc::clone(&loaded));
            Some(loaded)
        }

        /// Does the operator's own certificate cover `name`?
        fn base_covers(&self, name: &str) -> bool {
            self.base_names.iter().any(|pattern| {
                pattern
                    .strip_prefix("*.")
                    .map_or(pattern == name, |suffix| {
                        // A wildcard matches exactly one label, per RFC 6125.
                        name.strip_suffix(suffix)
                            .and_then(|prefix| prefix.strip_suffix('.'))
                            .is_some_and(|label| !label.is_empty() && !label.contains('.'))
                    })
            })
        }
    }

    impl ResolvesServerCert for SniCertResolver {
        fn resolve(&self, client_hello: ClientHello<'_>) -> Option<Arc<CertifiedKey>> {
            client_hello.server_name().map_or_else(
                // No SNI at all (an IP-address client, or an old one): serve the
                // operator's certificate, exactly as before custom domains.
                || Some(self.base.current()),
                |name| self.certificate_for(name),
            )
        }
    }
}

#[cfg(feature = "tls")]
pub use sni::{CustomDomainCertCache, SniCertResolver, SniCertSource};

// ── Lock helpers ─────────────────────────────────────────────────────────

/// Read a lock, recovering from poisoning.
///
/// A panic while holding one of these locks leaves the registry index
/// readable and internally consistent (every mutation is a whole-record
/// replacement), so refusing to serve every tenant afterwards would be a
/// worse outcome than continuing.
fn read_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    lock.read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Write a lock, recovering from poisoning. See [`read_lock`].
fn write_lock<T>(lock: &RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    lock.write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The write-gate map must stay bounded: every hostname ever gated used to
    /// keep an entry for the life of the process, and two paths made that
    /// unbounded rather than merely bounded by the registry — a refused
    /// registration (the gate was minted before the cap was checked) and an
    /// offboarded domain, whose gate outlived it.
    #[tokio::test]
    async fn write_gates_are_bounded_by_the_registry_not_by_hostnames_ever_seen() {
        let registry = CustomDomainRegistry::new(Arc::new(MemoryCustomDomainStore::new()), 2);
        for host in ["a.clientco.com", "b.clientco.com"] {
            registry.register(host, "tenant-a", 0).await.unwrap();
        }

        // A tenant-facing endpoint hammered with unique hostnames against a
        // full registry mints nothing: the cap is checked before the gate.
        for i in 0..500 {
            let host = format!("flood-{i}.clientco.com");
            assert!(matches!(
                registry.register(&host, "tenant-evil", 0).await,
                Err(RegisterError::LimitReached { .. })
            ));
        }
        assert_eq!(
            registry.writes.lock().await.len(),
            2,
            "a refused registration must not leave a gate behind"
        );

        // And a long churn of register/offboard cycles is swept rather than
        // keeping one entry per hostname ever seen.
        for host in ["a.clientco.com", "b.clientco.com"] {
            assert!(registry.remove(host).await.unwrap());
        }
        for i in 0..300 {
            let host = format!("churn-{i}.clientco.com");
            registry.register(&host, "tenant-a", 0).await.unwrap();
            assert!(registry.remove(&host).await.unwrap());
        }
        let gates = registry.writes.lock().await.len();
        assert!(
            gates <= registry.gate_high_water(),
            "idle gates must be swept, but {gates} are held"
        );
    }

    #[test]
    fn apex_detection() {
        assert!(is_apex("clientco.com"));
        assert!(!is_apex("app.clientco.com"));
        assert!(!is_apex("a.b.clientco.com"));
    }

    #[test]
    fn backoff_doubles_and_caps() {
        assert_eq!(backoff_secs(0, 300, 3600), 0);
        assert_eq!(backoff_secs(1, 300, 3600), 300);
        assert_eq!(backoff_secs(4, 300, 3600), 2400);
        assert_eq!(backoff_secs(5, 300, 3600), 3600);
        assert_eq!(backoff_secs(u32::MAX, 300, 3600), 3600);
    }

    #[test]
    fn a_cname_target_is_compared_case_and_dot_insensitively() {
        let expected = ExpectedIngress {
            hostname: Some("Ingress.MyApp.com".to_owned()),
            ..ExpectedIngress::default()
        };
        assert_eq!(
            grade_dns_verification(
                &ObservedTarget::Cname("ingress.myapp.com.".to_owned()),
                &expected
            ),
            VerificationOutcome::PointsHere
        );
    }

    #[test]
    fn a_partial_address_match_is_not_a_pass() {
        let expected = ExpectedIngress {
            ipv4: vec!["203.0.113.10".parse().unwrap()],
            ..ExpectedIngress::default()
        };
        let observed = ObservedTarget::Addresses(vec![
            "203.0.113.10".parse().unwrap(),
            "198.51.100.7".parse().unwrap(),
        ]);
        match grade_dns_verification(&observed, &expected) {
            VerificationOutcome::PointsElsewhere { detail } => {
                assert!(detail.contains("198.51.100.7"), "{detail}");
                assert!(!detail.contains("203.0.113.10"), "{detail}");
            }
            other => panic!("expected a mismatch, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn the_filesystem_store_round_trips_and_deletes() {
        let dir = tempfile::tempdir().unwrap();
        let store = FsCustomDomainStore::new(dir.path());
        let domain = CustomDomain::new("app.clientco.com".to_owned(), "t1".to_owned(), 100);
        store.save(&domain).await.unwrap();

        let loaded = store.load_all().await.unwrap();
        assert_eq!(loaded, vec![domain.clone()]);

        store.delete("app.clientco.com").await.unwrap();
        assert!(store.load_all().await.unwrap().is_empty());
        // Deleting again is not an error.
        store.delete("app.clientco.com").await.unwrap();
    }

    #[tokio::test]
    async fn a_registry_hydrates_from_its_store() {
        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(FsCustomDomainStore::new(dir.path()));
        let first = CustomDomainRegistry::new(store.clone(), 10);
        first.register("app.clientco.com", "t1", 100).await.unwrap();
        first
            .record_active("app.clientco.com", 100, 200)
            .await
            .unwrap();

        let second = CustomDomainRegistry::new(store, 10);
        assert_eq!(second.load().await.unwrap(), 1);
        assert_eq!(
            second.tenant_for_host("app.clientco.com").as_deref(),
            Some("t1")
        );
    }

    #[tokio::test]
    async fn re_registering_the_same_pair_is_idempotent() {
        let registry = CustomDomainRegistry::new(Arc::new(MemoryCustomDomainStore::new()), 10);
        registry
            .register("app.clientco.com", "t1", 100)
            .await
            .unwrap();
        registry
            .record_active("app.clientco.com", 100, 200)
            .await
            .unwrap();
        let again = registry
            .register("app.clientco.com", "t1", 500)
            .await
            .unwrap();
        assert_eq!(
            again.status,
            DomainStatus::Active,
            "re-registering must not reset a live domain"
        );
    }

    #[tokio::test]
    async fn a_failed_order_returns_to_verified_not_pending() {
        let registry = CustomDomainRegistry::new(Arc::new(MemoryCustomDomainStore::new()), 10);
        registry
            .register("app.clientco.com", "t1", 100)
            .await
            .unwrap();
        registry
            .record_verified("app.clientco.com", 100)
            .await
            .unwrap();
        registry.record_issuing("app.clientco.com").await.unwrap();
        registry
            .record_failure("app.clientco.com", 100, "CA said no", 300)
            .await
            .unwrap();
        let record = registry.get("app.clientco.com").unwrap();
        assert_eq!(record.status, DomainStatus::Verified);
        assert_eq!(record.next_attempt_unix, Some(400));
        assert!(!record.is_due(399));
        assert!(record.is_due(400));
    }

    #[test]
    fn a_spent_budget_reports_when_it_rolls() {
        let limiter = IssuanceLimiter::new(1, 10, 300, 3600);
        limiter.record_attempt("a.test", 1000);
        match limiter.check("a.test", 1500) {
            IssuanceDecision::PerDomainLimit { retry_after_secs } => {
                assert_eq!(retry_after_secs, 1000 + PER_DOMAIN_WINDOW_SECS - 1500);
            }
            other => panic!("expected a per-domain refusal, got {other:?}"),
        }
        // Once the window has rolled the domain is allowed again.
        assert_eq!(
            limiter.check("a.test", 1000 + PER_DOMAIN_WINDOW_SECS + 1),
            IssuanceDecision::Allow
        );
        // Offboarding clears the history.
        limiter.forget("a.test");
        assert_eq!(limiter.check("a.test", 1500), IssuanceDecision::Allow);
    }

    #[test]
    fn the_budget_says_nothing_about_backoff() {
        // The retry deadline is the domain record's job; the limiter only ever
        // reports a spent budget, so a failing domain is not refused twice.
        let limiter = IssuanceLimiter::new(10, 10, 300, 3600);
        assert_eq!(limiter.check("a.test", 1000), IssuanceDecision::Allow);
        assert_eq!(limiter.backoff_for(3), 1200);
    }
}
