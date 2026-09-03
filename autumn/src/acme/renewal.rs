//! ACME issuance, the renewal loop, the self-signed placeholder, and the
//! observability seam (issue #1608).
//!
//! At boot the TLS listener needs *a* certificate to bind `:443` immediately.
//! If a valid certificate is already stored it is served; otherwise a
//! short-lived self-signed placeholder ([`self_signed_placeholder`]) is served
//! so the port comes up, and the background renewal task
//! ([`AcmeRenewalTask::run`]) obtains the real certificate over HTTP-01 and
//! hot-swaps it into the SAME [`ReloadableCertResolver`](crate::tls::ReloadableCertResolver).
//!
//! The renewal loop is spawned unconditionally whenever ACME is configured (a
//! pure `web` replica must renew its own cert). It leader-elects through the
//! existing [`SchedulerCoordinator`](crate::scheduler::SchedulerCoordinator) so
//! that, across a fleet, only one replica orders per certificate.

use std::sync::{Arc, RwLock};
use std::time::Duration;

use rustls::crypto::CryptoProvider;

use crate::acme::challenge::Http01Tokens;
use crate::acme::dns::resolver::DnsLookup;
use crate::acme::dns::{DnsProvider, TxtRecord};
use crate::acme::store::{AcmeStore, CertId, StoredCert};
use crate::config::AcmeConfig;
use crate::scheduler::SchedulerCoordinator;
use crate::task::TaskCoordination;
use crate::tls::ReloadableCertResolver;

/// How often the renewal loop wakes to re-check expiry. An hour is frequent
/// enough to act well inside the (default 30-day) renew-before window while
/// costing almost nothing.
const RENEWAL_CHECK_INTERVAL: Duration = Duration::from_secs(3600);

/// How long to wait for a DNS-01 order to settle after signalling the challenges
/// ready. See [`AcmeRenewalTask::await_order_ready`] for why the default is too
/// short here.
const DNS01_ORDER_READY_TIMEOUT: Duration = Duration::from_secs(180);

/// The scheduler task name used for ACME renewal leader-election.
const RENEWAL_TASK_NAME: &str = "acme-renewal";

/// A per-failure callback invoked by the renewal task with a message.
///
/// The app wires this to the registered [`ErrorReporter`] chain (behind the
/// `reporting` feature); by default it is a no-op and failures still log via
/// `tracing` inside the loop.
///
/// [`ErrorReporter`]: crate::reporting::ErrorReporter
pub type ReporterFn = Arc<dyn Fn(String) + Send + Sync>;

/// A callback invoked after a successful issuance/renewal that followed a
/// recorded failure.
///
/// The app wires this to #1610's operator-alert recovery, so a
/// `scheduled_task_failure` alert raised for `acme-renewal` is cleared once the
/// certificate is obtained. Failures reach the operator through
/// [`ReporterFn`]; this is the other half of that pair.
pub type RecoveryFn = Arc<dyn Fn() + Send + Sync>;

/// Everything the DNS-01 challenge path needs (issue #1620).
///
/// Present on [`AcmeRenewalTask`] exactly when `[server.tls.acme.dns]` is
/// configured; its absence keeps issuance on #1608's HTTP-01 path byte for byte.
pub struct DnsChallenge {
    /// Writes and removes the `_acme-challenge` TXT records.
    pub provider: Arc<dyn DnsProvider>,
    /// Sends the DNS queries that discover the zone's nameservers and confirm
    /// the records are visible.
    pub lookup: Arc<dyn DnsLookup>,
    /// The resolvers used to DISCOVER the zone's authoritative nameservers — and
    /// the fallback probed directly when discovery fails. See
    /// [`resolver`](crate::acme::dns::resolver) for why the propagation probe
    /// goes to the authoritative servers rather than to these.
    pub resolvers: Vec<std::net::SocketAddr>,
    /// Bound on the propagation wait.
    pub propagation_timeout: Duration,
    /// Gap between propagation probes.
    pub poll_interval: Duration,
}

/// Decide whether a certificate whose leaf expires at `not_after_unix` should be
/// renewed now.
///
/// Renews once fewer than `renew_before_days` of validity remain (and, of
/// course, once already expired). `now_unix` is injected so the decision is
/// deterministically unit-testable, mirroring `tls.rs`'s `now_unix` style.
#[must_use]
pub const fn needs_renewal(not_after_unix: i64, renew_before_days: u32, now_unix: i64) -> bool {
    let threshold = (renew_before_days as i64).saturating_mul(86_400);
    not_after_unix.saturating_sub(now_unix) < threshold
}

/// Generate a short-lived self-signed placeholder certificate covering
/// `domains` (CN = first domain), so `:443` can bind before the first real
/// issuance completes.
///
/// # Errors
///
/// Returns a message if certificate generation fails.
pub fn self_signed_placeholder(domains: &[String]) -> Result<StoredCert, String> {
    use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair};

    if domains.is_empty() {
        return Err("cannot build a placeholder certificate with no domains".to_owned());
    }
    let key_pair =
        KeyPair::generate().map_err(|e| format!("failed to generate placeholder key: {e}"))?;
    let mut params = CertificateParams::new(domains.to_vec())
        .map_err(|e| format!("failed to build placeholder params: {e}"))?;
    let mut dn = DistinguishedName::new();
    dn.push(DnType::CommonName, domains[0].clone());
    params.distinguished_name = dn;
    let cert = params
        .self_signed(&key_pair)
        .map_err(|e| format!("failed to self-sign placeholder: {e}"))?;
    Ok(StoredCert {
        chain_pem: cert.pem(),
        key_pem: key_pair.serialize_pem(),
    })
}

/// Live status of ACME provisioning, written by the renewal task and read by
/// [`AcmeHealthIndicator`]. Cheap to clone (an `Arc`).
#[derive(Clone, Default)]
pub struct AcmeStatus(Arc<RwLock<AcmeStatusInner>>);

#[derive(Default)]
struct AcmeStatusInner {
    last_success_unix: Option<i64>,
    last_failure: Option<(i64, String)>,
    cert_not_after_unix: Option<i64>,
}

/// An immutable snapshot of [`AcmeStatus`].
#[derive(Clone, Default)]
pub struct AcmeStatusSnapshot {
    /// UNIX time of the last successful issuance/renewal, if any.
    pub last_success_unix: Option<i64>,
    /// UNIX time and message of the last failure, if any.
    pub last_failure: Option<(i64, String)>,
    /// The currently served certificate's `notAfter`, if known.
    pub cert_not_after_unix: Option<i64>,
}

impl AcmeStatus {
    /// Create an empty status.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    fn write(&self) -> std::sync::RwLockWriteGuard<'_, AcmeStatusInner> {
        self.0
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Record a successful issuance/renewal serving a cert with the given expiry.
    pub fn record_success(&self, now_unix: i64, cert_not_after_unix: i64) {
        let mut inner = self.write();
        inner.last_success_unix = Some(now_unix);
        inner.cert_not_after_unix = Some(cert_not_after_unix);
        inner.last_failure = None;
    }

    /// Record a failed issuance/renewal attempt.
    pub fn record_failure(&self, now_unix: i64, message: impl Into<String>) {
        self.write().last_failure = Some((now_unix, message.into()));
    }

    /// Note the expiry of the certificate currently being served (e.g. the
    /// stored cert or placeholder loaded at boot).
    pub fn set_cert_not_after(&self, not_after_unix: i64) {
        self.write().cert_not_after_unix = Some(not_after_unix);
    }

    /// Take an immutable snapshot.
    #[must_use]
    pub fn snapshot(&self) -> AcmeStatusSnapshot {
        let inner = self
            .0
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        AcmeStatusSnapshot {
            last_success_unix: inner.last_success_unix,
            last_failure: inner.last_failure.clone(),
            cert_not_after_unix: inner.cert_not_after_unix,
        }
    }
}

/// A [`HealthIndicator`](crate::actuator::HealthIndicator) reporting ACME
/// provisioning health.
///
/// Registered in the `HealthOnly` group so a transient renewal failure surfaces
/// in `/actuator/health` WITHOUT failing `/ready` (a working, not-yet-expired
/// certificate is still being served). It grades `Down` only when a failure
/// coincides with real expiry danger — the served cert is inside its
/// renew-before window — otherwise `Up` with diagnostic details.
pub struct AcmeHealthIndicator {
    status: AcmeStatus,
    renew_before_days: u32,
    now_unix: fn() -> i64,
    /// The configured DNS-01 provider name, when DNS-01 is in use (#1620).
    /// A provider NAME, never a credential — it is published in
    /// `/actuator/health`.
    dns_provider: Option<&'static str>,
}

impl AcmeHealthIndicator {
    /// Build an indicator reading `status`, using `renew_before_days` to decide
    /// when a failure is dangerous.
    #[must_use]
    pub fn new(status: AcmeStatus, renew_before_days: u32) -> Self {
        Self {
            status,
            renew_before_days,
            now_unix: default_now_unix,
            dns_provider: None,
        }
    }

    /// Report which challenge type is in use, naming the DNS-01 provider when
    /// one is configured (issue #1620).
    ///
    /// Surfaces `challenge` (`http-01`/`dns-01`) and `dns_provider` in the
    /// health details, which is the first thing an operator needs when issuance
    /// is failing. Both are configuration NAMES; no credential is exposed.
    #[must_use]
    pub const fn with_dns_provider(mut self, provider: Option<&'static str>) -> Self {
        self.dns_provider = provider;
        self
    }

