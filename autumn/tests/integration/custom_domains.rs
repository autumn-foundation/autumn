//! Tenant custom domains with per-domain ACME certificates (issue #1635).
//!
//! Covers the whole journey: register a hostname for a tenant, render the DNS
//! instructions the tenant needs, gate ACME issuance on an independent DNS
//! check, resolve the tenant from the registered `Host`, serve that domain's
//! certificate by SNI, renew per domain, and offboard cleanly.
//!
//! Every ACME interaction goes through the [`DomainIssuer`] seam, so these
//! tests assert the *orchestration* — how many orders are placed, and when —
//! without a CA. `acme_fake_ca.rs` already covers the wire protocol.

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use autumn_web::config::AutumnConfig;
use autumn_web::custom_domain::{
    CustomDomainRegistry, CustomDomainStore as _, DnsInstructions, DomainStatus, ExpectedIngress,
    IssuanceDecision, IssuanceLimiter, IssuedCertificate, MemoryCustomDomainStore, ObservedTarget,
    RegisterError, VerificationOutcome, grade_dns_verification, normalize_hostname,
};
use autumn_web::tenancy::extract_tenant_from_parts_with_domains;
use axum::http::Request;

// ── Fixtures ─────────────────────────────────────────────────────────────

const NOW: i64 = 1_800_000_000;

fn ingress() -> ExpectedIngress {
    ExpectedIngress {
        hostname: Some("ingress.myapp.com".to_owned()),
        ipv4: vec!["203.0.113.10".parse().unwrap()],
        ipv6: vec![],
    }
}

fn registry() -> Arc<CustomDomainRegistry> {
    Arc::new(CustomDomainRegistry::new(
        Arc::new(MemoryCustomDomainStore::new()),
        1000,
    ))
}

/// A [`DomainIssuer`] that records every hostname it was asked to issue for,
/// so a test can assert "zero ACME orders" rather than trusting a status
/// string.
#[derive(Debug, Default)]
struct CountingIssuer {
    calls: AtomicUsize,
}

impl CountingIssuer {
    fn count(&self) -> usize {
        self.calls.load(Ordering::SeqCst)
    }
}

impl autumn_web::custom_domain::DomainIssuer for CountingIssuer {
    fn issue<'a>(
        &'a self,
        _hostname: &'a str,
    ) -> futures::future::BoxFuture<'a, Result<IssuedCertificate, String>> {
        Box::pin(async move {
            self.calls.fetch_add(1, Ordering::SeqCst);
            Err("no CA in this test".to_owned())
        })
    }
}

// ── AC1: the connect-your-domain journey ─────────────────────────────────

#[test]
fn hostnames_are_normalised_and_validated() {
    assert_eq!(
        normalize_hostname(" App.ClientCo.COM. ").unwrap(),
        "app.clientco.com"
    );
    // A port is not part of a hostname the CA can be asked about.
    assert_eq!(
        normalize_hostname("app.clientco.com:443").unwrap(),
        "app.clientco.com"
    );

    for bad in [
        "",
        "*.clientco.com",
        "192.0.2.1",
        "no-dot",
        "-lead.clientco.com",
        "under_score.clientco.com",
        "a..b.com",
    ] {
        assert!(
            normalize_hostname(bad).is_err(),
            "expected {bad:?} to be rejected"
        );
    }
}

#[test]
fn dns_instructions_are_cname_for_subdomains_and_addresses_for_apex() {
    let sub = DnsInstructions::for_hostname("app.clientco.com", &ingress()).unwrap();
    match &sub {
        DnsInstructions::Cname { name, value } => {
            assert_eq!(name, "app.clientco.com");
            assert_eq!(value, "ingress.myapp.com");
        }
        other @ DnsInstructions::Address { .. } => {
            panic!("expected a CNAME instruction, got {other:?}")
        }
    }
    assert!(sub.render().contains("CNAME"));

    // An apex domain cannot carry a CNAME, so it gets A/AAAA records.
    let apex = DnsInstructions::for_hostname("clientco.com", &ingress()).unwrap();
    match &apex {
        DnsInstructions::Address { name, ipv4, ipv6 } => {
            assert_eq!(name, "clientco.com");
            assert_eq!(ipv4, &["203.0.113.10".to_owned()]);
            assert!(ipv6.is_empty());
        }
        other @ DnsInstructions::Cname { .. } => {
            panic!("expected address records, got {other:?}")
        }
    }
    assert!(apex.render().contains('A'));
}

