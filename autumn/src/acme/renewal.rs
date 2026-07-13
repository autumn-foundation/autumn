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
use crate::acme::store::{AcmeStore, CertId, StoredCert};
use crate::config::AcmeConfig;
use crate::scheduler::SchedulerCoordinator;
use crate::task::TaskCoordination;
use crate::tls::ReloadableCertResolver;

/// How often the renewal loop wakes to re-check expiry. An hour is frequent
/// enough to act well inside the (default 30-day) renew-before window while
/// costing almost nothing.
const RENEWAL_CHECK_INTERVAL: Duration = Duration::from_secs(3600);

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
        }
    }

    /// Grade the current status against `now_unix` (pure; used by `check` and
    /// unit tests).
    #[must_use]
    pub fn grade(&self, now_unix: i64) -> crate::actuator::HealthCheckOutput {
        use crate::actuator::{HealthCheckOutput, HealthStatus};
        let snap = self.status.snapshot();
        let mut details = std::collections::HashMap::new();

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

        // Down ONLY when a failure coincides with real expiry danger: the served
        // certificate is already inside its renew-before window (or we have no
        // certificate at all). A failure with plenty of validity left is a blip.
        let in_danger = snap
            .cert_not_after_unix
            .is_none_or(|not_after| needs_renewal(not_after, self.renew_before_days, now_unix));
        let status = if snap.last_failure.is_some() && in_danger {
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
        .map(|d| i64::try_from(d.as_secs()).unwrap_or(i64::MAX))
        .unwrap_or(0)
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
        // No stored cert (still on the placeholder) → always due.
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
                let msg = format!("ACME renewal leader election failed: {e}");
                tracing::warn!("{msg}");
                return;
            }
        };

        let outcome = self.issue().await;
        // Always release the lease, regardless of outcome.
        if let Err(e) = lease.release().await {
            tracing::warn!(error = %e, "failed to release ACME renewal lease");
        }

        match outcome {
            Ok(not_after) => {
                self.status.record_success(now_unix(), not_after);
                tracing::info!(
                    cert_id = self.cert_id.as_str(),
                    "ACME certificate issued/renewed and hot-swapped into the TLS listener"
                );
            }
            Err(e) => {
                let msg = format!("ACME certificate issuance failed: {e}");
                tracing::error!("{msg}");
                self.status.record_failure(now_unix(), msg.clone());
                reporter(msg);
            }
        }
    }

    /// The stored certificate's leaf `notAfter`, if one is persisted.
    async fn stored_not_after(&self) -> Option<i64> {
        let stored = self.store.load_cert(&self.cert_id).await.ok().flatten()?;
        crate::tls::leaf_not_after_from_pem(stored.chain_pem.as_bytes()).ok()
    }

    /// If a shared store holds a newer certificate than we serve, adopt it.
    async fn adopt_stored_cert_if_newer(&self) {
        let Some(stored) = self.store.load_cert(&self.cert_id).await.ok().flatten() else {
            return;
        };
        let Ok(not_after) = crate::tls::leaf_not_after_from_pem(stored.chain_pem.as_bytes()) else {
            return;
        };
        let serving = self.status.snapshot().cert_not_after_unix;
        if serving != Some(not_after)
            && let Ok(certified) = crate::tls::certified_key_from_pem(
                stored.chain_pem.as_bytes(),
                stored.key_pem.as_bytes(),
                &self.provider,
            )
        {
            self.resolver.store(certified);
            self.status.set_cert_not_after(not_after);
            tracing::info!("adopted a newer ACME certificate from the shared store");
        }
    }

    /// The full HTTP-01 issuance flow: order → challenge → finalize → persist →
    /// hot-swap. Returns the issued leaf's `notAfter` on success.
    async fn issue(&self) -> Result<i64, String> {
        use instant_acme::{
            AuthorizationStatus, ChallengeType, Identifier, NewOrder, OrderStatus, RetryPolicy,
        };

        let account = self.load_or_register_account().await?;

        let identifiers: Vec<Identifier> = self
            .config
            .domains
            .iter()
            .map(|d| Identifier::Dns(d.clone()))
            .collect();
        let mut order = account
            .new_order(&NewOrder::new(&identifiers))
            .await
            .map_err(|e| format!("failed to create ACME order: {e}"))?;

        // Publish an HTTP-01 response for each pending authorization. The RAII
        // guard removes every published token from the shared map on ALL exit
        // paths (early `?` inside this scope, the `poll_ready` error below, or
        // normal completion), so a transient failure cannot leak tokens that
        // accumulate across repeated renewal attempts.
        let mut published = PublishedTokens::new(&self.tokens);
        {
            let mut authorizations = order.authorizations();
            while let Some(result) = authorizations.next().await {
                let mut authz =
                    result.map_err(|e| format!("failed to fetch authorization: {e}"))?;
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
        }

        // Wait for the order to become ready. The guard clears published tokens
        // when it drops at the end of `issue`, on both the error and Ok paths.
        let status = order
            .poll_ready(&RetryPolicy::default())
            .await
            .map_err(|e| format!("order did not become ready: {e}"))?;
        if status != OrderStatus::Ready {
            return Err(format!("ACME order ended in unexpected state {status:?}"));
        }

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

    /// Load the persisted ACME account, or register a fresh one and persist it.
    async fn load_or_register_account(&self) -> Result<instant_acme::Account, String> {
        use instant_acme::{Account, AccountCredentials, NewAccount};

        let directory_url = crate::acme::directory_url(&self.config.directory);

        if let Some(bytes) = self
            .store
            .load_account()
            .await
            .map_err(|e| format!("failed to read stored ACME account: {e}"))?
        {
            let credentials: AccountCredentials = serde_json::from_slice(&bytes)
                .map_err(|e| format!("stored ACME account is corrupt: {e}"))?;
            let account = Account::builder()
                .map_err(|e| format!("failed to build ACME client: {e}"))?
                .from_credentials(credentials)
                .await
                .map_err(|e| format!("failed to restore ACME account: {e}"))?;
            return Ok(account);
        }

        let contact = format!("mailto:{}", self.config.contact_email.trim());
        let contacts = [contact.as_str()];
        let (account, credentials) = Account::builder()
            .map_err(|e| format!("failed to build ACME client: {e}"))?
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

/// Current UNIX time in seconds.
fn now_unix() -> i64 {
    default_now_unix()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actuator::HealthStatus;

    const DAY: i64 = 86_400;

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
}