    /// Grade the current status against `now_unix` (pure; used by `check` and
    /// unit tests).
    #[must_use]
    pub fn grade(&self, now_unix: i64) -> crate::actuator::HealthCheckOutput {
        use crate::actuator::{HealthCheckOutput, HealthStatus};
        let snap = self.status.snapshot();
        let mut details = std::collections::HashMap::new();

        details.insert(
            "challenge".to_owned(),
            serde_json::json!(if self.dns_provider.is_some() {
                "dns-01"
            } else {
                "http-01"
            }),
        );
        if let Some(provider) = self.dns_provider {
            details.insert("dns_provider".to_owned(), serde_json::json!(provider));
        }
        if let Some(not_after) = snap.cert_not_after_unix {
            let days = (not_after - now_unix) / 86_400;
            details.insert("days_until_expiry".to_owned(), serde_json::json!(days));
        }
        if let Some(ts) = snap.last_success_unix {
            details.insert("last_success_unix".to_owned(), serde_json::json!(ts));
        }
        if let Some((ts, msg)) = &snap.last_failure {
            details.insert("last_failure_unix".to_owned(), serde_json::json!(ts));
            details.insert("last_failure".to_owned(), serde_json::json!(msg));
        }

        // An already-expired served certificate is Down on its own, INDEPENDENTLY
        // of whether a renewal failure has been recorded. At boot
        // `build_acme_tls_listener` records the stored cert's `cert_not_after_unix`
        // and serves it while renewal runs; if leadership is skipped/degraded or
        // issuance is merely pending, `last_failure` stays `None` and the
        // failure-in-danger-window rule below would report `Up` for a TLS cert
        // that is already invalid. Treat `not_after <= now` as Down and say so.
        let cert_expired = snap
            .cert_not_after_unix
            .is_some_and(|not_after| not_after <= now_unix);
        if cert_expired {
            details.insert("cert_expired".to_owned(), serde_json::json!(true));
        }

        // Down ALSO when a failure coincides with real expiry danger: the served
        // certificate is already inside its renew-before window (or we have no
        // certificate at all). A failure with plenty of validity left is a blip.
        let in_danger = snap
            .cert_not_after_unix
            .is_none_or(|not_after| needs_renewal(not_after, self.renew_before_days, now_unix));
        let status = if cert_expired || (snap.last_failure.is_some() && in_danger) {
            HealthStatus::Down
        } else {
            HealthStatus::Up
        };
        HealthCheckOutput { status, details }
    }
}

impl crate::actuator::HealthIndicator for AcmeHealthIndicator {
    fn check(&self) -> futures::future::BoxFuture<'_, crate::actuator::HealthCheckOutput> {
        let now = (self.now_unix)();
        Box::pin(async move { self.grade(now) })
    }

    fn group(&self) -> crate::actuator::IndicatorGroup {
        crate::actuator::IndicatorGroup::HealthOnly
    }
}

fn default_now_unix() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
}

/// Everything the background renewal task needs to run.
///
/// Built by the app at bind time (after the initial resolver is constructed) and
/// spawned as a sibling of the main server under `server_shutdown`.
pub struct AcmeRenewalTask {
    /// The resolver the TLS listener serves; issued certs are swapped in here.
    pub resolver: Arc<ReloadableCertResolver>,
    /// The crypto provider (shared with the listener).
    pub provider: Arc<CryptoProvider>,
    /// Persistent store for the account + issued certificate.
    pub store: Arc<dyn AcmeStore>,
    /// The certificate id (hash of the sorted domain set).
    pub cert_id: CertId,
    /// The HTTP-01 token map shared with the `:80` challenge listener.
    pub tokens: Http01Tokens,
    /// Live status for the health indicator.
    pub status: AcmeStatus,
    /// Resolved ACME configuration.
    pub config: AcmeConfig,
    /// Whether a valid stored certificate is already being served (so the first
    /// tick can skip ordering unless it is due for renewal).
    pub serving_stored_cert: bool,
    /// Set when a *distributed* scheduler backend was configured (multi-replica
    /// intent) but this process could not build the distributed coordinator and
    /// fell back to a per-process in-process one. In that state we must NOT
    /// order: every replica would grab its OWN local lease and order the SAME
    /// certificate concurrently, racing the per-process HTTP-01 token maps and
    /// local stores and burning the CA's rate limits. Instead each cycle records
    /// an ACME failure and keeps serving the existing/placeholder cert until
    /// leadership is restored. Never set for a genuinely single-replica
    /// deployment (`scheduler.backend = in_process` → in-process is correct).
    pub leadership_degraded: bool,
    /// Runtime backstop against a re-renewal loop that burns CA rate limits.
    ///
    /// Set once a freshly-issued certificate STILL immediately satisfies
    /// [`needs_renewal`] — i.e. the configured `renew_before_days` is >= the
    /// certificate's own lifetime (a value `AcmeConfig::validate()` rejects for
    /// public CAs, but a custom directory issuing shorter-lived certificates can
    /// still trip). While set, the loop refuses to order again — otherwise every
    /// hourly tick would order a brand-new certificate until the CA rate-limits
    /// the account. Cleared only by a restart (which re-runs config validation).
    pub renew_window_misconfigured: std::sync::atomic::AtomicBool,
    /// DNS-01 wiring (issue #1620), present exactly when
    /// `[server.tls.acme.dns]` is configured. `None` keeps issuance on #1608's
    /// HTTP-01 path; `Some` answers every authorization over DNS-01, which is
    /// the only challenge type a CA accepts for a **wildcard** identifier.
    pub dns: Option<DnsChallenge>,
    /// Invoked after an issuance that succeeded following a recorded failure,
    /// so the app can clear the operator alert its `reporter` raised (#1610).
    pub recovery: Option<RecoveryFn>,
}

/// RAII guard tracking the HTTP-01 tokens published for one order.
///
/// Every token handed to [`PublishedTokens::publish`] is inserted into the
/// shared [`Http01Tokens`] map immediately and removed again when the guard
/// drops — so no matter which `?` in the order flow returns `Err`, published
/// tokens never leak into the map to accumulate across repeated failures.
struct PublishedTokens<'a> {
    tokens: &'a Http01Tokens,
    published: Vec<String>,
}

impl<'a> PublishedTokens<'a> {
    const fn new(tokens: &'a Http01Tokens) -> Self {
        Self {
            tokens,
            published: Vec::new(),
        }
    }

    /// Publish `token → key_authorization` and track it for cleanup on drop.
    fn publish(&mut self, token: String, key_authorization: String) {
        self.tokens.insert(token.clone(), key_authorization);
        self.published.push(token);
    }
}

impl Drop for PublishedTokens<'_> {
    fn drop(&mut self) {
        for token in &self.published {
            self.tokens.remove(token);
        }
    }
}

/// Decide whether the boot-time immediate order should fire, given the
/// `notAfter` of the stored leaf we would serve (`None` while still on the
/// self-signed placeholder — i.e. no real cert yet).
///
/// Fires when there is no real stored certificate yet OR the stored leaf is
/// already inside its renew-before window (including already expired) —
/// otherwise a loadable but dead/expiring stored cert would be served for up to
/// [`RENEWAL_CHECK_INTERVAL`] before the first scheduled check renews it.
///
/// Pure and injectable (`now_unix`) so the decision is unit-testable without the
/// network, mirroring [`needs_renewal`].
#[must_use]
fn due_at_boot(stored_not_after: Option<i64>, renew_before_days: u32, now_unix: i64) -> bool {
    stored_not_after.is_none_or(|not_after| needs_renewal(not_after, renew_before_days, now_unix))
}

impl AcmeRenewalTask {
    /// Run the renewal loop until `shutdown` fires.
    ///
    /// On boot it ensures a certificate is present (ordering immediately when
    /// none is stored or the stored one is due), then wakes hourly to renew
    /// before expiry. Ordering is leader-elected via `coordinator` so only one
    /// replica per certificate orders; a failure is logged, reported through
    /// `reporter`, and retried on the next tick — it never tears down the
    /// listener (the previously served certificate keeps working).
    pub async fn run(
        self,
        coordinator: Arc<dyn SchedulerCoordinator>,
        reporter: ReporterFn,
        shutdown: tokio_util::sync::CancellationToken,
    ) {
        // Immediate check on boot: order now if we have no real cert yet, or the
        // stored cert we would serve is already inside its renew-before window
        // (or expired). Without the latter, a loadable-but-expired stored cert
        // sets `serving_stored_cert = true` and we would serve a dead cert for
        // up to `RENEWAL_CHECK_INTERVAL` before the first scheduled renewal.
        let stored_not_after = if self.serving_stored_cert {
            self.stored_not_after().await
        } else {
            None
        };
        if due_at_boot(stored_not_after, self.config.renew_before_days, now_unix()) {
            self.try_renew_once(&coordinator, &reporter).await;
        }

        loop {
            tokio::select! {
                () = tokio::time::sleep(RENEWAL_CHECK_INTERVAL) => {}
                () = shutdown.cancelled() => break,
            }
            self.maybe_renew(&coordinator, &reporter).await;
        }
    }