#[tokio::test]
async fn a_domain_walks_pending_to_active_and_is_queryable_at_every_step() {
    let registry = registry();
    let domain = registry
        .register("App.ClientCo.com", "tenant-a", NOW)
        .await
        .unwrap();
    assert_eq!(domain.hostname, "app.clientco.com");
    assert_eq!(domain.status, DomainStatus::PendingDns);

    registry
        .record_verified("app.clientco.com", NOW + 60)
        .await
        .unwrap();
    assert_eq!(
        registry.get("app.clientco.com").unwrap().status,
        DomainStatus::Verified
    );

    registry.record_issuing("app.clientco.com").await.unwrap();
    assert_eq!(
        registry.get("app.clientco.com").unwrap().status,
        DomainStatus::Issuing
    );

    registry
        .record_active("app.clientco.com", NOW + 90, NOW + 90 * 86_400)
        .await
        .unwrap();
    let active = registry.get("app.clientco.com").unwrap();
    assert_eq!(active.status, DomainStatus::Active);
    assert_eq!(active.cert_not_after_unix, Some(NOW + 90 * 86_400));
    assert!(active.failure_reason.is_none());

    // The app renders per-tenant status.
    let listed = registry.list_for_tenant("tenant-a");
    assert_eq!(listed.len(), 1);
    assert_eq!(listed[0].hostname, "app.clientco.com");
}

#[tokio::test]
async fn a_stuck_domain_reports_why() {
    let registry = registry();
    registry
        .register("app.clientco.com", "tenant-a", NOW)
        .await
        .unwrap();
    registry
        .record_failure(
            "app.clientco.com",
            NOW + 10,
            "DNS for app.clientco.com resolves to 198.51.100.7, not this deployment",
            300,
        )
        .await
        .unwrap();

    let stuck = registry.get("app.clientco.com").unwrap();
    assert_eq!(stuck.status, DomainStatus::PendingDns);
    assert!(
        stuck
            .failure_reason
            .as_deref()
            .unwrap()
            .contains("198.51.100.7"),
        "the failure reason must name what was observed: {stuck:?}"
    );
    assert_eq!(stuck.next_attempt_unix, Some(NOW + 310));
}

#[tokio::test]
async fn registering_the_same_hostname_for_another_tenant_is_refused() {
    let registry = registry();
    registry
        .register("app.clientco.com", "tenant-a", NOW)
        .await
        .unwrap();
    let err = registry
        .register("app.clientco.com", "tenant-b", NOW)
        .await
        .unwrap_err();
    assert!(matches!(err, RegisterError::Conflict { .. }), "{err:?}");
}

// ── AC2: the verification gate ───────────────────────────────────────────

#[test]
fn verification_grades_where_the_hostname_actually_points() {
    let expected = ingress();
    assert_eq!(
        grade_dns_verification(
            &ObservedTarget::Cname("ingress.myapp.com.".to_owned()),
            &expected
        ),
        VerificationOutcome::PointsHere
    );
    assert_eq!(
        grade_dns_verification(
            &ObservedTarget::Addresses(vec!["203.0.113.10".parse().unwrap()]),
            &expected
        ),
        VerificationOutcome::PointsHere
    );
    assert!(matches!(
        grade_dns_verification(
            &ObservedTarget::Addresses(vec!["198.51.100.7".parse().unwrap()]),
            &expected
        ),
        VerificationOutcome::PointsElsewhere { .. }
    ));
    assert_eq!(
        grade_dns_verification(&ObservedTarget::None, &expected),
        VerificationOutcome::Unresolved
    );
}

#[tokio::test]
async fn a_domain_pointing_elsewhere_never_reaches_the_acme_provider() {
    let registry = registry();
    let issuer = Arc::new(CountingIssuer::default());
    registry
        .register("app.clientco.com", "tenant-a", NOW)
        .await
        .unwrap();

    let outcome = VerificationOutcome::PointsElsewhere {
        detail: "resolves to 198.51.100.7".to_owned(),
    };
    autumn_web::custom_domain::apply_verification(
        &registry,
        "app.clientco.com",
        &outcome,
        NOW,
        300,
    )
    .await
    .unwrap();

    let after = registry.get("app.clientco.com").unwrap();
    assert_eq!(after.status, DomainStatus::PendingDns);
    assert!(
        after
            .failure_reason
            .as_deref()
            .unwrap()
            .contains("198.51.100.7")
    );

    // Nothing is due for issuance, so the issuer is never called.
    assert!(registry.due_for_issuance(NOW + 1).is_empty());
    assert_eq!(
        issuer.count(),
        0,
        "an unverified domain must place no ACME order"
    );
}

// ── AC3: request routing for a registered domain ─────────────────────────

