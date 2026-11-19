//! Per-tenant custom-domain issuance and renewal (issue #1635).
//!
//! [`crate::custom_domain`] owns the registry, the verification gate and the
//! issuance budget. This module is the half that needs a CA: the
//! [`AcmeDomainIssuer`] that orders one certificate per tenant hostname, and
//! the [`CustomDomainTask`] loop that drives every registered domain through
//! verify → issue → renew and offboards the ones an app removes.
//!
//! # Composition, not a second ACME client
//!
//! Orders reuse #1608's account, store and HTTP-01 token map through the
//! helpers extracted in [`crate::acme::renewal`]. One ACME account covers the
//! deployment's own certificate and every tenant's, so the CA sees one
//! registration however many domains connect.
//!
//! # Why HTTP-01 only
//!
//! A tenant's domain lives in the *tenant's* zone, so autumn holds no
//! credential that could write a DNS-01 `_acme-challenge` record there. The
//! verification gate already proves the hostname points at this deployment
//! (and, with the TXT token, that the tenant controls it), which is exactly
//! the condition that makes HTTP-01 work — so the challenge
//! the operator can actually answer is the one used, even when the
//! deployment's own certificate is issued over DNS-01.
//!
//! # Abuse posture
//!
//! Every order passes three gates first: the hostname is registered by the
//! app, DNS independently points here and carries the registration's TXT
//! token, and the
//! [budget](crate::custom_domain::IssuanceLimiter) has headroom. An SNI
//! hostname failing the first never reaches this module at all —
//! [`SniCertResolver`](crate::custom_domain::SniCertResolver) refuses the
//! handshake.

use std::sync::Arc;

use crate::acme::challenge::Http01Tokens;
use crate::acme::store::{AcmeStore, CertId, StoredCert};
use crate::config::AcmeConfig;
use crate::custom_domain::{
    CustomDomainCertCache, CustomDomainRegistry, DomainIssuer, DomainVerifier, ExpectedIngress,
    IssuanceLimiter, IssuedCertificate, apply_verification, grade_dns_verification,
    grade_ownership,
};
use crate::scheduler::SchedulerCoordinator;
use crate::task::TaskCoordination;
use rustls::crypto::CryptoProvider;

/// The scheduled-task name custom-domain leases and alerts are keyed on.
pub const CUSTOM_DOMAIN_TASK: &str = "custom_domain_certificates";

/// How many pending domains one tick verifies at the same time.
const VERIFY_CONCURRENCY: usize = 16;

/// How long one renewal DNS lookup may run before it counts as a blip.
const RECHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

/// How long one verification may run. A longer one is a failure with backoff.
///
/// Without it, hung names sort first, fill every slot each tick, and are
/// never backed off. Domains behind them would never be checked.
const VERIFY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(20);

/// Verification time per tick when the caller sets no budget.
const DEFAULT_VERIFY_BUDGET: std::time::Duration = std::time::Duration::from_secs(30);

/// Callback that dispatches a custom-domain failure to the operator (#1610).
pub type ReporterFn = Arc<dyn Fn(String) + Send + Sync>;

/// Callback that clears an outstanding custom-domain alert once every domain
/// is healthy again.
pub type RecoveryFn = Arc<dyn Fn() + Send + Sync>;

/// The [`CertId`] a tenant hostname's certificate is stored under.
///
/// One hostname per certificate, so offboarding one tenant deletes exactly one
/// pair and a renewal for one domain cannot invalidate another's.
#[must_use]
pub fn cert_id_for(hostname: &str) -> CertId {
    CertId::from_domains(&[hostname.to_owned()])
}

/// `base_unix` plus the monotonic time since `started`.
///
/// Used instead of a fresh wall-clock read so a caller-injected `now_unix`
/// (tests, simulation) stays the single source of time while a long pass still
/// advances past the instant it began.
fn elapsed_since(base_unix: i64, started: tokio::time::Instant) -> i64 {
    base_unix.saturating_add(i64::try_from(started.elapsed().as_secs()).unwrap_or(i64::MAX))
}

// ── The ACME issuer ──────────────────────────────────────────────────────

/// Orders one certificate per tenant hostname over HTTP-01.
pub struct AcmeDomainIssuer {
    config: AcmeConfig,
    store: Arc<dyn AcmeStore>,
    tokens: Http01Tokens,
}

impl std::fmt::Debug for AcmeDomainIssuer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AcmeDomainIssuer").finish_non_exhaustive()
    }
}

impl AcmeDomainIssuer {
    /// An issuer sharing `store` (hence the ACME account) and the `:80`
    /// challenge listener's `tokens` with the deployment's own renewal loop.
    #[must_use]
    pub const fn new(config: AcmeConfig, store: Arc<dyn AcmeStore>, tokens: Http01Tokens) -> Self {
        Self {
            config,
            store,
            tokens,
        }
    }

    async fn order(&self, hostname: &str) -> Result<IssuedCertificate, String> {
        use instant_acme::{Identifier, NewOrder};

        let account =
            crate::acme::renewal::load_or_register_account(self.store.as_ref(), &self.config)
                .await?;
        let identifiers = [Identifier::Dns(hostname.to_owned())];
        let mut order = account
            .new_order(&NewOrder::new(&identifiers))
            .await
            .map_err(|e| format!("failed to create ACME order for {hostname}: {e}"))?;

        // The token stays published until the order settles: `set_ready` only
        // queues validation, so tearing it down earlier removes the answer
        // while the CA is still looking at it.
        let published = crate::acme::renewal::answer_http01(&self.tokens, &mut order).await?;
        let ready = crate::acme::renewal::await_order_ready(&mut order, None).await;
        drop(published);
        ready?;

        let (csr_der, key_pem) = crate::acme::renewal::generate_csr(&[hostname.to_owned()])?;
        order
            .finalize_csr(&csr_der)
            .await
            .map_err(|e| format!("failed to finalize order for {hostname}: {e}"))?;
        let chain_pem = order
            .poll_certificate(&instant_acme::RetryPolicy::default())
            .await
            .map_err(|e| format!("failed to download certificate for {hostname}: {e}"))?;
        Ok(IssuedCertificate { chain_pem, key_pem })
    }
}

impl DomainIssuer for AcmeDomainIssuer {
    fn issue<'a>(
        &'a self,
        hostname: &'a str,
    ) -> futures::future::BoxFuture<'a, Result<IssuedCertificate, String>> {
        Box::pin(self.order(hostname))
    }
}

// ── The orchestrator ─────────────────────────────────────────────────────

/// Drives every registered custom domain through its lifecycle.
///
/// One tick does three passes, in order: verify the domains still awaiting
/// DNS, order for the ones now verified, and renew the active ones inside
/// their renew-before window. Each domain is handled independently — a failure
/// records a reason and a backoff on that domain only, so nothing one tenant
/// does can stop another being issued, renewed or served.
pub struct CustomDomainTask {
    /// The hostname → tenant registry, and the lifecycle state it holds.
    pub registry: Arc<CustomDomainRegistry>,
    /// The bounded cache the SNI resolver reads.
    pub cache: Arc<CustomDomainCertCache>,
    /// Certificate persistence — the same store the deployment's own
    /// certificate and the ACME account live in.
    pub certs: Arc<dyn AcmeStore>,
    /// The crypto provider shared with the TLS listener.
    pub provider: Arc<CryptoProvider>,
    /// Where a hostname currently points.
    pub verifier: Arc<dyn DomainVerifier>,
    /// How a certificate is obtained.
    pub issuer: Arc<dyn DomainIssuer>,
    /// Per-domain and deployment-wide order budgets.
    pub limiter: Arc<IssuanceLimiter>,
    /// What tenants are told to point DNS at.
    pub ingress: ExpectedIngress,
    /// Renew once a certificate has fewer than this many days left.
    pub renew_before_days: u32,
    /// Where a failure is reported (#1610's failed-scheduled-operation alert).
    pub reporter: ReporterFn,
    /// Invoked once no domain carries a failure any more, so the operator
    /// alert the reporter raised is cleared rather than left standing.
    pub recovery: Option<RecoveryFn>,
    /// Leader election, so only one replica orders per domain.
    ///
    /// The same coordinator the deployment's own renewal uses. Without it every
    /// replica orders every tenant's certificate against the one shared ACME
    /// account.
    pub coordinator: Arc<dyn SchedulerCoordinator>,
    /// Set when a distributed scheduler backend was configured but this process
    /// could not build its coordinator. Mirrors `AcmeRenewalTask`: ordering
    /// under a per-process lease would race every other replica.
    pub leadership_degraded: bool,
    /// The certificate store as a filesystem store, when it is one, so the
    /// retention prune can enumerate stored pairs. `None` disables orphan
    /// pruning rather than guessing at another store's layout.
    pub cert_store_paths: Option<Arc<crate::acme::store::FsAcmeStore>>,
    /// Certificate ids the prune must never delete — the deployment's own
    /// certificate, which shares this store but has no registry record.
    pub retained_cert_ids: std::collections::HashSet<String>,
}