    /// Check expiry and renew if within the renew-before window.
    async fn maybe_renew(
        &self,
        coordinator: &Arc<dyn SchedulerCoordinator>,
        reporter: &ReporterFn,
    ) {
        // Every tick, BEFORE the renewal decision, hot-swap in a newer/healthier
        // stored certificate if one has appeared. When another replica (or an
        // out-of-band tool) persists a fresh cert to the shared/local store, a
        // process that did not issue it must adopt it immediately — otherwise a
        // follower keeps serving the placeholder or a stale in-memory cert until
        // its own renew window opens (which, for a freshly-renewed cert, is up to
        // a full renew-before period away). Adoption never downgrades: it only
        // swaps to a strictly-newer, fully-loadable pair.
        self.adopt_stored_cert_if_newer().await;

        // Runtime backstop: a previous issuance produced a certificate that STILL
        // immediately satisfied `needs_renewal` (the renew-before window is >= the
        // cert's own lifetime). Ordering again would loop and burn the CA's rate
        // limits, so refuse to order until a restart re-runs config validation.
        // The failure was already recorded/reported when the flag was set.
        if self
            .renew_window_misconfigured
            .load(std::sync::atomic::Ordering::SeqCst)
        {
            return;
        }

        // No stored cert (still on the placeholder), or a torn/mismatched pair
        // that does not load → treated as absent → always due.
        let due = self.stored_not_after().await.is_none_or(|not_after| {
            needs_renewal(not_after, self.config.renew_before_days, now_unix())
        });
        if due {
            self.try_renew_once(coordinator, reporter).await;
        }
    }

    /// Attempt one leader-elected issuance/renewal, updating status.
    async fn try_renew_once(
        &self,
        coordinator: &Arc<dyn SchedulerCoordinator>,
        reporter: &ReporterFn,
    ) {
        // A distributed scheduler backend was configured (multi-replica intent)
        // but this process could not build the distributed coordinator and fell
        // back to a per-process in-process one. Ordering now would give this
        // replica its OWN local lease, so every replica would order the SAME
        // certificate concurrently — racing the per-process HTTP-01 token maps
        // and local stores and burning the CA's rate limits. Refuse to order:
        // record the failure and dispatch it through the reporter (same seam as
        // the coordinator-error path) so health does not stay `Up` while serving
        // only the placeholder, then keep serving the existing cert this cycle.
        if self.leadership_degraded {
            let msg = "ACME renewal: a distributed scheduler backend is configured but its \
                coordinator is unavailable in this process — refusing to order to avoid racing \
                replicas and Let's Encrypt rate limits (fix the scheduler backend / database \
                connectivity, or run ACME on a single host)"
                .to_owned();
            tracing::error!("{msg}");
            self.status.record_failure(now_unix(), msg.clone());
            reporter(msg);
            return;
        }

        // `Fleet` is the single-leader mode: the in-process backend always
        // grants (correct single-replica behavior), while the Postgres backend
        // grants to exactly one replica via an advisory lock keyed on the cert.
        let tick_key = format!("acme:{}", self.cert_id.as_str());
        let lease = match coordinator
            .try_acquire(RENEWAL_TASK_NAME, &tick_key, TaskCoordination::Fleet)
            .await
        {
            Ok(Some(lease)) => lease,
            Ok(None) => {
                // Another replica is the leader for this cert. Try to pick up a
                // cert it may have already persisted to a shared store.
                self.adopt_stored_cert_if_newer().await;
                return;
            }
            Err(e) => {
                // The coordinator itself errored (e.g. the Postgres advisory-lock
                // pool is unavailable at first boot). We do NOT know whether we
                // are the leader, so we must NOT order — but this is a real
                // failure, not a benign "someone else leads". Record it and
                // dispatch through the reporter (same as an issuance failure) so
                // the health indicator does not stay `Up` while serving only the
                // self-signed placeholder, then skip this cycle and retry next
                // tick.
                let msg = format!("ACME renewal leader election failed: {e}");
                tracing::error!("{msg}");
                self.status.record_failure(now_unix(), msg.clone());
                reporter(msg);
                return;
            }
        };

        let outcome = self.issue().await;
        // Always release the lease, regardless of outcome.
        if let Err(e) = lease.release().await {
            tracing::warn!(error = %e, "failed to release ACME renewal lease");
        }

        self.handle_issue_outcome(outcome, reporter);
    }

    /// Record the result of one [`issue`](Self::issue) attempt and apply the
    /// misconfigured-renew-window backstop.
    ///
    /// Extracted from [`try_renew_once`](Self::try_renew_once) (and kept
    /// network-free) so the backstop is unit-testable without ordering a real
    /// certificate.
    fn handle_issue_outcome(&self, outcome: Result<i64, String>, reporter: &ReporterFn) {
        match outcome {
            Ok(not_after) => {
                // Read BEFORE `record_success` clears it: the recovery callback
                // must fire only for a success that ends an OUTSTANDING failure,
                // so a steady-state renewal does not re-notify every cycle.
                let recovered_from_failure = self.status.snapshot().last_failure.is_some();
                self.status.record_success(now_unix(), not_after);
                if recovered_from_failure && let Some(recovery) = &self.recovery {
                    recovery();
                }
                tracing::info!(
                    cert_id = self.cert_id.as_str(),
                    "ACME certificate issued/renewed and hot-swapped into the TLS listener"
                );

                // Backstop (defense in depth): if the freshly-issued certificate
                // ALREADY satisfies `needs_renewal`, the configured
                // `renew_before_days` is >= the certificate's own lifetime.
                // `AcmeConfig::validate()` rejects this for a public CA, but a
                // custom directory issuing shorter-lived certs can still reach it.
                // Re-ordering on the next tick would loop and burn the CA's rate
                // limits, so record a failure, report it, and refuse to order
                // again until a restart re-runs config validation.
                if needs_renewal(not_after, self.config.renew_before_days, now_unix()) {
                    let msg = format!(
                        "ACME renewal: renew_before_days ({}) is >= the issued certificate \
                         lifetime; refusing to re-order to avoid burning CA rate limits — lower \
                         [server.tls.acme] renew_before_days below the certificate's validity \
                         period",
                        self.config.renew_before_days
                    );
                    tracing::error!("{msg}");
                    self.status.record_failure(now_unix(), msg.clone());
                    reporter(msg);
                    self.renew_window_misconfigured
                        .store(true, std::sync::atomic::Ordering::SeqCst);
                }
            }
            Err(e) => {
                let msg = format!("ACME certificate issuance failed: {e}");
                tracing::error!("{msg}");
                self.status.record_failure(now_unix(), msg.clone());
                reporter(msg);
            }
        }
    }

    /// The stored certificate's leaf `notAfter`, if a *usable* pair is persisted.
    ///
    /// Returns the expiry ONLY when the full chain+key pair loads as a valid
    /// [`CertifiedKey`](rustls::sign::CertifiedKey). A torn write (a new chain
    /// left with the old/mismatched key by a crash between `save_cert`'s two
    /// renames — or observed mid-write by another reader), or any otherwise
    /// malformed pair, is treated as ABSENT (`None`) rather than trusting the
    /// future-dated-but-unusable chain: otherwise `maybe_renew` would see a
    /// healthy far-future `notAfter`, decide renewal is not due, and stay stuck on
    /// the placeholder/old cert until the wrong chain finally entered its renew
    /// window. `None` here makes the renewal decision proceed instead.
    async fn stored_not_after(&self) -> Option<i64> {
        let stored = self.store.load_cert(&self.cert_id).await.ok().flatten()?;
        // Validate the FULL pair (same load/validation path the resolver uses) so
        // a mismatched or malformed pair is detected and counts as absent.
        crate::tls::certified_key_from_pem(
            stored.chain_pem.as_bytes(),
            stored.key_pem.as_bytes(),
            &self.provider,
        )
        .ok()?;
        crate::tls::leaf_not_after_from_pem(stored.chain_pem.as_bytes()).ok()
    }

    /// If the store holds a strictly-newer, fully-loadable certificate than we
    /// currently serve, adopt it into the resolver.
    ///
    /// Only swaps when the stored pair loads as a valid `CertifiedKey` AND its
    /// leaf `notAfter` is strictly later than the currently-served cert's (or we
    /// have no served expiry recorded yet) — it never downgrades to an older cert
    /// and never swaps in a torn/mismatched pair.
    async fn adopt_stored_cert_if_newer(&self) {
        let Some(stored) = self.store.load_cert(&self.cert_id).await.ok().flatten() else {
            return;
        };
        let Ok(not_after) = crate::tls::leaf_not_after_from_pem(stored.chain_pem.as_bytes()) else {
            return;
        };
        let serving = self.status.snapshot().cert_not_after_unix;
        if serving.is_none_or(|current| not_after > current)
            && let Ok(certified) = crate::tls::certified_key_from_pem(
                stored.chain_pem.as_bytes(),
                stored.key_pem.as_bytes(),
                &self.provider,
            )
        {
            self.resolver.store(certified);
            self.status.set_cert_not_after(not_after);
            tracing::info!("adopted a newer ACME certificate from the store");
        }
    }

    /// The full issuance flow: order → challenge → finalize → persist →
    /// hot-swap. Returns the issued leaf's `notAfter` on success.
    ///
    /// The challenge half is the only part that varies: HTTP-01 (#1608) when no
    /// DNS provider is configured, DNS-01 (#1620) when one is — the latter being
    /// the only challenge type a CA will accept for a wildcard identifier.
    /// Everything downstream (CSR, finalize, persistence, hot-swap) is shared.
    async fn issue(&self) -> Result<i64, String> {
        use instant_acme::{Identifier, NewOrder};

        let account = self.load_or_register_account().await?;

        let identifiers: Vec<Identifier> = self
            .config
            .domains
            .iter()
            .map(|d| Identifier::Dns(d.clone()))
            .collect();
        // A wildcard is ordered as the literal `*.myapp.com` identifier (RFC 8555
        // §7.1.3); the CA answers with an authorization whose identifier is the
        // BASE domain plus `wildcard: true` (§7.1.4), which is why the
        // `_acme-challenge` record name is derived from the AUTHORIZATION's
        // identifier rather than from the configured domain list.
        let mut order = account
            .new_order(&NewOrder::new(&identifiers))
            .await
            .map_err(|e| format!("failed to create ACME order: {e}"))?;

        // Both arms answer their challenges AND wait for the order to settle
        // before their challenge responses are torn down: `set_ready` only tells
        // the CA to *queue* validation, so a token or TXT record removed before
        // the order reaches `ready` is removed while the CA is still looking at
        // it.
        if let Some(dns) = &self.dns {
            self.answer_dns01(&mut order, dns).await?;
        } else {
            let published = self.answer_http01(&mut order).await?;
            let ready = self.await_order_ready(&mut order).await;
            drop(published);
            ready?;
        }
        self.finalize_and_install(&mut order).await
    }