#[tokio::test]
async fn a_verified_custom_domain_resolves_to_its_tenant() {
    let mut config = AutumnConfig::default();
    config.tenancy.enabled = true;
    config.tenancy.source = "subdomain".to_owned();
    config.tenancy.base_domain = Some("myapp.com".to_owned());

    let registry = registry();
    registry
        .register("app.clientco.com", "tenant-a", NOW)
        .await
        .unwrap();

    // Still pending: the base-domain rejection still applies.
    let req = Request::builder()
        .header("Host", "app.clientco.com")
        .body(())
        .unwrap();
    let (mut parts, ()) = req.into_parts();
    assert!(
        extract_tenant_from_parts_with_domains(&mut parts, &config, Some(&registry))
            .await
            .is_err(),
        "an unverified domain must not route"
    );

    registry
        .record_active("app.clientco.com", NOW, NOW + 86_400)
        .await
        .unwrap();

    let req = Request::builder()
        .header("Host", "App.ClientCo.com:443")
        .body(())
        .unwrap();
    let (mut parts, ()) = req.into_parts();
    let tenant = extract_tenant_from_parts_with_domains(&mut parts, &config, Some(&registry))
        .await
        .expect("a registered, verified domain must resolve to its tenant");
    assert_eq!(tenant, "tenant-a");

    // Subdomain tenancy is untouched.
    let req = Request::builder()
        .header("Host", "acme.myapp.com")
        .body(())
        .unwrap();
    let (mut parts, ()) = req.into_parts();
    assert_eq!(
        extract_tenant_from_parts_with_domains(&mut parts, &config, Some(&registry))
            .await
            .unwrap(),
        "acme"
    );

    // An unregistered outside host is still a 400.
    let req = Request::builder()
        .header("Host", "evil.example.net")
        .body(())
        .unwrap();
    let (mut parts, ()) = req.into_parts();
    assert!(
        extract_tenant_from_parts_with_domains(&mut parts, &config, Some(&registry))
            .await
            .is_err()
    );
}

// ── AC4: on-demand issuance safety ───────────────────────────────────────

#[test]
fn issuance_is_rate_limited_per_domain_and_globally() {
    let limiter = IssuanceLimiter::new(2, 3, 300, 86_400);

    assert_eq!(limiter.check("a.test", NOW), IssuanceDecision::Allow);
    limiter.record_attempt("a.test", NOW);
    assert_eq!(limiter.check("a.test", NOW), IssuanceDecision::Allow);
    limiter.record_attempt("a.test", NOW);
    // Third attempt for the same domain inside the window is refused.
    assert!(matches!(
        limiter.check("a.test", NOW),
        IssuanceDecision::PerDomainLimit { .. }
    ));

    // A different domain still gets through until the global budget is spent.
    assert_eq!(limiter.check("b.test", NOW), IssuanceDecision::Allow);
    limiter.record_attempt("b.test", NOW);
    assert!(matches!(
        limiter.check("c.test", NOW),
        IssuanceDecision::GlobalLimit { .. }
    ));
}

#[test]
fn repeated_failures_back_off_exponentially_and_cap() {
    let limiter = IssuanceLimiter::new(100, 100, 300, 3600);
    assert_eq!(limiter.check("a.test", NOW), IssuanceDecision::Allow);
    assert_eq!(limiter.backoff_for(2), 600);
    assert_eq!(autumn_web::custom_domain::backoff_secs(1, 300, 3600), 300);
    assert_eq!(autumn_web::custom_domain::backoff_secs(2, 300, 3600), 600);
    assert_eq!(autumn_web::custom_domain::backoff_secs(3, 300, 3600), 1200);
    // Capped, and never overflows for an absurd failure count.
    assert_eq!(autumn_web::custom_domain::backoff_secs(64, 300, 3600), 3600);
}

#[tokio::test]
async fn an_unregistered_sni_hostname_is_refused_without_contacting_the_ca() {
    let registry = registry();
    let issuer = Arc::new(CountingIssuer::default());
    registry
        .register("app.clientco.com", "tenant-a", NOW)
        .await
        .unwrap();
    registry
        .record_active("app.clientco.com", NOW, NOW + 86_400)
        .await
        .unwrap();

    assert!(registry.is_servable("app.clientco.com"));
    assert!(!registry.is_servable("attacker.example.net"));
    assert_eq!(issuer.count(), 0);
}

// ── AC6: scale ───────────────────────────────────────────────────────────

#[tokio::test]
async fn a_thousand_domains_register_and_resolve_without_per_domain_config() {
    let registry = Arc::new(CustomDomainRegistry::new(
        Arc::new(MemoryCustomDomainStore::new()),
        1000,
    ));
    for i in 0..1000 {
        let host = format!("tenant{i}.clientco.com");
        registry
            .register(&host, &format!("tenant-{i}"), NOW)
            .await
            .unwrap();
        registry
            .record_active(&host, NOW, NOW + 86_400)
            .await
            .unwrap();
    }
    assert_eq!(registry.len(), 1000);
    assert_eq!(
        registry
            .tenant_for_host("tenant999.clientco.com")
            .as_deref(),
        Some("tenant-999")
    );

    // The cap is enforced rather than silently exceeded.
    let err = registry
        .register("one.too.many.com", "tenant-x", NOW)
        .await
        .unwrap_err();
    assert!(matches!(err, RegisterError::LimitReached { .. }), "{err:?}");
}

// ── AC7: offboarding ─────────────────────────────────────────────────────

#[tokio::test]
async fn removing_a_domain_stops_routing_serving_and_renewal() {
    let store = Arc::new(MemoryCustomDomainStore::new());
    let registry = Arc::new(CustomDomainRegistry::new(store.clone(), 1000));
    registry
        .register("app.clientco.com", "tenant-a", NOW)
        .await
        .unwrap();
    registry
        .record_active("app.clientco.com", NOW, NOW + 86_400)
        .await
        .unwrap();

    assert!(registry.remove("app.clientco.com").await.unwrap());
    assert!(registry.get("app.clientco.com").is_none());
    assert!(!registry.is_servable("app.clientco.com"));
    assert!(registry.tenant_for_host("app.clientco.com").is_none());
    assert!(registry.due_for_renewal(NOW + 86_400, 30).is_empty());
    assert!(
        store.load_all().await.unwrap().is_empty(),
        "the record must be deleted, not orphaned"
    );

    // Removing twice is not an error.
    assert!(!registry.remove("app.clientco.com").await.unwrap());
}

