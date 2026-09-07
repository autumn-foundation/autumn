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
        other => panic!("expected a CNAME instruction, got {other:?}"),
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
        other => panic!("expected address records, got {other:?}"),
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