    /// Publish an HTTP-01 response for every pending authorization and tell the
    /// CA each is ready, returning the RAII guard that un-publishes them.
    async fn answer_http01<'a>(
        &'a self,
        order: &mut instant_acme::Order,
    ) -> Result<PublishedTokens<'a>, String> {
        use instant_acme::{AuthorizationStatus, ChallengeType};

        // Every token handed to the guard is inserted into the shared map
        // immediately and removed again when the guard drops — so no matter
        // which `?` returns `Err`, published tokens never leak into the map to
        // accumulate across repeated failures.
        let mut published = PublishedTokens::new(&self.tokens);
        let mut authorizations = order.authorizations();
        while let Some(result) = authorizations.next().await {
            let mut authz = result.map_err(|e| format!("failed to fetch authorization: {e}"))?;
            if authz.status == AuthorizationStatus::Valid {
                continue;
            }
            let mut challenge = authz
                .challenge(ChallengeType::Http01)
                .ok_or_else(|| "authorization offered no http-01 challenge".to_owned())?;
            let token = challenge.token.clone();
            let key_auth = challenge.key_authorization().as_str().to_owned();
            // Publish BEFORE signalling ready so the CA can fetch it — and
            // track it in the guard so a failing `set_ready` still cleans up.
            published.publish(token, key_auth);
            challenge
                .set_ready()
                .await
                .map_err(|e| format!("failed to signal challenge ready: {e}"))?;
        }
        Ok(published)
    }

    /// Answer every pending authorization over DNS-01 and wait for the order to
    /// settle (issue #1620).
    ///
    /// Strictly ordered: collect every challenge value, publish **all** of them,
    /// wait until every configured resolver sees every value, tell the CA to
    /// validate, and only then wait for the order to become ready. Signalling
    /// ready before propagation is how a DNS-01 authorization is burnt — the CA
    /// queries once, sees nothing, and marks it invalid.
    ///
    /// The records are removed on **every** exit path, including the error ones,
    /// so a failing order never leaves `_acme-challenge` litter in the zone to
    /// accumulate across retries. Removal happens only after the order has
    /// settled: `set_ready` merely *queues* validation, so deleting a record the
    /// moment it returns would pull it out from under a CA that has not looked
    /// yet.
    async fn answer_dns01(
        &self,
        order: &mut instant_acme::Order,
        dns: &DnsChallenge,
    ) -> Result<(), String> {
        let wanted = self.collect_dns01_records(order).await?;
        if wanted.is_empty() {
            // Every authorization is already valid (a re-used order): nothing to
            // publish, nothing to clean up — but the order still has to settle.
            return self.await_order_ready(order).await;
        }

        // Track what actually reached the provider — including a partial publish
        // interrupted mid-way — so cleanup removes exactly those and no more.
        let mut published: Vec<TxtRecord> = Vec::new();
        let outcome = self
            .publish_and_validate_dns01(order, dns, &wanted, &mut published)
            .await;
        Self::cleanup_dns01(dns, &published).await;
        outcome
    }

    /// Publish `wanted`, wait for propagation, signal every challenge ready, and
    /// wait for the order to reach `ready`.
    ///
    /// Split out of [`answer_dns01`](Self::answer_dns01) so the caller can run
    /// cleanup after it regardless of which step failed — and so cleanup runs
    /// only once the CA has finished with the records.
    async fn publish_and_validate_dns01(
        &self,
        order: &mut instant_acme::Order,
        dns: &DnsChallenge,
        wanted: &[TxtRecord],
        published: &mut Vec<TxtRecord>,
    ) -> Result<(), String> {
        for record in wanted {
            dns.provider.upsert_txt(record).await.map_err(|e| {
                format!(
                    "failed to publish the DNS-01 TXT record {} via the {} provider: {e}",
                    record.fqdn,
                    dns.provider.name()
                )
            })?;
            // Recorded only after the write succeeded, but before the next one —
            // a failure on record two still cleans up record one.
            published.push(record.clone());
        }

        // Probe the zone's OWN nameservers, not the configured recursive
        // resolvers. Probing a public recursive immediately after the write
        // plants a negative-cache entry whose TTL (900s for Route 53, 1800s for
        // Cloudflare) outlives the propagation budget, so every later probe —
        // and every later renewal — reads the cached "not there" and the wait
        // can never succeed. The configured resolvers are how the authoritative
        // set is discovered, and the fallback when that fails.
        let probe_targets = self.dns01_probe_targets(dns, wanted).await;

        crate::acme::dns::resolver::wait_for_propagation(
            wanted,
            &probe_targets,
            dns.propagation_timeout,
            dns.poll_interval,
            dns.lookup.as_ref(),
        )
        .await?;

        Self::signal_dns01_ready(order).await?;
        // Inside the cleanup scope on purpose: the CA validates asynchronously
        // after `set_ready`, so the records must stay published until the order
        // has actually settled.
        self.await_order_ready(order).await
    }

    /// The servers the propagation wait should probe: the zone's authoritative
    /// nameservers when they can be discovered, else the configured resolvers.
    ///
    /// Discovery is per-order rather than per-boot because the zone's NS set can
    /// change between renewals, and because a wildcard order's records all live
    /// in one zone — so this costs one `NS` lookup plus one `A` lookup per
    /// nameserver, once.
    async fn dns01_probe_targets(
        &self,
        dns: &DnsChallenge,
        wanted: &[TxtRecord],
    ) -> Vec<std::net::SocketAddr> {
        let Some(first) = wanted.first() else {
            return dns.resolvers.clone();
        };
        let authoritative = crate::acme::dns::resolver::authoritative_resolvers(
            &first.fqdn,
            &dns.resolvers,
            dns.lookup.as_ref(),
        )
        .await;
        if authoritative.is_empty() {
            tracing::warn!(
                fqdn = first.fqdn,
                "could not discover the authoritative nameservers for the DNS-01 challenge zone; \
                 falling back to the configured resolvers. A recursive resolver can cache a \
                 negative answer for longer than the propagation budget, so raise \
                 [server.tls.acme.dns] propagation_timeout_secs if issuance times out"
            );
            return dns.resolvers.clone();
        }
        tracing::debug!(
            fqdn = first.fqdn,
            servers = authoritative.len(),
            "probing the DNS-01 challenge zone's authoritative nameservers"
        );
        authoritative
    }

    /// The TXT record each pending authorization needs, in order.
    ///
    /// An authorization for `*.myapp.com` carries the identifier `myapp.com`
    /// with a wildcard flag, so an apex + wildcard order yields TWO records at
    /// the SAME name with different values. Both are returned and both must be
    /// live before validation — which is why every provider here appends rather
    /// than replaces.
    async fn collect_dns01_records(
        &self,
        order: &mut instant_acme::Order,
    ) -> Result<Vec<TxtRecord>, String> {
        use instant_acme::{AuthorizationStatus, ChallengeType, Identifier};

        let mut records = Vec::new();
        let mut authorizations = order.authorizations();
        while let Some(result) = authorizations.next().await {
            let mut authz = result.map_err(|e| format!("failed to fetch authorization: {e}"))?;
            if authz.status == AuthorizationStatus::Valid {
                continue;
            }
            let domain = match authz.identifier().identifier {
                Identifier::Dns(dns) => dns.clone(),
                other => {
                    return Err(format!(
                        "the DNS-01 challenge can only answer a DNS identifier, but this \
                         authorization is for {other:?}"
                    ));
                }
            };
            // The record name comes from the CA's answer, and writing it commits
            // a DNS change in whatever zone the provider credential can reach.
            // The CSR is pinned to `config.domains`, so a hostile directory
            // cannot obtain a certificate for a name it was not asked for — but
            // it could still steer autumn into writing `_acme-challenge.<x>`
            // into an unrelated zone (and, under the exec hook, into an argv
            // entry). Only answer for a name that was actually ordered.
            if !Self::is_ordered_identifier(&self.config.domains, &domain) {
                return Err(format!(
                    "the CA returned an authorization for `{domain}`, which is not one of the \
                     configured [server.tls.acme] domains; refusing to publish a challenge record \
                     for it"
                ));
            }
            let challenge = authz.challenge(ChallengeType::Dns01).ok_or_else(|| {
                format!(
                    "the CA offered no dns-01 challenge for `{domain}`; a wildcard certificate \
                     cannot be issued without it"
                )
            })?;
            records.push(TxtRecord::new(
                &domain,
                challenge.key_authorization().dns_value(),
            ));
        }
        Ok(records)
    }

    /// Whether `identifier` is a name this order actually asked for.
    ///
    /// An authorization's identifier is the BASE domain, so `*.myapp.com` in the
    /// config authorises an authorization for `myapp.com` (RFC 8555 §7.1.4).
    fn is_ordered_identifier(domains: &[String], identifier: &str) -> bool {
        let identifier = identifier.trim().trim_end_matches('.').to_ascii_lowercase();
        domains.iter().any(|domain| {
            let domain = domain.trim().trim_end_matches('.').to_ascii_lowercase();
            domain == identifier || domain.strip_prefix("*.") == Some(identifier.as_str())
        })
    }

    /// Tell the CA every pending DNS-01 challenge is ready to validate.
    ///
    /// A second pass over the order's authorizations: their state is already
    /// cached from [`collect_dns01_records`](Self::collect_dns01_records), so
    /// this costs no extra fetch — it exists only so no challenge is signalled
    /// before every record has propagated.
    async fn signal_dns01_ready(order: &mut instant_acme::Order) -> Result<(), String> {
        use instant_acme::{AuthorizationStatus, ChallengeType};

        let mut authorizations = order.authorizations();
        while let Some(result) = authorizations.next().await {
            let mut authz = result.map_err(|e| format!("failed to fetch authorization: {e}"))?;
            if authz.status == AuthorizationStatus::Valid {
                continue;
            }
            let mut challenge = authz
                .challenge(ChallengeType::Dns01)
                .ok_or_else(|| "authorization offered no dns-01 challenge".to_owned())?;
            challenge
                .set_ready()
                .await
                .map_err(|e| format!("failed to signal the dns-01 challenge ready: {e}"))?;
        }
        Ok(())
    }

    /// Remove every published challenge record, best-effort.
    ///
    /// A cleanup failure is logged, never propagated: the order's own outcome is
    /// what the caller reports, and turning a successful issuance into a failure
    /// because a leftover TXT record could not be deleted would be worse than
    /// the litter. The next order's publish is idempotent either way.
    async fn cleanup_dns01(dns: &DnsChallenge, published: &[TxtRecord]) {
        for record in published {
            if let Err(e) = dns.provider.delete_txt(record).await {
                tracing::warn!(
                    fqdn = record.fqdn,
                    provider = dns.provider.name(),
                    error = %e,
                    "could not remove the DNS-01 challenge TXT record; it will be overwritten by \
                     the next issuance but can be deleted by hand"
                );
            }
        }
    }

    /// Wait for the order to leave `pending`, rejecting any state but `ready`.
    ///
    /// DNS-01 gets a much longer budget than the default. `RetryPolicy::default()`
    /// stops after roughly 16 seconds, which is fine for HTTP-01 (the CA fetches
    /// a URL synchronously) but marginal for DNS-01, where Let's Encrypt
    /// validates asynchronously from several network perspectives. And giving up
    /// early is not benign: the caller then runs cleanup, deleting the TXT
    /// records **while the CA is still reading them**, so the authorizations go
    /// invalid and the next attempt spends another of the CA's five
    /// failed-validations-per-hour.
    async fn await_order_ready(&self, order: &mut instant_acme::Order) -> Result<(), String> {
        use instant_acme::{OrderStatus, RetryPolicy};

        let policy = if self.dns.is_some() {
            RetryPolicy::default().timeout(DNS01_ORDER_READY_TIMEOUT)
        } else {
            RetryPolicy::default()
        };
        let status = order
            .poll_ready(&policy)
            .await
            .map_err(|e| format!("order did not become ready: {e}"))?;
        if status != OrderStatus::Ready {
            return Err(format!("ACME order ended in unexpected state {status:?}"));
        }
        Ok(())
    }

    /// Finalize the ready order, persist the issued pair, and hot-swap it into
    /// the live resolver. Returns the leaf's `notAfter`.
    async fn finalize_and_install(&self, order: &mut instant_acme::Order) -> Result<i64, String> {
        use instant_acme::RetryPolicy;

        // Finalize with a FRESH rcgen keypair + CSR for exactly these domains.
        let (csr_der, key_pem) = self.generate_csr()?;
        order
            .finalize_csr(&csr_der)
            .await
            .map_err(|e| format!("failed to finalize order: {e}"))?;
        let chain_pem = order
            .poll_certificate(&RetryPolicy::default())
            .await
            .map_err(|e| format!("failed to download certificate: {e}"))?;

        let stored = StoredCert { chain_pem, key_pem };
        let not_after = crate::tls::leaf_not_after_from_pem(stored.chain_pem.as_bytes())?;

        // Persist BEFORE swapping so a crash mid-swap still has the cert on disk.
        self.store
            .save_cert(&self.cert_id, &stored)
            .await
            .map_err(|e| format!("failed to persist issued certificate: {e}"))?;

        let certified = crate::tls::certified_key_from_pem(
            stored.chain_pem.as_bytes(),
            stored.key_pem.as_bytes(),
            &self.provider,
        )?;
        self.resolver.store(certified);
        Ok(not_after)
    }

    /// Generate a fresh keypair + CSR (DER) for the configured domains. Returns
    /// the CSR DER and the private key PEM.
    fn generate_csr(&self) -> Result<(Vec<u8>, String), String> {
        use rcgen::{CertificateParams, DistinguishedName, KeyPair};
        let key_pair =
            KeyPair::generate().map_err(|e| format!("failed to generate cert key: {e}"))?;
        let mut params = CertificateParams::new(self.config.domains.clone())
            .map_err(|e| format!("failed to build CSR params: {e}"))?;
        params.distinguished_name = DistinguishedName::new();
        let csr = params
            .serialize_request(&key_pair)
            .map_err(|e| format!("failed to serialize CSR: {e}"))?;
        Ok((csr.der().to_vec(), key_pair.serialize_pem()))
    }

    /// Build an ACME account builder whose HTTP client trusts the right roots.
    ///
    /// Calls [`ensure_default_crypto_provider`] first — see there for why a
    /// missing process default is a panic rather than an error.
    ///
    /// With no `ca_root_path` the client verifies the directory against the
    /// platform trust store, which is what Let's Encrypt (staging and production
    /// alike — both API endpoints carry publicly-trusted certificates) needs. A
    /// private CA or a Pebble test server serves its directory under a root the
    /// host does not know, so `ca_root_path` replaces the trust anchors with
    /// that root; without it the client cannot complete the TLS handshake and
    /// every order fails.
    ///
    /// Both the register and the restore path go through here, so a restart
    /// against a private directory works exactly like a first boot.
    fn account_builder(&self) -> Result<instant_acme::AccountBuilder, String> {
        ensure_default_crypto_provider();
        self.config.ca_root_path.as_ref().map_or_else(
            || {
                instant_acme::Account::builder()
                    .map_err(|e| format!("failed to build ACME client: {e}"))
            },
            |path| {
                instant_acme::Account::builder_with_root(path).map_err(|e| {
                    format!(
                        "failed to build ACME client with [server.tls.acme] ca_root_path {}: {e}",
                        path.display()
                    )
                })
            },
        )
    }

    /// Load the persisted ACME account, or register a fresh one and persist it.
    async fn load_or_register_account(&self) -> Result<instant_acme::Account, String> {
        use instant_acme::{AccountCredentials, NewAccount};

        let directory_url = crate::acme::directory_url(&self.config.directory);

        if let Some(bytes) = self
            .store
            .load_account()
            .await
            .map_err(|e| format!("failed to read stored ACME account: {e}"))?
        {
            let credentials: AccountCredentials = serde_json::from_slice(&bytes)
                .map_err(|e| format!("stored ACME account is corrupt: {e}"))?;
            let account = self
                .account_builder()?
                .from_credentials(credentials)
                .await
                .map_err(|e| format!("failed to restore ACME account: {e}"))?;
            return Ok(account);
        }

        let contact = format!("mailto:{}", self.config.contact_email.trim());
        let contacts = [contact.as_str()];
        let (account, credentials) = self
            .account_builder()?
            .create(
                &NewAccount {
                    contact: &contacts,
                    terms_of_service_agreed: true,
                    only_return_existing: false,
                },
                directory_url,
                None,
            )
            .await
            .map_err(|e| format!("failed to register ACME account: {e}"))?;

        let serialized = serde_json::to_vec(&credentials)
            .map_err(|e| format!("failed to serialize ACME account: {e}"))?;
        self.store
            .save_account(&serialized)
            .await
            .map_err(|e| format!("failed to persist ACME account: {e}"))?;
        Ok(account)
    }
}