#[tokio::test]
async fn offboarding_a_tenant_removes_every_domain_it_owns() {
    let registry = registry();
    for host in ["a.clientco.com", "b.clientco.com"] {
        registry.register(host, "tenant-a", NOW).await.unwrap();
    }
    registry
        .register("c.other.com", "tenant-b", NOW)
        .await
        .unwrap();

    assert_eq!(registry.remove_tenant("tenant-a").await.unwrap(), 2);
    assert!(registry.list_for_tenant("tenant-a").is_empty());
    assert_eq!(registry.list_for_tenant("tenant-b").len(), 1);
}

// ── AC5: renewal isolation ───────────────────────────────────────────────

#[tokio::test]
async fn renewal_is_due_per_domain_and_one_failure_leaves_the_others_alone() {
    let registry = registry();
    registry
        .register("soon.clientco.com", "tenant-a", NOW)
        .await
        .unwrap();
    registry
        .record_active("soon.clientco.com", NOW, NOW + 10 * 86_400)
        .await
        .unwrap();
    registry
        .register("later.clientco.com", "tenant-b", NOW)
        .await
        .unwrap();
    registry
        .record_active("later.clientco.com", NOW, NOW + 80 * 86_400)
        .await
        .unwrap();

    let due = registry.due_for_renewal(NOW, 30);
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].hostname, "soon.clientco.com");

    registry
        .record_failure("soon.clientco.com", NOW, "order failed", 300)
        .await
        .unwrap();

    // The failed domain keeps serving its still-valid certificate, and the
    // healthy domain is untouched.
    let failed = registry.get("soon.clientco.com").unwrap();
    assert_eq!(
        failed.status,
        DomainStatus::Active,
        "a renewal failure must not stop serving"
    );
    assert!(failed.failure_reason.is_some());
    let healthy = registry.get("later.clientco.com").unwrap();
    assert_eq!(healthy.status, DomainStatus::Active);
    assert!(healthy.failure_reason.is_none());

    // Health output names the domain and the tenant so an operator can act.
    let report = registry.health_report(NOW);
    assert!(report.contains("soon.clientco.com"), "{report}");
    assert!(report.contains("tenant-a"), "{report}");
}

// ── AC3/AC4: SNI certificate selection ───────────────────────────────────

#[cfg(feature = "tls")]
mod sni {
    use super::super::tls_support::{CERT_PEM, KEY_PEM, RENEWED_CERT_PEM, RENEWED_KEY_PEM};
    use super::{CustomDomainRegistry, MemoryCustomDomainStore, NOW};
    use autumn_web::custom_domain::{CustomDomainCertCache, SniCertResolver};
    use std::sync::Arc;

    /// Load a fixture pair into a `CertifiedKey`. Which fixture is irrelevant —
    /// the resolver selects on the registry, not on the leaf's SANs — so two
    /// distinct pairs are enough to prove "a different certificate was served".
    fn key(chain: &str, private: &str) -> Arc<rustls::sign::CertifiedKey> {
        autumn_web::tls::certified_key_from_pem(
            chain.as_bytes(),
            private.as_bytes(),
            &autumn_web::tls::crypto_provider(),
        )
        .unwrap()
    }

    #[tokio::test]
    async fn sni_serves_the_registered_domains_certificate_and_refuses_the_rest() {
        let registry = Arc::new(CustomDomainRegistry::new(
            Arc::new(MemoryCustomDomainStore::new()),
            10,
        ));
        registry
            .register("app.clientco.com", "tenant-a", NOW)
            .await
            .unwrap();
        registry
            .record_active("app.clientco.com", NOW, NOW + 86_400)
            .await
            .unwrap();

        let base = Arc::new(autumn_web::tls::ReloadableCertResolver::new(key(
            CERT_PEM, KEY_PEM,
        )));
        let cache = Arc::new(CustomDomainCertCache::new(4));
        cache.insert("app.clientco.com", key(RENEWED_CERT_PEM, RENEWED_KEY_PEM));

        let resolver = SniCertResolver::new(
            Arc::clone(&base),
            vec!["myapp.com".to_owned(), "*.myapp.com".to_owned()],
            Arc::clone(&registry),
            Arc::clone(&cache),
        );

        // A registered, active domain is served its OWN certificate.
        let served = resolver.certificate_for("app.clientco.com").unwrap();
        assert!(!Arc::ptr_eq(&served, &base.current()));
        // The operator's own names still get the base certificate.
        assert!(Arc::ptr_eq(
            &resolver.certificate_for("myapp.com").unwrap(),
            &base.current()
        ));
        assert!(resolver.certificate_for("acme.myapp.com").is_some());
        // An unregistered hostname is refused outright — the handshake fails
        // and nothing reaches the ACME provider.
        assert!(resolver.certificate_for("attacker.example.net").is_none());
        // Registered but not yet active: still refused.
        registry
            .register("pending.clientco.com", "tenant-b", NOW)
            .await
            .unwrap();
        assert!(resolver.certificate_for("pending.clientco.com").is_none());

        // Offboarding stops serving immediately.
        registry.remove("app.clientco.com").await.unwrap();
        cache.remove("app.clientco.com");
        assert!(resolver.certificate_for("app.clientco.com").is_none());
    }