impl std::fmt::Debug for CustomDomainTask {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CustomDomainTask")
            .field("domains", &self.registry.len())
            .field("renew_before_days", &self.renew_before_days)
            .finish_non_exhaustive()
    }
}

// ── The prune-only half ────────────────────────────────────────────────────

/// The retention-prune half of [`CustomDomainTask`]: everything the
/// [`CustomDomainPruner`](crate::custom_domain::CustomDomainPruner) trait
/// needs, and none of the issuance/renewal orchestration.
///
/// The server installs this — not the task — as the `dyn CustomDomainPruner`
/// state extension, and the one-shot `autumn db retention` path installs the
/// same type. Pruning only touches the registry and the certificate store, so
/// the CLI never builds an ACME issuer: no order is placed and no CA is
/// contacted (#2652).
pub struct PruneOnlyCustomDomainPruner {
    /// The hostname → tenant registry, and the lifecycle state it holds.
    pub registry: Arc<CustomDomainRegistry>,
    /// The bounded cache the SNI resolver reads; offboarding evicts from it.
    pub cache: Arc<CustomDomainCertCache>,
    /// Certificate persistence, for deleting an offboarded pair.
    pub certs: Arc<dyn AcmeStore>,
    /// Per-domain issuance budgets; offboarding forgets the hostname.
    pub limiter: Arc<IssuanceLimiter>,
    /// The certificate store as a filesystem store, when it is one, so the
    /// retention prune can enumerate stored pairs. `None` disables orphan
    /// pruning rather than guessing at another store's layout.
    pub cert_store_paths: Option<Arc<crate::acme::store::FsAcmeStore>>,
    /// Certificate ids the prune must never delete — the deployment's own
    /// certificate, which shares this store but has no registry record.
    pub retained_cert_ids: std::collections::HashSet<String>,
    /// Where a failed certificate delete is reported (#1610's
    /// failed-scheduled-operation alert).
    pub reporter: ReporterFn,
    /// Invoked once no domain carries a failure any more, so the operator
    /// alert the reporter raised is cleared rather than left standing.
    pub recovery: Option<RecoveryFn>,
}

impl PruneOnlyCustomDomainPruner {
    /// Offboard `hostname`: stop routing, stop serving, halt renewal, and
    /// delete the stored certificate so it is not orphaned (AC7, #1605).
    ///
    /// # Errors
    ///
    /// Propagates a registry-store error. A certificate that cannot be deleted
    /// is logged rather than failing the offboarding: the domain is already
    /// unroutable and unservable by then, so leaving it registered would be
    /// worse.
    pub async fn offboard(&self, hostname: &str) -> std::io::Result<bool> {
        // Normalise here, not only in the registry: the cache key, budget history and
        // certificate id derive from this string, so `offboard("APP.ClientCo.com.")`
        // would otherwise drop the record and leave the private key on disk.
        let Ok(host) = crate::custom_domain::normalize_hostname(hostname) else {
            return Ok(false);
        };
        self.offboard_guarded(&host, |_| true).await
    }

    /// Remove one hostname while `guard` holds for the stored record, then drop
    /// what this process holds for it.
    ///
    /// Every offboarding path goes through here. The removal and the purge run
    /// in one gate section. The guard runs in it too. It checks the record as
    /// it is now, not as it was before an `.await`. A retention sweep or a
    /// tenant teardown therefore never disconnects a new owner.
    async fn offboard_guarded(
        &self,
        host: &str,
        guard: impl FnOnce(&crate::custom_domain::CustomDomain) -> bool,
    ) -> std::io::Result<bool> {
        // One critical section: the record, the cache slot and the certificate
        // go together. A successor cannot register, or save its certificate,
        // until the purge is done, so the purge cannot take its pair.
        let Some(outcome) = self
            .registry
            .with_hostname(host, async |gate| {
                let removed = gate.remove_if(guard).await?;
                // Purge only while the hostname is nobody's: after a rejected
                // guard, the record belongs to a live domain.
                let purged = if gate.get().is_none() {
                    self.purge_local(host).await
                } else {
                    true
                };
                std::io::Result::Ok((removed, purged))
            })
            .await
        else {
            return Ok(false);
        };
        let (removed, purged) = outcome?;
        // A failed purge has raised its own alert about a private key left on disk.
        // Keep it even if the registry now looks healthy: no retention sweep runs by
        // default, so nothing else points at that key.
        if removed && purged {
            self.clear_alert_if_healthy(crate::custom_domain::now_unix());
        }
        Ok(removed)
    }

    /// Offboard `hostname` only while it is still the record `expected`
    /// describes — same tenant, same status, same registration time.
    ///
    /// The retention sweep's candidates come from a snapshot taken before a
    /// series of awaits; this re-asserts each one inside the registry's
    /// per-hostname write gate, so a domain that finished setup in the
    /// meantime is left alone. Nothing local is purged when the guard rejects:
    /// the certificate belongs to a domain that is now live.
    async fn offboard_if_unchanged(
        &self,
        expected: &crate::custom_domain::CustomDomain,
    ) -> std::io::Result<bool> {
        let Ok(host) = crate::custom_domain::normalize_hostname(&expected.hostname) else {
            return Ok(false);
        };
        let removed = self
            .offboard_guarded(&host, |current| {
                current.tenant == expected.tenant
                    && current.status == expected.status
                    && current.registered_at_unix == expected.registered_at_unix
            })
            .await?;
        if !removed {
            tracing::debug!(
                hostname = %host,
                "skipping a retention offboard: the domain changed while the sweep was running"
            );
        }
        Ok(removed)
    }

    /// Drop everything this process holds for an offboarded hostname.
    ///
    /// A certificate that cannot be deleted does not fail the offboarding: the
    /// domain is already unroutable and unservable, and reporting failure would
    /// suggest none of it happened. It is ALERTED instead — an offboarded
    /// tenant's private key left on disk is an operator's problem, and nothing
    /// else retries it: the orphan prune only runs when a `[retention]
    /// custom_domains` window is configured, and it is unset by default.
    /// Returns whether everything local actually went.
    ///
    /// Runs inside the hostname's gate. The reporter must not call the
    /// registry.
    async fn purge_local(&self, host: &str) -> bool {
        self.cache.remove(host);
        self.limiter.forget(host);
        if let Err(e) = self.certs.delete_cert(&cert_id_for(host)).await {
            let message = format!(
                "offboarded custom domain {host} but could not delete its certificate: {e}. The \
                 certificate and private key are still on disk"
            );
            tracing::warn!("{message}");
            (self.reporter)(message);
            return false;
        }
        true
    }