/// Pin the process-level rustls `CryptoProvider` to `ring` if nothing has set one.
///
/// `instant-acme` builds its HTTPS transport through `rustls::ClientConfig::builder()`,
/// which resolves its provider from process-global state. That call does not
/// return an error when it cannot resolve one — it **panics**:
///
/// ```text
/// Could not automatically determine the process-level CryptoProvider from Rustls
/// crate features. Call CryptoProvider::install_default() before this point ...
/// ```
///
/// rustls resolves implicitly only while exactly ONE of its `ring` /
/// `aws-lc-rs` features is enabled. Autumn pins `ring` everywhere, but Cargo
/// unifies features across the whole graph, so any dependency that turns on
/// `aws-lc-rs` makes the choice ambiguous and every ACME order panics. That is
/// not hypothetical or test-only: enabling `telemetry-otlp` alone is enough, and
/// so are `testcontainers`/`bollard` and `postgresql_embedded`.
///
/// Installing is process-wide and one-shot. If a provider is already installed —
/// by an earlier call, or by the application itself — we keep it: the
/// requirement is only that *a* default exists before rustls looks for one, and
/// silently replacing an application's deliberate choice would be worse than
/// the panic this prevents.
fn ensure_default_crypto_provider() {
    if rustls::crypto::CryptoProvider::get_default().is_none() {
        // Errors only if another thread won the race to install one, which
        // satisfies the requirement just as well.
        let _ = rustls::crypto::ring::default_provider().install_default();
    }
}