    #[test]
    fn the_certificate_cache_is_bounded_and_evicts_oldest_first() {
        let cache = CustomDomainCertCache::new(2);
        for host in ["a.test", "b.test", "c.test"] {
            cache.insert(host, key(CERT_PEM, KEY_PEM));
        }
        assert_eq!(cache.len(), 2);
        assert!(
            cache.get("a.test").is_none(),
            "oldest entry must be evicted"
        );
        assert!(cache.get("c.test").is_some());

        cache.remove("c.test");
        assert!(cache.get("c.test").is_none());
    }
}

// ── Tenant isolation (review findings) ───────────────────────────────────

#[tokio::test]
async fn a_tenant_cannot_connect_a_hostname_the_deployment_already_owns() {
    // Without this, a low-tier tenant registers `acme.myapp.com` — another
    // tenant's subdomain — and, because the operator's own wildcard already
    // points it here, it verifies, issues, and from then on every request for
    // that host resolves to the ATTACKER's tenant.
    let store = Arc::new(MemoryCustomDomainStore::new());
    let registry = Arc::new(
        CustomDomainRegistry::new(store, 1000)
            .with_reserved(["myapp.com".to_owned(), "*.myapp.com".to_owned()]),
    );

    for reserved in [
        "myapp.com",
        "www.myapp.com",
        "acme.myapp.com",
        "MyApp.com",
        "deep.nested.myapp.com",
    ] {
        let err = registry
            .register(reserved, "tenant-evil", NOW)
            .await
            .unwrap_err();
        assert!(
            matches!(err, RegisterError::Reserved { .. }),
            "{reserved} must not be registrable: {err:?}"
        );
    }

    // A genuinely third-party hostname is unaffected.
    assert!(
        registry
            .register("app.clientco.com", "tenant-a", NOW)
            .await
            .is_ok()
    );
    // And so is a name that merely ends with the same letters.
    assert!(
        registry
            .register("notmyapp.com", "tenant-a", NOW)
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn a_custom_domain_never_overrides_an_authenticated_tenant_source() {
    // The registry answers on `Host`, which the client controls. Under
    // `jwt`/`session`/`header` tenancy the tenant comes from a verified
    // credential, so a `Host` a tenant connected must NOT outrank it —
    // otherwise any logged-in user reaches another tenant's data by setting a
    // header.
    let registry = registry();
    registry
        .register("app.clientco.com", "tenant-a", NOW)
        .await
        .unwrap();
    registry
        .record_active("app.clientco.com", NOW, NOW + 86_400)
        .await
        .unwrap();

    let mut config = AutumnConfig::default();
    config.tenancy.enabled = true;
    config.tenancy.source = "header".to_owned();
    config.tenancy.header_name = "x-tenant-id".to_owned();

    let req = Request::builder()
        .header("Host", "app.clientco.com")
        .header("x-tenant-id", "tenant-b")
        .body(())
        .unwrap();
    let (mut parts, ()) = req.into_parts();
    assert_eq!(
        extract_tenant_from_parts_with_domains(&mut parts, &config, Some(&registry))
            .await
            .unwrap(),
        "tenant-b",
        "the configured tenancy source must win over a client-supplied Host"
    );
}

// ── Codex round 2 ────────────────────────────────────────────────────────

#[tokio::test]
async fn a_tenant_cannot_claim_the_deployments_ingress_hostname() {
    // The ingress hostname already resolves to the ingress, so a tenant who
    // registered it would need no DNS change at all: verification passes on the
    // first tick, HTTP-01 validates, and every request to the deployment's own
    // infrastructure hostname would then route to that tenant. It is reserved
    // even when it sits outside the ACME domains and the tenancy base domain.
    let registry = Arc::new(
        CustomDomainRegistry::new(Arc::new(MemoryCustomDomainStore::new()), 10).with_reserved([
            "myapp.com".to_owned(),
            "*.myapp.com".to_owned(),
            // A separate infrastructure zone — not covered by the two above.
            "ingress.myapp-infra.net".to_owned(),
        ]),
    );

    let err = registry
        .register("ingress.myapp-infra.net", "tenant-a", NOW)
        .await
        .unwrap_err();
    assert!(matches!(err, RegisterError::Reserved { .. }), "{err:?}");

    // The operator's own names stay reserved too.
    for host in ["myapp.com", "acme.myapp.com"] {
        assert!(
            registry.register(host, "tenant-a", NOW).await.is_err(),
            "{host} must stay reserved"
        );
    }
    // An unrelated tenant hostname is still fine.
    assert!(
        registry
            .register("app.clientco.com", "tenant-a", NOW)
            .await
            .is_ok()
    );
}

#[tokio::test]
async fn an_offboard_is_not_overtaken_by_an_in_flight_save() {
    // Both writes target the same hostname. Without per-hostname serialisation
    // the save can land after the delete, leaving no index entry but a file on
    // disk — which the next restart hydrates as a live domain the app already
    // offboarded.
    let store = Arc::new(MemoryCustomDomainStore::new());
    let registry = Arc::new(CustomDomainRegistry::new(store.clone(), 10));
    registry.load().await.unwrap();
    registry
        .register("app.clientco.com", "tenant-a", NOW)
        .await
        .unwrap();

    let writer = {
        let registry = Arc::clone(&registry);
        tokio::spawn(async move {
            registry
                .record_verified("app.clientco.com", NOW + 1)
                .await
                .unwrap();
        })
    };
    let remover = {
        let registry = Arc::clone(&registry);
        tokio::spawn(async move { registry.remove("app.clientco.com").await.unwrap() })
    };
    writer.await.unwrap();
    remover.await.unwrap();

    // Whichever order they ran in, what a restart sees must match what this
    // process serves. Read the durable state the way a restart does — through
    // `load` — rather than through a test-only accessor, so this asserts the
    // real recovery path.
    let in_index = registry.get("app.clientco.com");
    let reloaded = CustomDomainRegistry::new(store, 10);
    reloaded.load().await.unwrap();
    assert_eq!(
        reloaded.get("app.clientco.com").is_none(),
        in_index.is_none(),
        "the store and the index disagree: a restart would resurrect a domain \
         the app offboarded (or lose one it kept)"
    );
}

/// A store whose `delete` blocks until released, so an offboard can be held
/// mid-flight while another task registers the same hostname.
#[derive(Debug)]
struct PausingDeleteStore {
    inner: MemoryCustomDomainStore,
    release: tokio::sync::Notify,
    paused: std::sync::atomic::AtomicBool,
    /// Fires when the held delete is entered, so a test synchronises on the
    /// suspension itself rather than on a number of `yield_now()`s — a guess
    /// about the scheduler, and a flake under a loaded test binary.
    entered: tokio::sync::Notify,
}

impl PausingDeleteStore {
    fn new() -> Self {
        Self {
            inner: MemoryCustomDomainStore::new(),
            release: tokio::sync::Notify::new(),
            paused: std::sync::atomic::AtomicBool::new(false),
            entered: tokio::sync::Notify::new(),
        }
    }

    /// Hold the next `delete` until [`release`](Self::release).
    fn pause_next_delete(&self) {
        self.paused.store(true, Ordering::SeqCst);
    }

    fn release(&self) {
        self.release.notify_one();
    }

    /// Resolves once the held `delete` has been entered.
    async fn reached_pause(&self) {
        self.entered.notified().await;
    }
}

impl autumn_web::custom_domain::CustomDomainStore for PausingDeleteStore {
    fn load_all(
        &self,
    ) -> autumn_web::custom_domain::StoreFuture<
        '_,
        std::io::Result<Vec<autumn_web::custom_domain::CustomDomain>>,
    > {
        self.inner.load_all()
    }

    fn save<'a>(
        &'a self,
        domain: &'a autumn_web::custom_domain::CustomDomain,
    ) -> autumn_web::custom_domain::StoreFuture<'a, std::io::Result<()>> {
        self.inner.save(domain)
    }

    fn delete<'a>(
        &'a self,
        hostname: &'a str,
    ) -> autumn_web::custom_domain::StoreFuture<'a, std::io::Result<()>> {
        Box::pin(async move {
            if self.paused.swap(false, Ordering::SeqCst) {
                self.entered.notify_one();
                self.release.notified().await;
            }
            self.inner.delete(hostname).await
        })
    }
}