    /// Retract the operator alert once NOTHING is failing any more.
    ///
    /// Offboarding the one domain that was failing removes the failure from the
    /// registry but is not itself a success, so without this the
    /// `scheduled_task_failure` alert it raised would stand for a domain that
    /// no longer exists.
    fn clear_alert_if_healthy(&self, now_unix: i64) {
        if let Some(recovery) = &self.recovery
            && self.registry.health_report(now_unix).is_empty()
        {
            recovery();
        }
    }

    /// Offboard every domain a tenant owns. Returns how many were removed.
    ///
    /// The hostname list is a snapshot and each offboard awaits, so every
    /// removal re-asserts the tenant: a hostname freed early in the sweep can
    /// be re-registered by ANOTHER tenant before a later one runs, and removing
    /// it unconditionally would disconnect a tenant who was never part of this
    /// teardown.
    ///
    /// # Errors
    ///
    /// Propagates the registry store's delete error.
    pub async fn offboard_tenant(&self, tenant: &str) -> std::io::Result<usize> {
        let hostnames: Vec<String> = self
            .registry
            .list_for_tenant(tenant)
            .into_iter()
            .map(|d| d.hostname)
            .collect();
        let mut removed = 0;
        for hostname in hostnames {
            let Ok(host) = crate::custom_domain::normalize_hostname(&hostname) else {
                continue;
            };
            if self
                .offboard_guarded(&host, |current| current.tenant == tenant)
                .await?
            {
                removed += 1;
            }
        }
        Ok(removed)
    }

    /// Delete stored certificate pairs no registered hostname maps to.
    ///
    /// Only certificates whose [`CertId`] matches some *removed* custom domain
    /// can be identified: an id is a hash, so the deployment's own certificate
    /// (and any other) is left alone by construction — we delete only ids that
    /// no longer appear in the registry AND are not the configured cert.
    async fn prune_orphan_certs(&self, dry_run: bool) -> Result<u64, String> {
        let Some(fs) = self.cert_store_paths.as_ref() else {
            // A non-filesystem store cannot be enumerated here.
            return Ok(0);
        };
        // Enumerate the store before snapshotting the registry. In the reverse order, a
        // certificate issued in between is in `stored` but not in `live`, and is deleted
        // from under a domain going active. This way a pair written after the enumeration
        // is not a candidate, and one written before it has a record the snapshot sees.
        let stored = fs
            .list_certs()
            .map_err(|e| format!("failed to enumerate stored certificates: {e}"))?;
        // The live set unions the in-memory index with a fresh read of the store. The
        // index is only as current as this process's writes: a one-shot
        // `autumn db retention` loaded it at startup, and a serving process sharing the
        // store may have registered and issued a hostname since. A record either view
        // knows keeps its certificate.
        let fresh = self
            .registry
            .stored_records()
            .await
            .map_err(|e| format!("failed to re-read the custom-domain registry: {e}"))?;
        let live_ids = |extra: Vec<crate::custom_domain::CustomDomain>| {
            self.registry
                .list()
                .into_iter()
                .chain(extra)
                .map(|d| cert_id_for(&d.hostname).as_str().to_owned())
                .collect::<std::collections::HashSet<String>>()
        };
        let live = live_ids(fresh.clone());
        let candidates: Vec<_> = stored
            .into_iter()
            .map(|(id, _, _)| id)
            .filter(|id| {
                !self.retained_cert_ids.contains(id.as_str()) && !live.contains(id.as_str())
            })
            .collect();
        if dry_run {
            return Ok(candidates.len() as u64);
        }
        // A cert id is a hash, so the hostname is unknown and no per-hostname gate fits:
        // delete under `exclusive`, which keeps installs and offboards out. One section
        // per id, so writers wait for one delete, not the whole sweep. The live set is
        // re-read inside: a hostname registered since the snapshot keeps its pair.
        let mut removed = 0_u64;
        for id in &candidates {
            let deleted = self
                .registry
                .exclusive(async || {
                    if live_ids(fresh.clone()).contains(id.as_str()) {
                        return false;
                    }
                    // The store deletes the key before the chain: `list_certs` finds a
                    // pair by its chain, so the reverse order could strand a key.
                    match self.certs.delete_cert(id).await {
                        Ok(()) => true,
                        Err(e) => {
                            tracing::warn!(
                                id = id.as_str(),
                                "failed to remove an orphaned certificate: {e}"
                            );
                            false
                        }
                    }
                })
                .await;
            if deleted {
                removed += 1;
            }
        }
        Ok(removed)
    }
}

impl crate::custom_domain::CustomDomainPruner for PruneOnlyCustomDomainPruner {
    fn prune(
        &self,
        cutoff_unix: i64,
        dry_run: bool,
    ) -> futures::future::BoxFuture<'_, Result<u64, String>> {
        Box::pin(async move {
            // An index that failed to hydrate makes every tenant certificate look
            // orphaned, so one read error at boot would delete every private key.
            if !self.registry.is_hydrated() {
                return Err(
                    "refusing to prune: the custom-domain registry did not load at boot, so \
                     every certificate would look orphaned"
                        .to_owned(),
                );
            }
            // A record that did not load leaves its certificate looking
            // orphaned, and the prune would delete a live tenant's key.
            let skipped = self.registry.skipped_records();
            if !skipped.is_empty() {
                return Err(format!(
                    "refusing to prune: {} custom-domain record(s) will not load ({}), so their \
                     certificates would look orphaned",
                    skipped.len(),
                    skipped.join(", ")
                ));
            }
            let mut removed = 0_u64;
            // Re-read the store: another process sharing it (a serving app while a
            // one-shot `autumn db retention` runs) may have verified or re-registered a
            // hostname since this index loaded. A candidate must still be the same
            // abandoned registration in the store.
            let fresh: std::collections::HashMap<String, crate::custom_domain::CustomDomain> = self
                .registry
                .stored_records()
                .await
                .map_err(|e| format!("failed to re-read the custom-domain registry: {e}"))?
                .into_iter()
                .map(|d| (d.hostname.clone(), d))
                .collect();
            // Abandoned connections: a tenant was handed DNS instructions and
            // never published the record. Nothing else ever deletes these.
            for domain in self.registry.list() {
                let still_abandoned_in_store = fresh.get(&domain.hostname).is_some_and(|stored| {
                    stored.status == domain.status
                        && stored.tenant == domain.tenant
                        && stored.registered_at_unix == domain.registered_at_unix
                });
                if domain.status == crate::custom_domain::DomainStatus::PendingDns
                    && domain.registered_at_unix < cutoff_unix
                    && still_abandoned_in_store
                {
                    if dry_run {
                        removed += 1;
                        continue;
                    }
                    // The orchestrator ticks concurrently, so the domain may have gone
                    // `Active` since the snapshot. Re-check it inside the per-hostname
                    // gate rather than disconnect a tenant whose domain just came up.
                    if self
                        .offboard_if_unchanged(&domain)
                        .await
                        .map_err(|e| format!("failed to offboard {}: {e}", domain.hostname))?
                    {
                        removed += 1;
                    }
                }
            }
            // Orphaned certificates have no record left to age, so the cutoff does not
            // apply.
            removed += self.prune_orphan_certs(dry_run).await?;
            Ok(removed)
        })
    }

    fn offboard_domain<'a>(
        &'a self,
        hostname: &'a str,
    ) -> futures::future::BoxFuture<'a, std::io::Result<bool>> {
        Box::pin(self.offboard(hostname))
    }

    fn offboard_tenant_domains<'a>(
        &'a self,
        tenant: &'a str,
    ) -> futures::future::BoxFuture<'a, std::io::Result<usize>> {
        Box::pin(self.offboard_tenant(tenant))
    }
}