/// Current UNIX time in seconds.
fn now_unix() -> i64 {
    default_now_unix()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actuator::HealthStatus;

    const DAY: i64 = 86_400;

    /// `instant-acme` reaches `rustls::ClientConfig::builder()`, which PANICS
    /// rather than erroring when it cannot resolve a process-level provider —
    /// which is what happens as soon as any dependency enables `aws-lc-rs`
    /// alongside autumn's `ring` (`telemetry-otlp` alone is enough). Building an
    /// ACME client must therefore always leave a default installed.
    #[test]
    fn building_an_acme_client_guarantees_a_process_crypto_provider() {
        ensure_default_crypto_provider();
        assert!(
            rustls::crypto::CryptoProvider::get_default().is_some(),
            "no process-level CryptoProvider installed; every ACME order would panic"
        );
        // Idempotent: a second call keeps the existing provider rather than
        // failing or replacing it.
        ensure_default_crypto_provider();
        assert!(rustls::crypto::CryptoProvider::get_default().is_some());
    }

    #[test]
    fn needs_renewal_matrix() {
        let now = 1_000_000_000;
        // Far from expiry (60 days out, 30-day window) → not due.
        assert!(!needs_renewal(now + 60 * DAY, 30, now));
        // Just inside the window (29 days out) → due.
        assert!(needs_renewal(now + 29 * DAY, 30, now));
        // Exactly at the threshold (30 days out) → not due (strictly less-than).
        assert!(!needs_renewal(now + 30 * DAY, 30, now));
        // Already expired → due.
        assert!(needs_renewal(now - DAY, 30, now));
    }

    #[test]
    fn placeholder_self_signed_loads_and_swaps_via_resolver() {
        let placeholder =
            self_signed_placeholder(&["app.example.com".to_owned()]).expect("placeholder builds");
        let provider = crate::tls::crypto_provider();
        let certified = crate::tls::certified_key_from_pem(
            placeholder.chain_pem.as_bytes(),
            placeholder.key_pem.as_bytes(),
            &provider,
        )
        .expect("placeholder loads as a CertifiedKey");

        // It swaps into a ReloadableCertResolver (the #1603 hot-swap seam).
        let resolver = Arc::new(ReloadableCertResolver::new(Arc::clone(&certified)));
        let next = self_signed_placeholder(&["app.example.com".to_owned()]).unwrap();
        let next_key = crate::tls::certified_key_from_pem(
            next.chain_pem.as_bytes(),
            next.key_pem.as_bytes(),
            &provider,
        )
        .unwrap();
        resolver.store(Arc::clone(&next_key));
        assert!(Arc::ptr_eq(&resolver.current(), &next_key));

        // And its notAfter is readable for the renewal decision.
        assert!(crate::tls::leaf_not_after_from_pem(placeholder.chain_pem.as_bytes()).is_ok());
    }

    #[test]
    fn placeholder_requires_a_domain() {
        assert!(self_signed_placeholder(&[]).is_err());
    }

    #[test]
    fn health_up_when_healthy() {
        let now = 1_000_000_000;
        let status = AcmeStatus::new();
        status.record_success(now, now + 60 * DAY);
        let indicator = AcmeHealthIndicator::new(status, 30);
        assert_eq!(indicator.grade(now).status, HealthStatus::Up);
    }

    #[test]
    fn health_up_on_failure_while_cert_still_valid() {
        // A renewal blip must NOT flip health Down while the served cert has
        // plenty of validity left.
        let now = 1_000_000_000;
        let status = AcmeStatus::new();
        status.set_cert_not_after(now + 60 * DAY);
        status.record_failure(now, "temporary CA error");
        let indicator = AcmeHealthIndicator::new(status, 30);
        let out = indicator.grade(now);
        assert_eq!(out.status, HealthStatus::Up);
        assert!(out.details.contains_key("last_failure"));
    }

    #[test]
    fn health_down_on_failure_within_expiry_danger() {
        // A failure AND the cert is inside its renew-before window → Down.
        let now = 1_000_000_000;
        let status = AcmeStatus::new();
        status.set_cert_not_after(now + 5 * DAY);
        status.record_failure(now, "CA outage");
        let indicator = AcmeHealthIndicator::new(status, 30);
        assert_eq!(indicator.grade(now).status, HealthStatus::Down);
    }

    #[test]
    fn health_down_when_no_cert_and_failure() {
        let now = 1_000_000_000;
        let status = AcmeStatus::new();
        status.record_failure(now, "never issued");
        let indicator = AcmeHealthIndicator::new(status, 30);
        assert_eq!(indicator.grade(now).status, HealthStatus::Down);
    }

    #[test]
    fn health_down_when_served_cert_already_expired_without_failure() {
        // An already-expired stored cert served at boot (not_after <= now) must be
        // Down even though NO renewal failure has been recorded yet (leadership
        // skipped/degraded or issuance still pending). Otherwise health hides an
        // already-invalid TLS certificate.
        let now = 1_000_000_000;
        let status = AcmeStatus::new();
        status.set_cert_not_after(now - DAY);
        assert!(
            status.snapshot().last_failure.is_none(),
            "precondition: no failure recorded"
        );
        let indicator = AcmeHealthIndicator::new(status, 30);
        let out = indicator.grade(now);
        assert_eq!(out.status, HealthStatus::Down);
        assert_eq!(
            out.details.get("cert_expired"),
            Some(&serde_json::json!(true))
        );
    }

    #[test]
    fn health_up_when_cert_has_plenty_of_validity_and_no_failure() {
        // A healthy served cert with lots of validity left and no failure is Up,
        // and is NOT flagged as expired.
        let now = 1_000_000_000;
        let status = AcmeStatus::new();
        status.set_cert_not_after(now + 60 * DAY);
        let indicator = AcmeHealthIndicator::new(status, 30);
        let out = indicator.grade(now);
        assert_eq!(out.status, HealthStatus::Up);
        assert!(!out.details.contains_key("cert_expired"));
    }

    #[test]
    fn published_tokens_are_cleared_on_error_path() {
        // Simulate an order body that publishes N tokens and then returns Err
        // (e.g. `set_ready` or `poll_ready` failing). The guard must leave the
        // shared token map empty rather than leaking the published tokens.
        let tokens = Http01Tokens::new();
        // The inner block scopes the guard: it drops at the closing brace, just
        // as the guard in `issue` drops when that function returns `Err`.
        let outcome: Result<(), String> = {
            let mut published = PublishedTokens::new(&tokens);
            published.publish("token-a".to_owned(), "key-a".to_owned());
            published.publish("token-b".to_owned(), "key-b".to_owned());
            // Both are visible while the order is in flight.
            assert_eq!(tokens.get("token-a").as_deref(), Some("key-a"));
            assert_eq!(tokens.get("token-b").as_deref(), Some("key-b"));
            Err("simulated failure after publishing".to_owned())
        };

        assert!(outcome.is_err());
        // Every published token was removed on the error path.
        assert!(tokens.get("token-a").is_none());
        assert!(tokens.get("token-b").is_none());
    }

    #[test]
    fn published_tokens_are_cleared_on_success_path() {
        let tokens = Http01Tokens::new();
        {
            let mut published = PublishedTokens::new(&tokens);
            published.publish("token-a".to_owned(), "key-a".to_owned());
            assert_eq!(tokens.get("token-a").as_deref(), Some("key-a"));
        }
        assert!(tokens.get("token-a").is_none());
    }

    #[test]
    fn due_at_boot_matrix() {
        let now = 1_000_000_000;
        // No real cert yet (still on the placeholder) → order immediately.
        assert!(due_at_boot(None, 30, now));
        // Serving a healthy stored cert (60 days out) → no immediate order.
        assert!(!due_at_boot(Some(now + 60 * DAY), 30, now));
        // Serving a stored cert already inside its renew-before window (5 days
        // out, 30-day window) → order immediately.
        assert!(due_at_boot(Some(now + 5 * DAY), 30, now));
        // Serving a loadable-but-EXPIRED stored cert → order immediately (the
        // bug: previously this waited a full check interval serving a dead cert).
        assert!(due_at_boot(Some(now - DAY), 30, now));
    }

    /// A coordinator whose `try_acquire` always errors — simulates the Postgres
    /// advisory-lock pool being unavailable at first boot.
    struct FailingCoordinator;

    impl SchedulerCoordinator for FailingCoordinator {
        fn backend(&self) -> &'static str {
            "failing"
        }
        fn replica_id(&self) -> &'static str {
            "test-replica"
        }
        fn try_acquire<'a>(
            &'a self,
            _task_name: &'a str,
            _tick_key: &'a str,
            _coordination: TaskCoordination,
        ) -> crate::scheduler::SchedulerFuture<
            'a,
            crate::AutumnResult<Option<crate::scheduler::SchedulerLease>>,
        > {
            Box::pin(async {
                Err(crate::AutumnError::service_unavailable_msg(
                    "advisory-lock pool unavailable",
                ))
            })
        }
    }

    // Regression (#1608, Codex): when the coordinator itself errors, renewal is
    // skipped, but the failure MUST be recorded in AcmeStatus AND dispatched to
    // the reporter — otherwise health can stay `Up` while serving only the
    // self-signed placeholder and operators lose the error signal.
    #[tokio::test]
    async fn coordinator_error_records_failure_and_reports() {
        let domains = vec!["app.example.com".to_owned()];

        // A resolver serving the self-signed placeholder (the pre-issuance state).
        let placeholder = self_signed_placeholder(&domains).expect("placeholder builds");
        let provider = crate::tls::crypto_provider();
        let certified = crate::tls::certified_key_from_pem(
            placeholder.chain_pem.as_bytes(),
            placeholder.key_pem.as_bytes(),
            &provider,
        )
        .expect("placeholder loads");
        let resolver = Arc::new(ReloadableCertResolver::new(certified));

        // The store is never touched on the coordinator-error path, but the task
        // needs one.
        let store_dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn AcmeStore> = Arc::new(crate::acme::store::FsAcmeStore::new(
            store_dir.path(),
            "staging",
        ));

        let status = AcmeStatus::new();
        let config = AcmeConfig {
            domains: domains.clone(),
            contact_email: "ops@example.com".to_owned(),
            directory: crate::config::AcmeDirectory::Staging,
            cache_dir: store_dir.path().to_path_buf(),
            http_challenge_port: 80,
            renew_before_days: 30,
            ca_root_path: None,
            dns: None,
        };
        let task = AcmeRenewalTask {
            resolver,
            provider: crate::tls::crypto_provider(),
            store,
            cert_id: CertId::from_domains(&domains),
            tokens: Http01Tokens::new(),
            status: status.clone(),
            config,
            serving_stored_cert: false,
            leadership_degraded: false,
            renew_window_misconfigured: std::sync::atomic::AtomicBool::new(false),
            dns: None,
            recovery: None,
        };

        // Capture reporter invocations.
        let captured = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink = Arc::clone(&captured);
        let reporter: ReporterFn = Arc::new(move |msg| sink.lock().unwrap().push(msg));

        let coordinator: Arc<dyn SchedulerCoordinator> = Arc::new(FailingCoordinator);
        task.try_renew_once(&coordinator, &reporter).await;

        // The failure is recorded in status...
        let snap = status.snapshot();
        let failure = snap
            .last_failure
            .expect("coordinator error must be recorded as a failure");
        assert!(
            failure.1.contains("leader election failed"),
            "unexpected failure message: {}",
            failure.1
        );

        // ...and dispatched through the reporter exactly once. Clone out so the
        // mutex guard is released immediately.
        let msgs = captured.lock().unwrap().clone();
        assert_eq!(msgs.len(), 1, "reporter must be invoked once");
        assert!(msgs[0].contains("leader election failed"));
    }

    /// A coordinator that records whether leader election was ever attempted,
    /// then returns `Ok(None)` ("another replica leads") so the ordering path
    /// exits without a network round-trip. Lets a test assert whether the
    /// renewal task reached leader election at all.
    struct RecordingCoordinator {
        acquired: Arc<std::sync::atomic::AtomicBool>,
    }

    impl SchedulerCoordinator for RecordingCoordinator {
        fn backend(&self) -> &'static str {
            "in_process"
        }
        fn replica_id(&self) -> &'static str {
            "test-replica"
        }
        fn try_acquire<'a>(
            &'a self,
            _task_name: &'a str,
            _tick_key: &'a str,
            _coordination: TaskCoordination,
        ) -> crate::scheduler::SchedulerFuture<
            'a,
            crate::AutumnResult<Option<crate::scheduler::SchedulerLease>>,
        > {
            self.acquired
                .store(true, std::sync::atomic::Ordering::SeqCst);
            Box::pin(async { Ok(None) })
        }
    }

    /// Build a renewal task serving the self-signed placeholder (pre-issuance
    /// state) with the given `leadership_degraded` flag. The returned `TempDir`
    /// must be kept alive for the store to stay valid.
    fn degraded_test_task(
        leadership_degraded: bool,
    ) -> (AcmeRenewalTask, tempfile::TempDir, AcmeStatus) {
        let domains = vec!["app.example.com".to_owned()];
        let placeholder = self_signed_placeholder(&domains).expect("placeholder builds");
        let provider = crate::tls::crypto_provider();
        let certified = crate::tls::certified_key_from_pem(
            placeholder.chain_pem.as_bytes(),
            placeholder.key_pem.as_bytes(),
            &provider,
        )
        .expect("placeholder loads");
        let resolver = Arc::new(ReloadableCertResolver::new(certified));

        let store_dir = tempfile::tempdir().unwrap();
        let store: Arc<dyn AcmeStore> = Arc::new(crate::acme::store::FsAcmeStore::new(
            store_dir.path(),
            "staging",
        ));

        let status = AcmeStatus::new();
        let config = AcmeConfig {
            domains: domains.clone(),
            contact_email: "ops@example.com".to_owned(),
            directory: crate::config::AcmeDirectory::Staging,
            cache_dir: store_dir.path().to_path_buf(),
            http_challenge_port: 80,
            renew_before_days: 30,
            ca_root_path: None,
            dns: None,
        };
        let task = AcmeRenewalTask {
            resolver,
            provider: crate::tls::crypto_provider(),
            store,
            cert_id: CertId::from_domains(&domains),
            tokens: Http01Tokens::new(),
            status: status.clone(),
            config,
            serving_stored_cert: false,
            leadership_degraded,
            renew_window_misconfigured: std::sync::atomic::AtomicBool::new(false),
            dns: None,
            recovery: None,
        };
        (task, store_dir, status)
    }

    // Regression (#1608, Codex P2): a *distributed* scheduler backend was
    // configured (multi-replica intent) but this process could not build the
    // distributed coordinator and fell back to a per-process in-process one.
    // Ordering would let every replica grab its OWN local lease and race the CA.
    // The renewal task MUST refuse to order — record a failure, dispatch it
    // through the reporter, and never consult the coordinator this cycle.
    #[tokio::test]
    async fn leadership_degraded_refuses_to_order_and_reports() {
        let (task, _store_dir, status) = degraded_test_task(true);

        let captured = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink = Arc::clone(&captured);
        let reporter: ReporterFn = Arc::new(move |msg| sink.lock().unwrap().push(msg));

        // The degraded gate must return before touching the coordinator, so this
        // flag stays false.
        let acquired = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let coordinator: Arc<dyn SchedulerCoordinator> = Arc::new(RecordingCoordinator {
            acquired: Arc::clone(&acquired),
        });
        task.try_renew_once(&coordinator, &reporter).await;

        // No lease was acquired / no order was attempted.
        assert!(
            !acquired.load(std::sync::atomic::Ordering::SeqCst),
            "degraded leadership must NOT acquire a lease or order"
        );

        // The failure is recorded in status...
        let snap = status.snapshot();
        let failure = snap
            .last_failure
            .expect("degraded leadership must be recorded as a failure");
        assert!(
            failure.1.contains("refusing to order"),
            "unexpected failure message: {}",
            failure.1
        );

        // ...and dispatched through the reporter exactly once.
        let msgs = captured.lock().unwrap().clone();
        assert_eq!(msgs.len(), 1, "reporter must be invoked once");
        assert!(msgs[0].contains("refusing to order"));
    }

    // The genuinely single-replica path (in-process coordinator by design) is
    // unaffected by the degraded gate: it proceeds to leader election as normal.
    // A full network order is out of scope for a unit test, so the coordinator
    // returns `Ok(None)`; the assertion is that the gate did NOT short-circuit
    // (the coordinator WAS consulted) and no spurious degraded failure surfaced.
    #[tokio::test]
    async fn single_replica_path_is_unaffected_and_proceeds_to_order() {
        let (task, _store_dir, status) = degraded_test_task(false);

        let captured = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink = Arc::clone(&captured);
        let reporter: ReporterFn = Arc::new(move |msg| sink.lock().unwrap().push(msg));

        let acquired = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let coordinator: Arc<dyn SchedulerCoordinator> = Arc::new(RecordingCoordinator {
            acquired: Arc::clone(&acquired),
        });
        task.try_renew_once(&coordinator, &reporter).await;

        // The renewal task reached leader election rather than short-circuiting.
        assert!(
            acquired.load(std::sync::atomic::Ordering::SeqCst),
            "single-replica path must proceed to leader election / ordering"
        );

        // No degraded failure was recorded and the reporter was not invoked.
        assert!(
            status.snapshot().last_failure.is_none(),
            "single-replica path must not record a degraded failure"
        );
        assert!(
            captured.lock().unwrap().is_empty(),
            "single-replica path must not report a degraded failure"
        );
    }

    /// Build a real, loadable self-signed cert for `app.example.com` (matching
    /// the `CertId` `degraded_test_task` uses) valid until `year-month-day`.
    /// Returns the stored pair and its leaf `notAfter` in UNIX seconds.
    fn cert_valid_until_ymd(year: i32, month: u8, day: u8) -> (StoredCert, i64) {
        use rcgen::{CertificateParams, DistinguishedName, DnType, KeyPair, date_time_ymd};
        let key = KeyPair::generate().expect("keypair");
        let mut params =
            CertificateParams::new(vec!["app.example.com".to_owned()]).expect("params");
        params.not_before = date_time_ymd(2020, 1, 1);
        params.not_after = date_time_ymd(year, month, day);
        let mut dn = DistinguishedName::new();
        dn.push(DnType::CommonName, "app.example.com");
        params.distinguished_name = dn;
        let cert = params.self_signed(&key).expect("self-sign");
        let stored = StoredCert {
            chain_pem: cert.pem(),
            key_pem: key.serialize_pem(),
        };
        let not_after = crate::tls::leaf_not_after_from_pem(stored.chain_pem.as_bytes())
            .expect("leaf notAfter parses");
        (stored, not_after)
    }

    /// A torn stored pair: a valid, future-dated chain paired with a FRESH,
    /// MISMATCHED key (as a crash between `save_cert`'s two renames would leave —
    /// a new chain with the old/unrelated key).
    fn torn_cert_future() -> StoredCert {
        let (valid, _) = cert_valid_until_ymd(2200, 1, 1);
        let other = rcgen::KeyPair::generate().expect("mismatched keypair");
        StoredCert {
            chain_pem: valid.chain_pem,
            key_pem: other.serialize_pem(),
        }
    }

    // FIX 1 (#1608, Codex P2): a strictly-newer, fully-loadable stored cert is
    // hot-swapped into the resolver on adoption, independent of whether renewal
    // is due — so a follower / a process that did not issue picks it up at once.
    #[tokio::test]
    async fn adopt_swaps_in_a_strictly_newer_stored_cert() {
        let (task, _dir, status) = degraded_test_task(false);
        let initial = task.resolver.current();
        let (newer, newer_na) = cert_valid_until_ymd(2200, 1, 1);
        // The served baseline is OLDER than the stored cert.
        status.set_cert_not_after(newer_na - 100 * DAY);
        task.store.save_cert(&task.cert_id, &newer).await.unwrap();

        task.adopt_stored_cert_if_newer().await;

        assert!(
            !Arc::ptr_eq(&task.resolver.current(), &initial),
            "a strictly-newer stored cert must be swapped into the resolver"
        );
        assert_eq!(status.snapshot().cert_not_after_unix, Some(newer_na));
    }

    // FIX 1: adoption must never DOWNGRADE — an older stored cert than the one we
    // serve is left in place.
    #[tokio::test]
    async fn adopt_does_not_downgrade_to_an_older_stored_cert() {
        let (task, _dir, status) = degraded_test_task(false);
        let initial = task.resolver.current();
        let (older, older_na) = cert_valid_until_ymd(2100, 1, 1);
        // The served baseline is NEWER than the stored cert.
        let served = older_na + 100 * DAY;
        status.set_cert_not_after(served);
        task.store.save_cert(&task.cert_id, &older).await.unwrap();

        task.adopt_stored_cert_if_newer().await;

        assert!(
            Arc::ptr_eq(&task.resolver.current(), &initial),
            "must not downgrade to an older stored cert"
        );
        assert_eq!(
            status.snapshot().cert_not_after_unix,
            Some(served),
            "served expiry must be unchanged"
        );
    }

    // FIX 1: no stored cert at all → nothing to adopt, resolver untouched.
    #[tokio::test]
    async fn adopt_no_swap_when_store_is_empty() {
        let (task, _dir, status) = degraded_test_task(false);
        let initial = task.resolver.current();
        status.set_cert_not_after(1_000_000_000);

        task.adopt_stored_cert_if_newer().await;

        assert!(Arc::ptr_eq(&task.resolver.current(), &initial));
        assert_eq!(status.snapshot().cert_not_after_unix, Some(1_000_000_000));
    }

    // FIX 1 (integration): `maybe_renew` adopts a newer stored cert on a tick even
    // when renewal is NOT due — and, because the adopted cert is far from expiry,
    // it does NOT proceed to leader election / ordering.
    #[tokio::test]
    async fn maybe_renew_adopts_newer_cert_without_ordering_when_not_due() {
        let (task, _dir, status) = degraded_test_task(false);
        let initial = task.resolver.current();
        let (newer, newer_na) = cert_valid_until_ymd(2200, 1, 1);
        status.set_cert_not_after(newer_na - 100 * DAY);
        task.store.save_cert(&task.cert_id, &newer).await.unwrap();

        let reporter: ReporterFn = Arc::new(|_| {});
        let acquired = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let coordinator: Arc<dyn SchedulerCoordinator> = Arc::new(RecordingCoordinator {
            acquired: Arc::clone(&acquired),
        });

        task.maybe_renew(&coordinator, &reporter).await;

        // The newer cert was adopted into the resolver...
        assert!(
            !Arc::ptr_eq(&task.resolver.current(), &initial),
            "maybe_renew must adopt a newer stored cert every tick"
        );
        assert_eq!(status.snapshot().cert_not_after_unix, Some(newer_na));
        // ...and renewal was NOT due, so the coordinator was never consulted.
        assert!(
            !acquired.load(std::sync::atomic::Ordering::SeqCst),
            "adoption must not trigger ordering when renewal is not due"
        );
    }

    // FIX 2 (#1608, Codex P2): a torn/mismatched stored pair (valid future-dated
    // chain + wrong key) must count as ABSENT for the renewal decision, so a
    // follower does not treat the unusable future-dated chain as healthy and
    // stall on the placeholder/old cert.
    #[tokio::test]
    async fn stored_not_after_is_absent_for_a_torn_pair() {
        let (task, _dir, _status) = degraded_test_task(false);
        let torn = torn_cert_future();
        // The chain alone parses a future notAfter (the trap the old code fell
        // into)...
        assert!(
            crate::tls::leaf_not_after_from_pem(torn.chain_pem.as_bytes()).is_ok(),
            "precondition: the torn chain is itself a valid, future-dated leaf"
        );
        task.store.save_cert(&task.cert_id, &torn).await.unwrap();

        // ...but the renewal decision treats the unusable PAIR as absent.
        assert!(
            task.stored_not_after().await.is_none(),
            "a torn/mismatched pair must count as absent for the renewal decision"
        );
    }

    // FIX 2: a valid matching pair reports its expiry normally (healthy path).
    #[tokio::test]
    async fn stored_not_after_is_present_for_a_valid_pair() {
        let (task, _dir, _status) = degraded_test_task(false);
        let (valid, na) = cert_valid_until_ymd(2200, 1, 1);
        task.store.save_cert(&task.cert_id, &valid).await.unwrap();

        assert_eq!(
            task.stored_not_after().await,
            Some(na),
            "a valid matching pair reports its leaf notAfter normally"
        );
    }

    // FIX 1 (#1608, Codex P2): a freshly-issued certificate that STILL immediately
    // satisfies `needs_renewal` means the renew-before window is >= the cert's own
    // lifetime. The loop must record a failure, report it, set the backstop flag,
    // and — critically — NOT re-order on the next tick (no tight loop that burns CA
    // rate limits).
    #[tokio::test]
    async fn backstop_refuses_reorder_when_fresh_cert_still_needs_renewal() {
        // Default renew window is 30 days; simulate an issued cert that expires in
        // only 1 day, so `needs_renewal` is immediately true.
        let (task, _dir, status) = degraded_test_task(false);

        let captured = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let sink = Arc::clone(&captured);
        let reporter: ReporterFn = Arc::new(move |msg| sink.lock().unwrap().push(msg));

        // Feed the post-issuance handler a "just issued" cert whose notAfter is
        // inside the renew window — exactly what `issue()` would return for a
        // misconfigured window.
        task.handle_issue_outcome(Ok(now_unix() + DAY), &reporter);

        // A failure is recorded and dispatched, naming the misconfiguration.
        let snap = status.snapshot();
        let failure = snap
            .last_failure
            .expect("a fresh cert still due for renewal must record a failure");
        assert!(
            failure.1.contains("refusing to re-order"),
            "unexpected failure message: {}",
            failure.1
        );
        let msgs = captured.lock().unwrap().clone();
        assert_eq!(msgs.len(), 1, "reporter must be invoked once");
        assert!(msgs[0].contains("refusing to re-order"));

        // The backstop flag is now set.
        assert!(
            task.renew_window_misconfigured
                .load(std::sync::atomic::Ordering::SeqCst),
            "the misconfigured-window backstop must be set"
        );

        // A subsequent tick must NOT order, even though the store is empty (which
        // otherwise counts as due): the coordinator is never consulted.
        let acquired = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let coordinator: Arc<dyn SchedulerCoordinator> = Arc::new(RecordingCoordinator {
            acquired: Arc::clone(&acquired),
        });
        task.maybe_renew(&coordinator, &reporter).await;
        assert!(
            !acquired.load(std::sync::atomic::Ordering::SeqCst),
            "a backed-off loop must NOT re-order (no tight loop)"
        );
    }

    // FIX 1 companion: a healthy issuance (cert far from expiry) must NOT trip the
    // backstop — the loop stays free to renew normally when the next window opens.
    #[tokio::test]
    async fn healthy_issuance_does_not_trip_backstop() {
        let (task, _dir, status) = degraded_test_task(false);
        let reporter: ReporterFn = Arc::new(|_| {});

        // Issued cert with 60 days of validity, 30-day window → not immediately due.
        task.handle_issue_outcome(Ok(now_unix() + 60 * DAY), &reporter);

        let snap = status.snapshot();
        assert!(
            snap.last_failure.is_none(),
            "a healthy issuance must not record a failure"
        );
        assert!(
            snap.last_success_unix.is_some(),
            "a healthy issuance must record success"
        );
        assert!(
            !task
                .renew_window_misconfigured
                .load(std::sync::atomic::Ordering::SeqCst),
            "a healthy issuance must not trip the backstop"
        );

        // And the loop is free to order when due (empty store → due) — the
        // coordinator IS consulted.
        let acquired = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let coordinator: Arc<dyn SchedulerCoordinator> = Arc::new(RecordingCoordinator {
            acquired: Arc::clone(&acquired),
        });
        task.maybe_renew(&coordinator, &reporter).await;
        assert!(
            acquired.load(std::sync::atomic::Ordering::SeqCst),
            "a healthy issuance must leave the loop free to renew"
        );
    }
}