#[tokio::test]
async fn a_registration_racing_an_offboard_of_the_same_hostname_stays_durable() {
    // A registration that returns success must be in the index AND in the
    // store. Claiming the hostname before taking the write gate let the
    // offboard erase the claim while the save still landed — or, as here,
    // report success for a record the offboard was about to delete.
    let store = Arc::new(PausingDeleteStore::new());
    let registry = Arc::new(CustomDomainRegistry::new(
        Arc::clone(&store) as Arc<dyn autumn_web::custom_domain::CustomDomainStore>,
        10,
    ));
    registry
        .register("app.clientco.com", "tenant-a", NOW)
        .await
        .unwrap();

    store.pause_next_delete();
    let offboard = tokio::spawn({
        let registry = Arc::clone(&registry);
        async move { registry.remove("app.clientco.com").await.unwrap() }
    });
    // Wait for the offboard to take the write gate and reach the held delete.
    store.reached_pause().await;
    let reconnect = tokio::spawn({
        let registry = Arc::clone(&registry);
        async move {
            registry
                .register("app.clientco.com", "tenant-a", NOW + 1)
                .await
        }
    });
    // The reconnect is now queued behind the same write gate; let it get there.
    tokio::task::yield_now().await;
    store.release();

    assert!(offboard.await.unwrap());
    reconnect
        .await
        .unwrap()
        .expect("the reconnect must succeed");

    assert!(
        registry.get("app.clientco.com").is_some(),
        "the hostname must still route in this process"
    );
    let restarted = CustomDomainRegistry::new(
        Arc::clone(&store) as Arc<dyn autumn_web::custom_domain::CustomDomainStore>,
        10,
    );
    assert_eq!(restarted.load().await.unwrap(), 1);
    assert!(
        restarted.get("app.clientco.com").is_some(),
        "and must survive a restart"
    );
}