impl CustomDomainTask {
    /// Run until `shutdown`, ticking every `interval`.
    ///
    /// The loop never returns an error: a tick that fails leaves the affected
    /// domain's reason recorded and every other domain serving.
    pub async fn run(
        &self,
        interval: std::time::Duration,
        shutdown: tokio_util::sync::CancellationToken,
    ) {
        // Warm the cache before the first tick so a restart serves stored
        // certificates on the first handshake instead of after an order.
        self.warm_all().await;
        self.hydrate_limiter(crate::custom_domain::now_unix()).await;
        loop {
            // The tick is inside the `select!`: a pass over a thousand domains
            // is long, and shutdown must not wait for it. Verification gets
            // half the interval, so issuance and renewal start in time.
            tokio::select! {
                () = self.tick_within(crate::custom_domain::now_unix(), interval / 2) => {}
                () = shutdown.cancelled() => break,
            }
            tokio::select! {
                () = tokio::time::sleep(interval) => {}
                () = shutdown.cancelled() => break,
            }
        }
    }

    /// One pass over every registered domain.
    pub async fn tick(&self, now_unix: i64) {
        self.tick_within(now_unix, DEFAULT_VERIFY_BUDGET).await;
    }

    /// One pass. The verification step stops after `verify_budget`.
    ///
    /// Domains it did not reach stay pending for the next tick. This keeps
    /// one tenant's slow DNS from delaying issuance and renewal for others.
    pub async fn tick_within(&self, now_unix: i64, verify_budget: std::time::Duration) {
        // `now_unix` is when the pass began, and a pass can run for minutes. Stamping a
        // late order with it would age its budget entry early and could put a failure's
        // `next_attempt_unix` in the past, sending the next tick straight back to the CA.
        // Adding elapsed monotonic time keeps the caller's clock injectable.
        let started = tokio::time::Instant::now();
        let at = move || elapsed_since(now_unix, started);
        // Each check can wait out a DNS timeout, so checks run `VERIFY_CONCURRENCY` at a
        // time instead of adding up.
        //
        // The budget is a deadline for DNS work only. A write that has begun always
        // finishes: cancelling one can leave a detached save that lands after a newer
        // record or an offboard. The per-check limit shrinks with a short budget, so a
        // check still times out (and backs off) before the deadline cuts it short.
        let deadline = tokio::time::Instant::now() + verify_budget;
        let limit = VERIFY_TIMEOUT.min(verify_budget / 2);
        // Domains that keep failing go last. A short backoff can expire before
        // the next tick, and hostname order alone would then put the same hung
        // names first again. The sort is stable, so ties stay in hostname order.
        let mut pending = self.registry.pending_verification(now_unix);
        pending.sort_by_key(|domain| domain.consecutive_failures);
        let skipped = std::sync::atomic::AtomicUsize::new(0);
        futures::StreamExt::for_each_concurrent(
            futures::stream::iter(pending),
            VERIFY_CONCURRENCY,
            |domain| {
                let skipped = &skipped;
                async move {
                    if !self
                        .verify_one(&domain.hostname, now_unix, limit, deadline)
                        .await
                    {
                        skipped.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
                    }
                }
            },
        )
        .await;
        let skipped = skipped.into_inner();
        if skipped > 0 {
            tracing::warn!(
                skipped,
                budget_ms = verify_budget.as_millis(),
                "custom-domain verification hit its time budget; the rest wait for the next tick"
            );
        }
        for domain in self.registry.due_for_issuance(now_unix) {
            self.issue_one(
                &domain.hostname,
                &domain.tenant,
                domain.verification_token.as_deref(),
                at(),
            )
            .await;
        }
        // Renewal re-verifies first, or a domain the tenant repointed or gave up would be
        // renewed forever, spending an order and a failed validation on the shared ACME
        // account each cycle. All re-checks finish before any order starts: an order
        // takes minutes, and a re-check interleaved with them would be stale.
        let due = self
            .registry
            .due_for_renewal(now_unix, self.renew_before_days);
        let checked: Vec<_> = futures::StreamExt::collect(futures::StreamExt::buffered(
            futures::StreamExt::map(futures::stream::iter(due), |domain| async move {
                let here = self.still_points_here(&domain, now_unix).await;
                (domain, here)
            }),
            VERIFY_CONCURRENCY,
        ))
        .await;
        for (domain, here) in checked {
            if !here {
                continue;
            }
            self.issue_one(
                &domain.hostname,
                &domain.tenant,
                domain.verification_token.as_deref(),
                at(),
            )
            .await;
        }
        // An active domain whose stored certificate is gone (torn write, partial
        // restore, manual delete) fails every handshake but is not due for renewal
        // until its recorded `notAfter` nears, so re-order it here.
        for domain in self.registry.list() {
            if domain.is_servable()
                && domain.is_due(now_unix)
                && !self.certificate_present(&domain.hostname)
            {
                tracing::warn!(
                    hostname = %domain.hostname,
                    "custom domain is active but its certificate is missing; re-ordering"
                );
                self.issue_one(
                    &domain.hostname,
                    &domain.tenant,
                    domain.verification_token.as_deref(),
                    at(),
                )
                .await;
            }
        }
    }

    /// Is `hostname`'s certificate available AND usable?
    ///
    /// Present-on-disk is not enough: a corrupt or mismatched pair passes a
    /// `stat` but is rejected by both `warm` and the handshake source, so
    /// answering `true` for it would suppress the repair below and leave the
    /// domain hard down until its renew window opened — months, potentially.
    /// The pair is therefore parsed, through the same loader the handshake
    /// uses, and a pair that will not load counts as absent.
    ///
    /// Only for a store this task can enumerate; a store it cannot answers
    /// `true`, so a non-filesystem store is never wrongly re-ordered.
    fn certificate_present(&self, hostname: &str) -> bool {
        let Some(fs) = self.cert_store_paths.as_ref() else {
            return true;
        };
        let Some((chain_path, key_path)) = fs.find_cert_for_domains(&[hostname.to_owned()]) else {
            return false;
        };
        // Check the cache only after the `stat` above: a pair deleted after it was cached
        // keeps serving from memory, then fails every handshake after an eviction or
        // restart. Only the parse is skipped while the cache holds a usable copy.
        if self.cache.get(hostname).is_some() {
            return true;
        }
        let (Ok(chain), Ok(key)) = (std::fs::read(&chain_path), std::fs::read(&key_path)) else {
            return false;
        };
        match crate::tls::certified_key_from_pem(&chain, &key, &self.provider) {
            Ok(_) => true,
            Err(e) => {
                tracing::warn!(
                    hostname,
                    "stored custom-domain certificate is present but unusable ({e}); \
                     treating it as missing so the repair pass re-orders it"
                );
                false
            }
        }
    }

    /// Does `domain` still resolve to this deployment?
    ///
    /// A lookup that returns nothing answers `true`: a resolver blip must not
    /// stop a healthy renewal, and the CA's own validation is the real gate.
    ///
    /// A domain that has genuinely moved away is RECORDED, not merely logged.
    /// It is inside its renewal window and will expire — the tenant repointed
    /// or gave the hostname up — so an operator has to act. Skipping silently
    /// left `/actuator/health` `Up` and raised no alert until the certificate
    /// expired, at which point the domain was already down.
    async fn still_points_here(
        &self,
        domain: &crate::custom_domain::CustomDomain,
        now_unix: i64,
    ) -> bool {
        let hostname = domain.hostname.as_str();
        // A lookup past `RECHECK_TIMEOUT`, for the tenant's name or for the
        // ingress, proves nothing. The renewal goes ahead.
        let Ok(observed) =
            tokio::time::timeout(RECHECK_TIMEOUT, self.verifier.observe(hostname)).await
        else {
            return true;
        };
        if matches!(observed, crate::custom_domain::ObservedTarget::None) {
            return true;
        }
        let Ok(ingress) = tokio::time::timeout(RECHECK_TIMEOUT, self.effective_ingress()).await
        else {
            return true;
        };
        let outcome = grade_dns_verification(&observed, &ingress);
        if outcome.is_verified() {
            return true;
        }
        tracing::warn!(
            hostname,
            "skipping renewal: the domain no longer points at this deployment"
        );
        let reason = outcome
            .reason()
            .unwrap_or_else(|| "the domain no longer points at this deployment".to_owned());
        self.record_failure(
            hostname,
            &domain.tenant,
            domain.verification_token.as_deref(),
            now_unix,
            format!(
                "renewal skipped: {reason}. The certificate will expire unless the record is \
                 restored or the domain is offboarded"
            ),
            true,
        )
        .await;
        false
    }

