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
//! verification gate already proves the hostname points at this deployment,
//! which is exactly the condition that makes HTTP-01 work — so the challenge
//! the operator can actually answer is the one used, even when the
//! deployment's own certificate is issued over DNS-01.
//!
//! # Abuse posture
//!
//! Every order passes three gates first: the hostname is registered by the
//! app, DNS independently points here, and the
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
};
use crate::scheduler::SchedulerCoordinator;
use crate::task::TaskCoordination;
use rustls::crypto::CryptoProvider;

/// The scheduled-task name custom-domain leases and alerts are keyed on.
pub const CUSTOM_DOMAIN_TASK: &str = "custom_domain_certificates";

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
        loop {
            // The tick is inside the `select!`: a pass over a thousand domains
            // is long, and shutdown must not wait for it.
            tokio::select! {
                () = self.tick(crate::custom_domain::now_unix()) => {}
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
        for domain in self.registry.pending_verification(now_unix) {
            self.verify_one(&domain.hostname, now_unix).await;
        }
        for domain in self.registry.due_for_issuance(now_unix) {
            self.issue_one(&domain.hostname, &domain.tenant, now_unix)
                .await;
        }
        // Renewal re-verifies first. A tenant who repointed or gave up their
        // domain would otherwise be renewed forever: every cycle spends an
        // order plus a failed validation against the shared ACME account, for a
        // certificate that reaches nobody.
        for domain in self
            .registry
            .due_for_renewal(now_unix, self.renew_before_days)
        {
            if !self.still_points_here(&domain.hostname).await {
                continue;
            }
            self.issue_one(&domain.hostname, &domain.tenant, now_unix)
                .await;
        }
        // An active domain whose stored certificate has gone (a torn write, a
        // partial restore, a manual delete) is refused at the handshake but is
        // NOT due for renewal — its recorded `notAfter` may be months away — so
        // without this it stays hard down until that window opens.
        for domain in self.registry.list() {
            if domain.is_servable()
                && domain.is_due(now_unix)
                && !self.certificate_present(&domain.hostname)
            {
                tracing::warn!(
                    hostname = %domain.hostname,
                    "custom domain is active but its certificate is missing; re-ordering"
                );
                self.issue_one(&domain.hostname, &domain.tenant, now_unix)
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
        if self.cache.get(hostname).is_some() {
            return true;
        }
        let Some(fs) = self.cert_store_paths.as_ref() else {
            return true;
        };
        let Some((chain_path, key_path)) = fs.find_cert_for_domains(&[hostname.to_owned()]) else {
            return false;
        };
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

    /// Does `hostname` still resolve to this deployment?
    ///
    /// A lookup that returns nothing answers `true`: a resolver blip must not
    /// stop a healthy renewal, and the CA's own validation is the real gate.
    async fn still_points_here(&self, hostname: &str) -> bool {
        let observed = self.verifier.observe(hostname).await;
        if matches!(observed, crate::custom_domain::ObservedTarget::None) {
            return true;
        }
        if grade_dns_verification(&observed, &self.effective_ingress().await).is_verified() {
            return true;
        }
        tracing::warn!(
            hostname,
            "skipping renewal: the domain no longer points at this deployment"
        );
        false
    }

    /// Check where one hostname points and record the result.
    async fn verify_one(&self, hostname: &str, now_unix: i64) {
        let observed = self.verifier.observe(hostname).await;
        let outcome = grade_dns_verification(&observed, &self.effective_ingress().await);
        let failures = self
            .registry
            .get(hostname)
            .map_or(0, |d| d.consecutive_failures);
        let backoff =
            i64::try_from(self.limiter.backoff_for(failures.saturating_add(1))).unwrap_or(i64::MAX);
        if let Err(e) =
            apply_verification(&self.registry, hostname, &outcome, now_unix, backoff).await
        {
            tracing::warn!(
                hostname,
                "failed to persist custom-domain verification: {e}"
            );
        }
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
    async fn issue_one(&self, hostname: &str, tenant: &str, now_unix: i64) {
        // A distributed scheduler backend was configured but this process fell
        // back to a per-process coordinator. Ordering now would give every
        // replica its own lease, so all of them would order the SAME
        // certificate against the one shared account. Refuse, and say why.
        if self.leadership_degraded {
            self.record_failure(
                hostname,
                tenant,
                now_unix,
                "refusing to order: a distributed scheduler backend is configured but its \
                 coordinator is unavailable in this process, so a lease would not exclude the \
                 other replicas",
                true,
            )
            .await;
            return;
        }

        // The budget is checked BEFORE any network call, so a spent budget
        // costs the CA nothing. The failure backoff is already applied by
        // `is_due` on the record, so the budget is the only thing left to ask.
        let decision = self.limiter.check(hostname, now_unix);
        if !decision.is_allowed() {
            if let Some(reason) = decision.reason() {
                self.record_failure(hostname, tenant, now_unix, reason, false)
                    .await;
            }
            return;
        }

        // One replica per hostname orders. `Fleet` grants unconditionally on
        // the in-process backend (correct for a single replica) and to exactly
        // one replica on a distributed one.
        let tick_key = format!("custom-domain:{hostname}");
        let lease = match self
            .coordinator
            .try_acquire(CUSTOM_DOMAIN_TASK, &tick_key, TaskCoordination::Fleet)
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
                    now_unix,
                    format!("leader election failed: {e}"),
                    true,
                )
                .await;
                return;
            }
        };

        if let Err(e) = self.registry.record_issuing(hostname).await {
            tracing::warn!(
                hostname,
                "failed to persist custom-domain issuing state: {e}"
            );
        }
        self.limiter.record_attempt(hostname, now_unix);

        let outcome = self.issuer.issue(hostname).await;
        // Always release the lease, whatever the order did.
        if let Err(e) = lease.release().await {
            tracing::warn!(hostname, error = %e, "failed to release the custom-domain lease");
        }

        let issued = match outcome {
            Ok(issued) => issued,
            Err(e) => {
                self.record_failure(hostname, tenant, now_unix, e, true)
                    .await;
                return;
            }
        };

        if let Err(e) = self.install(hostname, tenant, &issued, now_unix).await {
            self.record_failure(hostname, tenant, now_unix, e, true)
                .await;
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
        // The order took a network round trip. If the app offboarded the domain
        // — or another tenant registered it — meanwhile, installing would
        // resurrect a deleted certificate and promote a record straight to
        // active that was never verified, past the gate this module exists to
        // enforce. Discard instead.
        match self.registry.get(hostname) {
            Some(record) if record.tenant == tenant => {}
            Some(record) => {
                return Err(format!(
                    "discarding the certificate: {hostname} now belongs to tenant {}",
                    record.tenant
                ));
            }
            None => {
                return Err(format!(
                    "discarding the certificate: {hostname} was offboarded while the order ran"
                ));
            }
        }

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
        self.certs
            .save_cert(&cert_id_for(hostname), &stored)
            .await
            .map_err(|e| format!("failed to persist the certificate for {hostname}: {e}"))?;
        // Activate only while `tenant` STILL owns the hostname. The ownership
        // check above ran before `save_cert` awaited; an offboard plus a
        // re-registration in that window would otherwise stamp `Active` onto
        // the new tenant's record, carrying it from `pending_dns` to serving on
        // a certificate issued for someone else. Losing the race means the
        // certificate belongs to nobody: drop it rather than leave a private
        // key on disk for a hostname its owner never verified.
        let activated = self
            .registry
            .record_active_for(hostname, tenant, now_unix, not_after)
            .await
            .map_err(|e| format!("failed to persist the active state for {hostname}: {e}"))?;
        if !activated {
            self.cache.remove(hostname);
            if let Err(e) = self.certs.delete_cert(&cert_id_for(hostname)).await {
                tracing::warn!(hostname, "failed to delete a superseded certificate: {e}");
            }
            return Err(format!(
                "discarding the certificate: {hostname} changed hands while it was being issued"
            ));
        }
        self.cache.insert(hostname, certified);
        tracing::info!(hostname, not_after, "custom domain is active");
        // Backstop: a certificate already inside its renew-before window the
        // moment it is issued — a CA issuing a shorter lifetime than
        // `renew_before_days` — would be re-ordered every tick until the budget
        // stopped it, then again the next day, forever. Park it and say why.
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
        // Clear the operator alert only once NOTHING is failing: with a
        // thousand domains, recovering one while another is still broken must
        // not retract an alert that is still true.
        if let Some(recovery) = &self.recovery
            && self.registry.health_report(now_unix).is_empty()
        {
            recovery();
        }
        Ok(())
    }

    /// Record a failure on one domain, alerting the operator when it is an
    /// issuance failure rather than a deferral the budget already explains.
    async fn record_failure(
        &self,
        hostname: &str,
        tenant: &str,
        now_unix: i64,
        reason: impl Into<String>,
        alert: bool,
    ) {
        let reason = reason.into();
        let failures = self
            .registry
            .get(hostname)
            .filter(|d| d.tenant == tenant)
            .map_or(0, |d| d.consecutive_failures)
            .saturating_add(1);
        let backoff = i64::try_from(self.limiter.backoff_for(failures)).unwrap_or(i64::MAX);
        // Only while this tenant still owns the hostname. An order runs across
        // several awaits: if the domain was offboarded and re-registered
        // meanwhile, charging the failure here would put one tenant's reason
        // and backoff on the next tenant's record.
        match self
            .registry
            .record_failure_for(hostname, tenant, now_unix, reason.clone(), backoff)
            .await
        {
            Ok(true) => {}
            Ok(false) => {
                tracing::debug!(
                    hostname,
                    tenant,
                    "discarded a custom-domain failure: the hostname no longer belongs to this \
                     tenant"
                );
                return;
            }
            Err(e) => {
                tracing::warn!(hostname, "failed to persist custom-domain failure: {e}");
            }
        }
        // Naming the domain AND the tenant is what lets an operator act on the
        // alert without a lookup — one tenant's broken domain among a thousand.
        let message =
            format!("custom domain {hostname} (tenant {tenant}) failed to issue: {reason}");
        tracing::warn!("{message}");
        if alert {
            (self.reporter)(message);
        }
    }

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
        // Normalise once, up front: the registry normalises internally, but the
        // cache key, the budget history and the certificate id all derive from
        // the raw string, so `offboard("APP.ClientCo.com.")` would drop the
        // record and leave the private key on disk.
        let Ok(host) = crate::custom_domain::normalize_hostname(hostname) else {
            return Ok(false);
        };
        let removed = self.registry.remove(&host).await?;
        self.cache.remove(&host);
        self.limiter.forget(&host);
        if let Err(e) = self.certs.delete_cert(&cert_id_for(&host)).await {
            tracing::warn!(hostname = %host, "failed to delete the offboarded certificate: {e}");
        }
        Ok(removed)
    }

    /// Offboard every domain a tenant owns. Returns how many were removed.
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
            if self.offboard(&hostname).await? {
                removed += 1;
            }
        }
        Ok(removed)
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
        match self.certs.load_cert(&cert_id_for(hostname)).await {
            Ok(Some(stored)) => match crate::tls::certified_key_from_pem(
                stored.chain_pem.as_bytes(),
                stored.key_pem.as_bytes(),
                &self.provider,
            ) {
                Ok(certified) => {
                    self.cache.insert(hostname, certified);
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
        Box::pin(async move {
            // An index that failed to hydrate knows nothing, so EVERY tenant
            // certificate would read as an orphan and be deleted. One transient
            // read error at boot must not cost the deployment every private key
            // it holds.
            if !self.registry.is_hydrated() {
                return Err(
                    "refusing to prune: the custom-domain registry did not load at boot, so \
                     every certificate would look orphaned"
                        .to_owned(),
                );
            }
            let mut removed = 0_u64;
            // Abandoned connections: a tenant was handed DNS instructions and
            // never published the record. Nothing else ever deletes these.
            for domain in self.registry.list() {
                if domain.status == crate::custom_domain::DomainStatus::PendingDns
                    && domain.registered_at_unix < cutoff_unix
                {
                    removed += 1;
                    if !dry_run {
                        self.offboard(&domain.hostname)
                            .await
                            .map_err(|e| format!("failed to offboard {}: {e}", domain.hostname))?;
                    }
                }
            }
            // Orphaned certificates: a pair whose hostname is no longer
            // registered at all. Pruned regardless of the cutoff — there is no
            // record left to age.
            removed += self.prune_orphan_certs(dry_run)?;
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
    /// Delete stored certificate pairs no registered hostname maps to.
    ///
    /// Only certificates whose [`CertId`] matches some *removed* custom domain
    /// can be identified: an id is a hash, so the deployment's own certificate
    /// (and any other) is left alone by construction — we delete only ids that
    /// no longer appear in the registry AND are not the configured cert.
    fn prune_orphan_certs(&self, dry_run: bool) -> Result<u64, String> {
        let Some(fs) = self.cert_store_paths.as_ref() else {
            // A non-filesystem store cannot be enumerated through this seam.
            return Ok(0);
        };
        let live: std::collections::HashSet<String> = self
            .registry
            .list()
            .into_iter()
            .map(|d| cert_id_for(&d.hostname).as_str().to_owned())
            .collect();
        let stored = fs
            .list_certs()
            .map_err(|e| format!("failed to enumerate stored certificates: {e}"))?;
        let mut removed = 0;
        for (id, chain, key) in stored {
            if live.contains(id.as_str()) || self.retained_cert_ids.contains(id.as_str()) {
                continue;
            }
            if dry_run {
                removed += 1;
                continue;
            }
            let mut deleted = true;
            for path in [&chain, &key] {
                if let Err(e) = std::fs::remove_file(path) {
                    tracing::warn!(path = %path.display(), "failed to remove an orphaned certificate: {e}");
                    deleted = false;
                }
            }
            // Count only what actually went: the retention report must not
            // claim a deletion that failed.
            if deleted {
                removed += 1;
            }
        }
        Ok(removed)
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
}