#[tokio::test]
async fn a_malformed_ingress_hostname_is_refused_at_startup() {
    // The value reaches tenants verbatim as their CNAME target, so a scheme or
    // a port leaves every subdomain domain stuck at pending_dns with nothing
    // to explain why.
    let base = |hostname: &str| autumn_web::config::CustomDomainsConfig {
        enabled: true,
        ingress_hostname: Some(hostname.to_owned()),
        ..Default::default()
    };
    for bad in [
        "https://ingress.myapp.com",
        "ingress.myapp.com:443",
        "ingress.myapp.com/connect",
        "ingress",
        "*.myapp.com",
    ] {
        let error = base(bad)
            .validate()
            .expect_err(&format!("`{bad}` must be refused"));
        assert!(error.contains("ingress_hostname"), "{error}");
    }

    // A bare name passes, and case and a trailing dot are normalised rather
    // than refused.
    base("ingress.myapp.com").validate().unwrap();
    let config = base("Ingress.MyApp.com.");
    config.validate().unwrap();
    assert_eq!(
        config.ingress().hostname.as_deref(),
        Some("ingress.myapp.com")
    );
}

// ── Codex round 4 ────────────────────────────────────────────────────────

#[tokio::test]
async fn a_stored_domain_the_configuration_now_reserves_never_hydrates() {
    // Reservations come from configuration — the ACME domains, the tenancy
    // base domain, the ingress hostname — so a name that was a legitimate
    // third-party domain when it was registered becomes the deployment's own
    // the moment an operator adds that zone. `register` refuses a reserved
    // name, but the record persisted BEFORE the change is still on disk, and a
    // hydration that trusts it hands the operator's own hostname to whoever
    // registered it first: custom-domain lookup runs ahead of ordinary
    // subdomain tenancy, so the stale record wins every request after the
    // restart.
    let store = Arc::new(MemoryCustomDomainStore::new());
    let before = CustomDomainRegistry::new(
        Arc::clone(&store) as Arc<dyn autumn_web::custom_domain::CustomDomainStore>,
        10,
    );
    for (host, tenant) in [
        ("acme.myapp.com", "tenant-evil"),
        ("app.clientco.com", "tenant-a"),
    ] {
        before.register(host, tenant, NOW).await.unwrap();
        before.record_active(host, NOW, NOW + 86_400).await.unwrap();
    }

    // The operator now serves `myapp.com` themselves.
    let after = Arc::new(
        CustomDomainRegistry::new(
            Arc::clone(&store) as Arc<dyn autumn_web::custom_domain::CustomDomainStore>,
            10,
        )
        .with_reserved(["myapp.com".to_owned(), "*.myapp.com".to_owned()]),
    );
    assert_eq!(
        after.load().await.unwrap(),
        1,
        "only the still-legitimate domain may hydrate"
    );
    assert!(
        after.get("acme.myapp.com").is_none(),
        "a now-reserved hostname must not come back from the store"
    );
    assert!(
        after.tenant_for_host("acme.myapp.com").is_none(),
        "and must not route"
    );
    assert!(
        after.get("app.clientco.com").is_some(),
        "an unaffected domain still hydrates"
    );

    // The point of all of it: the reserved host resolves through ordinary
    // subdomain tenancy again, not to the tenant that had claimed it.
    let mut config = AutumnConfig::default();
    config.tenancy.enabled = true;
    config.tenancy.source = "subdomain".to_owned();
    config.tenancy.base_domain = Some("myapp.com".to_owned());
    let req = Request::builder()
        .header("Host", "acme.myapp.com")
        .body(())
        .unwrap();
    let (mut parts, ()) = req.into_parts();
    assert_eq!(
        extract_tenant_from_parts_with_domains(&mut parts, &config, Some(&after))
            .await
            .unwrap(),
        "acme",
        "the operator's own subdomain must resolve to its own tenant"
    );

    // Quarantined, not deleted: reverting the configuration restores it.
    let reverted = CustomDomainRegistry::new(
        Arc::clone(&store) as Arc<dyn autumn_web::custom_domain::CustomDomainStore>,
        10,
    );
    assert_eq!(reverted.load().await.unwrap(), 2);
}

/// A store whose `save` fails on demand, leaving `load_all` intact.
#[derive(Debug, Default)]
struct FailingSaveStore {
    inner: MemoryCustomDomainStore,
    fail: std::sync::atomic::AtomicBool,
}