    /// Back off a domain whose verification ran past `VERIFY_TIMEOUT`.
    async fn record_lookup_timeout(
        &self,
        domain: &crate::custom_domain::CustomDomain,
        now_unix: i64,
    ) {
        let backoff = i64::try_from(
            self.limiter
                .backoff_for(domain.consecutive_failures.saturating_add(1)),
        )
        .unwrap_or(i64::MAX);
        let recorded = self
            .registry
            .record_verification_failure_for(
                &domain.hostname,
                &domain.tenant,
                domain.verification_token.as_deref(),
                now_unix,
                "the DNS lookup timed out",
                backoff,
            )
            .await;
        if let Err(e) = recorded {
            tracing::warn!(
                hostname = %domain.hostname,
                "failed to persist custom-domain lookup timeout: {e}"
            );
        }
    }

    /// Check where one hostname points, and that its owner published this
    /// registration's token, and record the result.
    ///
    /// Pointing here is not enough on its own: a record another tenant left
    /// behind also points here, and HTTP-01 would then issue to whoever
    /// registered the hostname next (#2642).
    ///
    /// Only the DNS lookups are time-limited: by `limit`, and by `deadline`.
    /// A lookup past `limit` fails the domain with a backoff. One cut short by
    /// `deadline` is left for the next tick. Returns `false` for the latter.
    async fn verify_one(
        &self,
        hostname: &str,
        now_unix: i64,
        limit: std::time::Duration,
        deadline: tokio::time::Instant,
    ) -> bool {
        let Some(mut domain) = self.registry.get(hostname) else {
            return true;
        };
        // `load` gives a pre-token record its token; this retries a load whose
        // write failed.
        if domain.needs_verification_token() {
            if let Err(e) = self
                .registry
                .adopt_verification_token(hostname, now_unix)
                .await
            {
                tracing::warn!(
                    hostname,
                    "could not give a custom domain its verification token: {e}"
                );
                return true;
            }
            match self.registry.get(hostname) {
                Some(adopted) if adopted.verification_token.is_some() => domain = adopted,
                _ => return true,
            }
        }
        let allowed = limit.min(deadline.saturating_duration_since(tokio::time::Instant::now()));
        if allowed.is_zero() {
            return false;
        }
        let Ok(outcome) = tokio::time::timeout(allowed, self.grade_lookups(&domain)).await else {
            if allowed < limit {
                return false;
            }
            self.record_lookup_timeout(&domain, now_unix).await;
            return true;
        };
        let backoff = i64::try_from(
            self.limiter
                .backoff_for(domain.consecutive_failures.saturating_add(1)),
        )
        .unwrap_or(i64::MAX);
        if let Err(e) =
            apply_verification(&self.registry, &domain, &outcome, now_unix, backoff).await
        {
            tracing::warn!(
                hostname,
                "failed to persist custom-domain verification: {e}"
            );
        }
        true
    }

    /// Look up where `domain` points, and whether its owner published the
    /// token, and grade both. This is the only part of a verification that
    /// waits on DNS.
    async fn grade_lookups(
        &self,
        domain: &crate::custom_domain::CustomDomain,
    ) -> crate::custom_domain::VerificationOutcome {
        let observed = self.verifier.observe(&domain.hostname).await;
        let outcome = grade_dns_verification(&observed, &self.effective_ingress().await);
        if !outcome.is_verified() {
            return outcome;
        }
        let txt = self
            .verifier
            .observe_txt(&domain.verification_record_name())
            .await;
        grade_ownership(domain, &txt)
    }

    /// The ingress to compare a tenant's DNS against.
    ///
    /// A resolver reports the ADDRESSES a name ends at, following any CNAME
    /// silently — so an operator who configured only `ingress_hostname` has
    /// nothing to compare an observed address against. The hostname's addresses
    /// are therefore resolved and **added to** whatever was configured
    /// explicitly, rather than replacing them or being skipped.
    ///
    /// The union matters for the setup the guide documents: subdomains CNAME to
    /// a load balancer while apex domains use static A/AAAA records. Those are
    /// two DIFFERENT address sets, and a tenant subdomain resolves to the load
    /// balancer's. Returning only the configured apex addresses would leave
    /// every subdomain stuck at `pending_dns` forever.
    ///
    /// Resolved per tick, not cached: an ingress behind a load balancer whose
    /// address changes must not leave every tenant domain failing verification
    /// until the process restarts.
    async fn effective_ingress(&self) -> ExpectedIngress {
        let mut ingress = self.ingress.clone();
        let Some(host) = ingress.hostname.clone() else {
            return ingress;
        };
        if let crate::custom_domain::ObservedTarget::Addresses(addrs) =
            self.verifier.observe(&host).await
        {
            for addr in addrs {
                match addr {
                    std::net::IpAddr::V4(v4) if !ingress.ipv4.contains(&v4) => {
                        ingress.ipv4.push(v4);
                    }
                    std::net::IpAddr::V6(v6) if !ingress.ipv6.contains(&v6) => {
                        ingress.ipv6.push(v6);
                    }
                    _ => {}
                }
            }
        }
        ingress
    }

    /// Order (or renew) one hostname's certificate, budget permitting.
    async fn issue_one(&self, hostname: &str, tenant: &str, token: Option<&str>, now_unix: i64) {
        // A distributed backend is configured but this process fell back to a
        // per-process coordinator: every replica would hold its own lease and order the
        // same certificate against the shared account.
        if self.leadership_degraded {
            self.record_failure(
                hostname,
                tenant,
                token,
                now_unix,
                "refusing to order: a distributed scheduler backend is configured but its \
                 coordinator is unavailable in this process, so a lease would not exclude the \
                 other replicas",
                true,
            )
            .await;
            return;
        }

        // Check the budget before any network call, so a spent budget costs the CA
        // nothing. The failure backoff was already applied by `is_due`.
        if !self.within_budget(hostname, tenant, token, now_unix).await {
            return;
        }

        // One replica per hostname orders. `Fleet` always grants on the in-process
        // backend and grants one replica on a distributed one. The key is a mutex held
        // through the order.
        let tick_key = format!("custom-domain:{hostname}");
        let lease = match self
            .coordinator
            .try_acquire_for_period(
                CUSTOM_DOMAIN_TASK,
                &tick_key,
                TaskCoordination::Fleet,
                crate::acme::renewal::ACME_LEADER_HOLD,
            )
            .await
        {
            Ok(Some(lease)) => lease,
            // Another replica leads for this domain. Adopt whatever it has
            // already written rather than ordering a second certificate.
            Ok(None) => {
                self.warm(hostname).await;
                return;
            }
            Err(e) => {
                self.record_failure(
                    hostname,
                    tenant,
                    token,
                    now_unix,
                    format!("leader election failed: {e}"),
                    true,
                )
                .await;
                return;
            }
        };

        // The registry can change during the lease wait, so re-assert ownership in the
        // transition. Otherwise an offboarded hostname spends budget and CA rate limit on
        // a certificate `install` discards, and a re-registered one has another tenant's
        // record moved to `Issuing`.
        match self.registry.record_issuing_for(hostname, tenant).await {
            Ok(true) => {}
            Ok(false) => {
                tracing::debug!(
                    hostname,
                    tenant,
                    "abandoning a custom-domain order: the hostname is no longer this tenant's to \
                     order for"
                );
                if let Err(e) = lease.release_and_free().await {
                    tracing::warn!(hostname, error = %e, "failed to release the custom-domain lease");
                }
                return;
            }
            Err(e) => {
                // Without the `Issuing` state persisted, a successful order could not
                // be recorded either, and the next tick would spend another CA attempt.
                self.record_failure(
                    hostname,
                    tenant,
                    token,
                    now_unix,
                    format!("refusing to order: the issuing state could not be persisted ({e})"),
                    true,
                )
                .await;
                if let Err(e) = lease.release_and_free().await {
                    tracing::warn!(hostname, error = %e, "failed to release the custom-domain lease");
                }
                return;
            }
        }
        self.limiter.record_attempt(hostname, now_unix);
        self.persist_attempt(hostname, tenant, token, now_unix)
            .await;

        let order_started = tokio::time::Instant::now();
        let outcome = self.issuer.issue(hostname).await;
        // An order can take minutes; what follows it is stamped with when it
        // actually finished, not when it began.
        let now_unix = elapsed_since(now_unix, order_started);

        // Hold the key until the record leaves `Issuing` (installed, or failed with its
        // backoff written): `due_for_issuance` selects `Issuing` records, so an earlier
        // free lets a second replica order the same hostname.
        let failure = match outcome {
            Ok(issued) => self
                .install(hostname, tenant, &issued, now_unix)
                .await
                .err(),
            Err(e) => Some(e),
        };
        if let Some(e) = failure {
            self.record_failure(hostname, tenant, token, now_unix, e, true)
                .await;
        }

        if let Err(e) = lease.release_and_free().await {
            tracing::warn!(hostname, error = %e, "failed to release the custom-domain lease");
        }
    }

    /// Persist, parse and start serving a freshly issued certificate.
    ///
    /// Persisting first means a crash between the two still leaves the
    /// certificate on disk for the next boot to warm.
    async fn install(
        &self,
        hostname: &str,
        tenant: &str,
        issued: &IssuedCertificate,
        now_unix: i64,
    ) -> Result<(), String> {
        let stored = StoredCert {
            chain_pem: issued.chain_pem.clone(),
            key_pem: issued.key_pem.clone(),
        };
        let not_after = crate::tls::leaf_not_after_from_pem(stored.chain_pem.as_bytes())?;
        let certified = crate::tls::certified_key_from_pem(
            stored.chain_pem.as_bytes(),
            stored.key_pem.as_bytes(),
            &self.provider,
        )?;
        // Save, activate and cache in one gate section, so no offboard, successor or cold
        // load runs between the steps and the owner check still holds at the write. The
        // save comes first so that after a crash the next boot finds the certificate on disk.
        self.registry
            .with_hostname(hostname, async |gate| {
                // If the domain was offboarded or taken by another tenant during the
                // order, installing would resurrect a deleted certificate or promote an
                // unverified record. Discard before anything is written.
                match gate.get() {
                    Some(record) if record.tenant != tenant => {
                        return Err(format!(
                            "discarding the certificate: {hostname} now belongs to tenant {}",
                            record.tenant
                        ));
                    }
                    None => {
                        return Err(format!(
                            "discarding the certificate: {hostname} was offboarded while the \
                             order ran"
                        ));
                    }
                    Some(_) => {}
                }
                self.certs
                    .save_cert(&cert_id_for(hostname), &stored)
                    .await
                    .map_err(|e| {
                        format!("failed to persist the certificate for {hostname}: {e}")
                    })?;
                let activated = gate
                    .record_active_for(tenant, now_unix, not_after)
                    .await
                    .map_err(|e| {
                        format!("failed to persist the active state for {hostname}: {e}")
                    })?;
                if !activated {
                    // The record changed hands or is not verified. We hold the gate, so the
                    // pair at this id is the one just written: do not leave it on disk.
                    self.cache.remove(hostname);
                    if let Err(e) = self.certs.delete_cert(&cert_id_for(hostname)).await {
                        tracing::warn!(hostname, "failed to delete a superseded certificate: {e}");
                    }
                    return Err(format!(
                        "discarding the certificate: {hostname} changed hands while it was being \
                         issued"
                    ));
                }
                self.cache.insert(hostname, certified);
                Ok(())
            })
            .await
            .unwrap_or_else(|| {
                Err(format!(
                    "discarding the certificate: {hostname} is unusable"
                ))
            })?;
        tracing::info!(hostname, not_after, "custom domain is active");
        // A certificate issued already inside its renew-before window (CA lifetime shorter
        // than `renew_before_days`) would be re-ordered every tick forever. Park it at
        // the maximum backoff and say why.
        if crate::custom_domain::needs_renewal(not_after, self.renew_before_days, now_unix) {
            let backoff = i64::try_from(self.limiter.max_backoff()).unwrap_or(i64::MAX);
            let _ = self
                .registry
                .record_failure(
                    hostname,
                    now_unix,
                    format!(
                        "the issued certificate is already inside its renew-before window ({} \
                         days); lower [server.tls.acme] renew_before_days below the certificate \
                         lifetime",
                        self.renew_before_days
                    ),
                    backoff,
                )
                .await;
            return Ok(());
        }
        // Clears only once no domain is failing: another broken domain keeps the alert true.
        self.clear_alert_if_healthy(now_unix);
        Ok(())
    }

    /// Seed the issuance budget from the attempts persisted on each domain
    /// record, so a restart does not hand the shared ACME account a fresh
    /// quota. Called once before the first pass of [`Self::run`]; safe to call
    /// again.
    pub async fn hydrate_limiter(&self, now_unix: i64) {
        self.limiter.hydrate(
            self.registry.list().into_iter().flat_map(|d| {
                let hostname = d.hostname;
                d.issuance_attempts_unix
                    .into_iter()
                    .map(move |at| (hostname.clone(), at))
            }),
            now_unix,
        );
        // Copy orders the ledger lacks, such as those from before an upgrade.
        // Do it before a domain can be offboarded and take its record away.
        let recent = self
            .registry
            .list()
            .into_iter()
            .flat_map(|d| d.issuance_attempts_unix)
            .collect();
        if let Err(e) = self.registry.merge_global_attempts(recent, now_unix).await {
            tracing::warn!(error = %e, "cannot copy orders into the custom-domain attempt ledger");
        }
        // The ledger also holds orders for offboarded domains.
        match self.registry.load_global_attempts().await {
            Ok(ledger) => self.limiter.hydrate_global(ledger, now_unix),
            Err(e) => tracing::warn!(
                error = %e,
                "cannot read the custom-domain attempt ledger; offboarded orders do not count"
            ),
        }
    }

    /// Is there issuance budget for `hostname`? A refusal defers the domain
    /// until the refusing window rolls; it is not a failure, because no order
    /// was placed, so it never counts toward the failure backoff.
    async fn within_budget(
        &self,
        hostname: &str,
        tenant: &str,
        token: Option<&str>,
        now_unix: i64,
    ) -> bool {
        let decision = self.limiter.check(hostname, now_unix);
        if decision.is_allowed() {
            return true;
        }
        if let (Some(reason), Some(wait)) = (decision.reason(), decision.retry_after_secs()) {
            self.defer(
                hostname,
                tenant,
                token,
                now_unix.saturating_add(wait),
                reason,
            )
            .await;
        }
        false
    }

    /// Persist that an order is about to go out, so the issuance budget
    /// survives a restart, including a crash mid-order.
    ///
    /// Fails open: the in-memory limiter already counts it, and refusing to
    /// order would turn a store hiccup into an outage for the tenant.
    async fn persist_attempt(
        &self,
        hostname: &str,
        tenant: &str,
        token: Option<&str>,
        now_unix: i64,
    ) {
        if let Err(e) = self.registry.record_global_attempt(now_unix).await {
            tracing::warn!(
                hostname,
                error = %e,
                "cannot persist the attempt ledger; this order is lost if the domain goes and the \
                 process restarts"
            );
        }
        if let Err(e) = self
            .registry
            .record_attempt_for_registration(hostname, tenant, token, now_unix)
            .await
        {
            tracing::warn!(
                hostname,
                error = %e,
                "could not persist a custom-domain issuance attempt; the budget will not survive \
                 a restart"
            );
        }
    }