impl FailingSaveStore {
    fn fail_saves(&self) {
        self.fail.store(true, Ordering::SeqCst);
    }
}

impl autumn_web::custom_domain::CustomDomainStore for FailingSaveStore {
    fn load_all(
        &self,
    ) -> autumn_web::custom_domain::StoreFuture<
        '_,
        std::io::Result<Vec<autumn_web::custom_domain::CustomDomain>>,
    > {
        self.inner.load_all()
    }

    fn save<'a>(
        &'a self,
        domain: &'a autumn_web::custom_domain::CustomDomain,
    ) -> autumn_web::custom_domain::StoreFuture<'a, std::io::Result<()>> {
        if self.fail.load(Ordering::SeqCst) {
            return Box::pin(async move { Err(std::io::Error::other("the disk is full")) });
        }
        self.inner.save(domain)
    }

    fn delete<'a>(
        &'a self,
        hostname: &'a str,
    ) -> autumn_web::custom_domain::StoreFuture<'a, std::io::Result<()>> {
        self.inner.delete(hostname)
    }
}

#[tokio::test]
async fn a_state_change_that_fails_to_persist_is_not_left_in_the_index() {
    // The index is what routes and what the SNI resolver reads, so publishing
    // a state the store rejected serves a domain this process cannot justify
    // after a restart: `record_active_for` reported failure while the domain
    // went on routing as `active`, and the state then vanished at the next
    // boot with nothing to explain it.
    let store = Arc::new(FailingSaveStore::default());
    let registry = CustomDomainRegistry::new(
        Arc::clone(&store) as Arc<dyn autumn_web::custom_domain::CustomDomainStore>,
        10,
    );
    registry
        .register("app.clientco.com", "tenant-a", NOW)
        .await
        .unwrap();
    registry
        .record_verified("app.clientco.com", NOW)
        .await
        .unwrap();

    store.fail_saves();
    registry
        .record_active_for("app.clientco.com", "tenant-a", NOW, NOW + 86_400)
        .await
        .expect_err("the store write must surface as an error");

    let live = registry.get("app.clientco.com").unwrap();
    assert_eq!(
        live.status,
        DomainStatus::Verified,
        "a state that did not reach the store must not be published to the index"
    );
    assert!(
        !registry.is_servable("app.clientco.com"),
        "and must not route or serve"
    );

    // A failure that cannot be persisted is dropped the same way: it would
    // otherwise show a tenant a reason and a backoff that disappear at the
    // next restart.
    registry
        .record_failure("app.clientco.com", NOW, "order rejected", 60)
        .await
        .expect_err("the store write must surface as an error");
    let live = registry.get("app.clientco.com").unwrap();
    assert_eq!(live.failure_reason, None);
    assert_eq!(live.consecutive_failures, 0);
    assert!(live.is_due(NOW));

    // The index and the store still agree.
    let restarted = CustomDomainRegistry::new(
        Arc::clone(&store) as Arc<dyn autumn_web::custom_domain::CustomDomainStore>,
        10,
    );
    restarted.load().await.unwrap();
    assert_eq!(
        restarted.get("app.clientco.com").unwrap(),
        registry.get("app.clientco.com").unwrap()
    );
}

// ── Codex round 11 ───────────────────────────────────────────────────────

#[tokio::test]
async fn a_registry_tenant_teardown_leaves_a_hostname_another_tenant_took_over() {
    // `remove_tenant` walks a snapshot and each removal awaits, so a hostname
    // freed early can be re-registered by ANOTHER tenant before a later one
    // runs. Removing it unconditionally stops routing a domain that was never
    // part of this teardown.
    let store = Arc::new(PausingDeleteStore::new());
    let registry = Arc::new(CustomDomainRegistry::new(
        Arc::clone(&store) as Arc<dyn autumn_web::custom_domain::CustomDomainStore>,
        10,
    ));
    // `list_for_tenant` sorts by hostname, so `a-` is removed first and holds
    // the teardown inside its store delete.
    for host in ["a-first.clientco.com", "b-second.clientco.com"] {
        registry.register(host, "tenant-a", NOW).await.unwrap();
    }

    store.pause_next_delete();
    let teardown = tokio::spawn({
        let registry = Arc::clone(&registry);
        async move { registry.remove_tenant("tenant-a").await.unwrap() }
    });
    store.reached_pause().await;

    // The second hostname changes hands while the teardown is suspended.
    assert!(registry.remove("b-second.clientco.com").await.unwrap());
    registry
        .register("b-second.clientco.com", "tenant-b", NOW + 1)
        .await
        .unwrap();
    store.release();

    assert_eq!(
        teardown.await.unwrap(),
        1,
        "only the hostname still belonging to tenant-a is torn down"
    );
    assert!(registry.get("a-first.clientco.com").is_none());
    let survivor = registry
        .get("b-second.clientco.com")
        .expect("the new tenant's domain must survive the old tenant's teardown");
    assert_eq!(survivor.tenant, "tenant-b");
}