    /// Hold a domain back until `retry_at_unix` without recording a failure.
    async fn defer(
        &self,
        hostname: &str,
        tenant: &str,
        token: Option<&str>,
        retry_at_unix: i64,
        reason: String,
    ) {
        match self
            .registry
            .defer_for_registration(hostname, tenant, token, retry_at_unix, reason)
            .await
        {
            Ok(true) => {}
            Ok(false) => tracing::debug!(
                hostname,
                tenant,
                "discarded a custom-domain deferral: the hostname changed hands or was \
                 re-registered meanwhile"
            ),
            Err(e) => tracing::warn!(
                hostname,
                error = %e,
                "could not record a custom-domain issuance deferral"
            ),
        }
    }

    /// Record a failure on one domain, alerting the operator when it is an
    /// issuance failure rather than a deferral the budget already explains.
    ///
    /// `token` is the registration's ownership token as read when the work
    /// started: the failure is recorded only while the stored record is still
    /// that registration.
    async fn record_failure(
        &self,
        hostname: &str,
        tenant: &str,
        token: Option<&str>,
        now_unix: i64,
        reason: impl Into<String>,
        alert: bool,
    ) {
        let reason = reason.into();
        let failures = self
            .registry
            .get(hostname)
            .filter(|d| d.tenant == tenant && d.verification_token.as_deref() == token)
            .map_or(0, |d| d.consecutive_failures)
            .saturating_add(1);
        let backoff = i64::try_from(self.limiter.backoff_for(failures)).unwrap_or(i64::MAX);
        // Record only against this registration: if the hostname was re-registered while
        // the order ran, the successor must not inherit its reason and backoff.
        match self
            .registry
            .record_failure_for_registration(
                hostname,
                tenant,
                token,
                now_unix,
                reason.clone(),
                backoff,
            )
            .await
        {
            Ok(true) => {}
            Ok(false) => {
                tracing::debug!(
                    hostname,
                    tenant,
                    "discarded a custom-domain failure: the hostname changed hands or was \
                     re-registered while the order was in flight"
                );
                return;
            }
            Err(e) => {
                tracing::warn!(hostname, "failed to persist custom-domain failure: {e}");
            }
        }
        // Name the tenant too, so an operator can act on the alert without a lookup.
        let message =
            format!("custom domain {hostname} (tenant {tenant}) failed to issue: {reason}");
        tracing::warn!("{message}");
        if alert {
            (self.reporter)(message);
        }
    }

    /// The prune-only half of this task: the
    /// [`CustomDomainPruner`](crate::custom_domain::CustomDomainPruner)
    /// implementation the server and `autumn db retention` both install.
    pub(crate) fn pruner(&self) -> PruneOnlyCustomDomainPruner {
        PruneOnlyCustomDomainPruner {
            registry: Arc::clone(&self.registry),
            cache: Arc::clone(&self.cache),
            certs: Arc::clone(&self.certs),
            limiter: Arc::clone(&self.limiter),
            cert_store_paths: self.cert_store_paths.clone(),
            retained_cert_ids: self.retained_cert_ids.clone(),
            reporter: Arc::clone(&self.reporter),
            recovery: self.recovery.clone(),
        }
    }

    /// Retract the operator alert once NOTHING is failing any more.
    ///
    /// Shared with the prune half; see
    /// [`PruneOnlyCustomDomainPruner::clear_alert_if_healthy`].
    fn clear_alert_if_healthy(&self, now_unix: i64) {
        self.pruner().clear_alert_if_healthy(now_unix);
    }

    /// Offboard `hostname`: stop routing, stop serving, halt renewal, and
    /// delete the stored certificate so it is not orphaned (AC7, #1605).
    ///
    /// # Errors
    ///
    /// Propagates a registry-store error; see
    /// [`PruneOnlyCustomDomainPruner::offboard`].
    pub async fn offboard(&self, hostname: &str) -> std::io::Result<bool> {
        self.pruner().offboard(hostname).await
    }

    /// Offboard every domain a tenant owns. Returns how many were removed.
    ///
    /// # Errors
    ///
    /// Propagates the registry store's delete error.
    pub async fn offboard_tenant(&self, tenant: &str) -> std::io::Result<usize> {
        self.pruner().offboard_tenant(tenant).await
    }

    /// Load one domain's stored certificate into the cache. Returns whether a
    /// usable certificate was found.
    ///
    /// This is AC6's incremental load: a handshake for a domain evicted from
    /// the bounded cache re-reads its certificate here instead of the
    /// deployment needing every certificate resident at boot.
    pub async fn warm(&self, hostname: &str) -> bool {
        let Ok(host) = crate::custom_domain::normalize_hostname(hostname) else {
            return false;
        };
        let hostname = host.as_str();
        if !self.registry.is_servable(hostname) {
            return false;
        }
        // Load and publish inside the hostname's gate, so a renewal cannot
        // persist and cache a new pair between the two. Only a cache refill
        // takes it, never a handshake.
        self.registry
            .with_hostname(hostname, async |gate| {
                if !gate.get().is_some_and(|d| d.is_servable()) {
                    return false;
                }
                match self.certs.load_cert(&cert_id_for(hostname)).await {
                    Ok(Some(stored)) => match crate::tls::certified_key_from_pem(
                        stored.chain_pem.as_bytes(),
                        stored.key_pem.as_bytes(),
                        &self.provider,
                    ) {
                        Ok(certified) => {
                            self.cache.insert_if_not_older(hostname, certified);
                            true
                        }
                        Err(e) => {
                            tracing::warn!(
                                hostname,
                                "stored custom-domain certificate is unusable: {e}"
                            );
                            false
                        }
                    },
                    Ok(None) => false,
                    Err(e) => {
                        tracing::warn!(hostname, "failed to read a custom-domain certificate: {e}");
                        false
                    }
                }
            })
            .await
            .unwrap_or(false)
    }

    /// Warm as many active domains as the cache holds, newest activation
    /// first, so a restart serves the busiest domains without a disk read.
    ///
    /// Deliberately bounded by the cache: warming every domain would make
    /// "all certificates resident" a boot requirement, which AC6 rules out.
    async fn warm_all(&self) {
        let mut active: Vec<_> = self
            .registry
            .list()
            .into_iter()
            .filter(crate::custom_domain::CustomDomain::is_servable)
            .collect();
        active.sort_by_key(|d| std::cmp::Reverse(d.activated_at_unix.unwrap_or(0)));
        for domain in active.into_iter().take(self.cache.capacity()) {
            self.warm(&domain.hostname).await;
        }
    }
}

impl crate::custom_domain::CustomDomainPruner for CustomDomainTask {
    fn prune(
        &self,
        cutoff_unix: i64,
        dry_run: bool,
    ) -> futures::future::BoxFuture<'_, Result<u64, String>> {
        let pruner = self.pruner();
        Box::pin(async move {
            <PruneOnlyCustomDomainPruner as crate::custom_domain::CustomDomainPruner>::prune(
                &pruner,
                cutoff_unix,
                dry_run,
            )
            .await
        })
    }

    fn offboard_domain<'a>(
        &'a self,
        hostname: &'a str,
    ) -> futures::future::BoxFuture<'a, std::io::Result<bool>> {
        let pruner = self.pruner();
        Box::pin(async move {
            <PruneOnlyCustomDomainPruner as crate::custom_domain::CustomDomainPruner>::offboard_domain(
                &pruner, hostname,
            )
            .await
        })
    }

    fn offboard_tenant_domains<'a>(
        &'a self,
        tenant: &'a str,
    ) -> futures::future::BoxFuture<'a, std::io::Result<usize>> {
        let pruner = self.pruner();
        Box::pin(async move {
            <PruneOnlyCustomDomainPruner as crate::custom_domain::CustomDomainPruner>::offboard_tenant_domains(
                &pruner, tenant,
            )
            .await
        })
    }
}

/// A [`SniCertSource`](crate::custom_domain::SniCertSource) reading the
/// filesystem certificate store on the handshake path.
///
/// Two `read`s and a parse, only for a hostname the registry has already
/// confirmed is registered and active. That is what lets `cert_cache_size` be
/// much smaller than the number of connected domains without any of them
/// stopping being served.
#[derive(Debug)]
pub struct FsSniCertSource {
    store: Arc<crate::acme::store::FsAcmeStore>,
    provider: Arc<CryptoProvider>,
}

impl FsSniCertSource {
    /// A source over `store`, parsing with `provider`.
    #[must_use]
    pub const fn new(
        store: Arc<crate::acme::store::FsAcmeStore>,
        provider: Arc<CryptoProvider>,
    ) -> Self {
        Self { store, provider }
    }
}

impl crate::custom_domain::SniCertSource for FsSniCertSource {
    fn load(&self, hostname: &str) -> Option<Arc<rustls::sign::CertifiedKey>> {
        let (chain_path, key_path) = self.store.find_cert_for_domains(&[hostname.to_owned()])?;
        let chain = std::fs::read(&chain_path).ok()?;
        let key = std::fs::read(&key_path).ok()?;
        match crate::tls::certified_key_from_pem(&chain, &key, &self.provider) {
            Ok(certified) => Some(certified),
            Err(e) => {
                tracing::warn!(
                    hostname,
                    "stored custom-domain certificate is unusable: {e}"
                );
                None
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_hostname_gets_its_own_cert_id() {
        assert_ne!(cert_id_for("a.test"), cert_id_for("b.test"));
        assert_eq!(cert_id_for("a.test"), cert_id_for("a.test"));
    }

    use std::sync::atomic::{AtomicUsize, Ordering};

    use futures::future::BoxFuture;

    use crate::custom_domain::{
        CustomDomain, CustomDomainStore, DomainVerifier, IssuedCertificate,
        MemoryCustomDomainStore, ObservedTarget, ObservedTxt, StoreFuture,
    };

    /// Records how many leases were released at each save.
    #[derive(Debug, Default)]
    struct WatchStore {
        inner: MemoryCustomDomainStore,
        releases: Arc<AtomicUsize>,
        saves: std::sync::Mutex<Vec<usize>>,
    }

    impl CustomDomainStore for WatchStore {
        fn load_all(&self) -> StoreFuture<'_, std::io::Result<Vec<CustomDomain>>> {
            self.inner.load_all()
        }
        fn save<'a>(&'a self, domain: &'a CustomDomain) -> StoreFuture<'a, std::io::Result<()>> {
            self.saves
                .lock()
                .unwrap()
                .push(self.releases.load(Ordering::SeqCst));
            self.inner.save(domain)
        }
        fn delete<'a>(&'a self, hostname: &'a str) -> StoreFuture<'a, std::io::Result<()>> {
            self.inner.delete(hostname)
        }
        fn load_attempts(&self) -> StoreFuture<'_, std::io::Result<Vec<i64>>> {
            self.inner.load_attempts()
        }
        fn append_attempt(&self, at_unix: i64) -> StoreFuture<'_, std::io::Result<()>> {
            self.inner.append_attempt(at_unix)
        }
        fn merge_attempts(
            &self,
            attempts: Vec<i64>,
            now_unix: i64,
        ) -> StoreFuture<'_, std::io::Result<()>> {
            self.inner.merge_attempts(attempts, now_unix)
        }
    }

    /// Grants every claim with a lease that counts its release.
    struct CountingCoordinator(Arc<AtomicUsize>);

    impl SchedulerCoordinator for CountingCoordinator {
        fn backend(&self) -> &'static str {
            "postgres"
        }
        fn replica_id(&self) -> &'static str {
            "replica-a"
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
            let releases = Arc::clone(&self.0);
            Box::pin(async move {
                Ok(Some(crate::scheduler::SchedulerLease::tracked(
                    "postgres",
                    "replica-a",
                    releases,
                )))
            })
        }
    }

    /// Points every name at the ingress, and publishes each owner's token.
    struct OwnedVerifier(Arc<CustomDomainRegistry>);

    impl DomainVerifier for OwnedVerifier {
        fn observe<'a>(&'a self, _hostname: &'a str) -> BoxFuture<'a, ObservedTarget> {
            Box::pin(async { ObservedTarget::Addresses(vec!["203.0.113.10".parse().unwrap()]) })
        }
        fn observe_txt<'a>(&'a self, name: &'a str) -> BoxFuture<'a, ObservedTxt> {
            Box::pin(async move {
                let token = name
                    .strip_prefix("_autumn-challenge.")
                    .and_then(|host| self.0.get(host))
                    .and_then(|domain| domain.verification_token);
                ObservedTxt::Values(token.into_iter().collect())
            })
        }
    }

    struct RejectingIssuer;

    impl DomainIssuer for RejectingIssuer {
        fn issue<'a>(
            &'a self,
            _hostname: &'a str,
        ) -> BoxFuture<'a, Result<IssuedCertificate, String>> {
            Box::pin(async { Err("the CA rejected the order".to_owned()) })
        }
    }

    // #3052: `due_for_issuance` selects an `Issuing` record, so the key must stay held
    // until the failure and its backoff are saved, or a second replica orders the hostname.
    #[tokio::test]
    async fn domain_key_stays_held_until_the_failure_is_saved() {
        const NOW: i64 = 1_800_000_000;
        let releases = Arc::new(AtomicUsize::new(0));
        let store = Arc::new(WatchStore {
            releases: Arc::clone(&releases),
            ..WatchStore::default()
        });
        let registry = Arc::new(CustomDomainRegistry::new(
            Arc::clone(&store) as Arc<dyn CustomDomainStore>,
            100,
        ));
        registry.load().await.unwrap();
        registry
            .register("app.clientco.com", "tenant-a", NOW)
            .await
            .unwrap();
        let dir = tempfile::tempdir().unwrap();
        let task = CustomDomainTask {
            registry: Arc::clone(&registry),
            cache: Arc::new(CustomDomainCertCache::new(8)),
            certs: Arc::new(crate::acme::store::FsAcmeStore::new(dir.path(), "staging")),
            provider: crate::tls::crypto_provider(),
            verifier: Arc::new(OwnedVerifier(Arc::clone(&registry))),
            issuer: Arc::new(RejectingIssuer),
            limiter: Arc::new(IssuanceLimiter::new(5, 50, 300, 86_400)),
            ingress: ExpectedIngress {
                hostname: Some("ingress.myapp.com".to_owned()),
                ipv4: vec!["203.0.113.10".parse().unwrap()],
                ipv6: vec![],
            },
            renew_before_days: 30,
            reporter: Arc::new(|_| {}),
            recovery: None,
            coordinator: Arc::new(CountingCoordinator(Arc::clone(&releases))),
            leadership_degraded: false,
            cert_store_paths: None,
            retained_cert_ids: std::collections::HashSet::new(),
        };
        store.saves.lock().unwrap().clear();

        task.tick(NOW).await;

        let saves = store.saves.lock().unwrap().clone();
        assert!(!saves.is_empty(), "the order must save its state");
        assert!(
            saves.iter().all(|&released| released == 0),
            "every save, the failure included, comes before the key frees: {saves:?}"
        );
        assert_eq!(releases.load(Ordering::SeqCst), 1, "the key frees once");
    }
}
