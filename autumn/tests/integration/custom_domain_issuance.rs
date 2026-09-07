//! The custom-domain orchestrator: verify, issue, renew, offboard (#1635).
//!
//! Drives [`CustomDomainTask`] one tick at a time against fake
//! [`DomainVerifier`] / [`DomainIssuer`] seams, so the *policy* — what gets
//! ordered, when, and what happens after a failure — is asserted without a CA
//! and without wall-clock waits. The ACME wire protocol is covered by
//! `acme_end_to_end.rs`; what a real order does is not re-tested here.

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use autumn_web::acme::store::{AcmeStore as _, CertId, FsAcmeStore};

use autumn_web::acme::tenant_domains::CustomDomainTask;
use autumn_web::custom_domain::{
    CustomDomainCertCache, CustomDomainRegistry, DomainIssuer, DomainStatus, DomainVerifier,
    ExpectedIngress, IssuanceLimiter, IssuedCertificate, MemoryCustomDomainStore, ObservedTarget,
};
use futures::future::BoxFuture;

use super::tls_support::{CERT_PEM, KEY_PEM, RENEWED_CERT_PEM, RENEWED_KEY_PEM};

const NOW: i64 = 1_800_000_000;

// ── Seams ────────────────────────────────────────────────────────────────

/// A verifier answering from a fixed table; anything absent does not resolve.
#[derive(Debug)]
struct TableVerifier(HashMap<String, ObservedTarget>);

impl TableVerifier {
    fn new(entries: &[(&str, ObservedTarget)]) -> Arc<Self> {
        Arc::new(Self(
            entries
                .iter()
                .map(|(host, target)| ((*host).to_owned(), target.clone()))
                .collect(),
        ))
    }
}

impl DomainVerifier for TableVerifier {
    fn observe<'a>(&'a self, hostname: &'a str) -> BoxFuture<'a, ObservedTarget> {
        Box::pin(async move {
            self.0
                .get(hostname)
                .cloned()
                .unwrap_or(ObservedTarget::None)
        })
    }
}

/// An issuer that records every hostname it was asked about and fails for the
/// hostnames named in `failing`.
#[derive(Debug)]
struct ScriptedIssuer {
    calls: Mutex<Vec<String>>,
    failing: Vec<String>,
    count: AtomicUsize,
}

impl ScriptedIssuer {
    fn new(failing: &[&str]) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            failing: failing.iter().map(|h| (*h).to_owned()).collect(),
            count: AtomicUsize::new(0),
        })
    }

    fn calls(&self) -> Vec<String> {
        self.calls.lock().unwrap().clone()
    }

    fn count(&self) -> usize {
        self.count.load(Ordering::SeqCst)
    }
}

impl DomainIssuer for ScriptedIssuer {
    fn issue<'a>(&'a self, hostname: &'a str) -> BoxFuture<'a, Result<IssuedCertificate, String>> {
        Box::pin(async move {
            self.calls.lock().unwrap().push(hostname.to_owned());
            self.count.fetch_add(1, Ordering::SeqCst);
            if self.failing.iter().any(|h| h == hostname) {
                return Err("the CA rejected the order".to_owned());
            }
            Ok(IssuedCertificate {
                chain_pem: CERT_PEM.to_owned(),
                key_pem: KEY_PEM.to_owned(),
            })
        })
    }
}

// ── Harness ──────────────────────────────────────────────────────────────

struct Harness {
    task: CustomDomainTask,
    registry: Arc<CustomDomainRegistry>,
    cache: Arc<CustomDomainCertCache>,
    alerts: Arc<Mutex<Vec<String>>>,
    _dir: tempfile::TempDir,
}

fn harness(verifier: Arc<dyn DomainVerifier>, issuer: Arc<dyn DomainIssuer>) -> Harness {
    harness_with_limiter(verifier, issuer, IssuanceLimiter::new(5, 50, 300, 86_400))
}

fn harness_with_limiter(
    verifier: Arc<dyn DomainVerifier>,
    issuer: Arc<dyn DomainIssuer>,
    limiter: IssuanceLimiter,
) -> Harness {
    let dir = tempfile::tempdir().unwrap();
    let registry = Arc::new(CustomDomainRegistry::new(
        Arc::new(MemoryCustomDomainStore::new()),
        100,
    ));
    let cache = Arc::new(CustomDomainCertCache::new(8));
    let alerts: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&alerts);
    let task = CustomDomainTask {
        registry: Arc::clone(&registry),
        cache: Arc::clone(&cache),
        certs: Arc::new(FsAcmeStore::new(dir.path(), "staging")),
        provider: autumn_web::tls::crypto_provider(),
        verifier,
        issuer,
        limiter: Arc::new(limiter),
        ingress: ExpectedIngress {
            hostname: Some("ingress.myapp.com".to_owned()),
            ipv4: vec!["203.0.113.10".parse().unwrap()],
            ipv6: vec![],
        },
        renew_before_days: 30,
        reporter: Arc::new(move |message: String| sink.lock().unwrap().push(message)),
    };
    Harness {
        task,
        registry,
        cache,
        alerts,
        _dir: dir,
    }
}

fn points_here() -> ObservedTarget {
    ObservedTarget::Addresses(vec!["203.0.113.10".parse().unwrap()])
}

fn points_elsewhere() -> ObservedTarget {
    ObservedTarget::Addresses(vec!["198.51.100.7".parse().unwrap()])
}

// ── AC1/AC2: verification gates issuance ─────────────────────────────────

#[tokio::test]
async fn a_verified_domain_is_issued_activated_and_served() {
    let issuer = ScriptedIssuer::new(&[]);
    let h = harness(
        TableVerifier::new(&[("app.clientco.com", points_here())]),
        Arc::clone(&issuer) as Arc<dyn DomainIssuer>,
    );
    h.registry
        .register("app.clientco.com", "tenant-a", NOW)
        .await
        .unwrap();

    // A domain that verifies is ordered in the SAME tick: DNS has just been
    // proven, so waiting a poll interval would only delay time-to-active.
    h.task.tick(NOW).await;
    let active = h.registry.get("app.clientco.com").unwrap();
    assert_eq!(active.status, DomainStatus::Active);
    assert!(active.cert_not_after_unix.is_some());
    assert_eq!(issuer.calls(), vec!["app.clientco.com".to_owned()]);

    // The certificate is both served and persisted, so a restart re-serves it.
    assert!(h.cache.get("app.clientco.com").is_some());
    let stored = h
        .task
        .certs
        .load_cert(&CertId::from_domains(&["app.clientco.com".to_owned()]))
        .await
        .unwrap();
    assert!(stored.is_some(), "the issued certificate must be persisted");
}

#[tokio::test]
async fn a_domain_pointing_elsewhere_is_never_ordered() {
    let issuer = ScriptedIssuer::new(&[]);
    let h = harness(
        TableVerifier::new(&[("app.clientco.com", points_elsewhere())]),
        Arc::clone(&issuer) as Arc<dyn DomainIssuer>,
    );
    h.registry
        .register("app.clientco.com", "tenant-a", NOW)
        .await
        .unwrap();

    for tick in 0..5 {
        h.task.tick(NOW + tick * 100_000).await;
    }

    let record = h.registry.get("app.clientco.com").unwrap();
    assert_eq!(record.status, DomainStatus::PendingDns);
    assert!(
        record
            .failure_reason
            .as_deref()
            .unwrap()
            .contains("198.51.100.7"),
        "the status must explain why: {record:?}"
    );
    assert_eq!(issuer.count(), 0, "zero ACME orders for an unverified domain");
}

#[tokio::test]
async fn an_unresolved_domain_says_so_without_ordering() {
    let issuer = ScriptedIssuer::new(&[]);
    let h = harness(
        TableVerifier::new(&[]),
        Arc::clone(&issuer) as Arc<dyn DomainIssuer>,
    );
    h.registry
        .register("app.clientco.com", "tenant-a", NOW)
        .await
        .unwrap();
    h.task.tick(NOW).await;

    let record = h.registry.get("app.clientco.com").unwrap();
    assert_eq!(record.status, DomainStatus::PendingDns);
    assert!(record.failure_reason.as_deref().unwrap().contains("resolve"));
    assert_eq!(issuer.count(), 0);
}

// ── AC4/AC5: budgets, backoff, isolation ─────────────────────────────────

#[tokio::test]
async fn a_failing_domain_backs_off_and_leaves_its_neighbours_alone() {
    let issuer = ScriptedIssuer::new(&["bad.clientco.com"]);
    let h = harness(
        TableVerifier::new(&[
            ("bad.clientco.com", points_here()),
            ("good.clientco.com", points_here()),
        ]),
        Arc::clone(&issuer) as Arc<dyn DomainIssuer>,
    );
    h.registry
        .register("bad.clientco.com", "tenant-a", NOW)
        .await
        .unwrap();
    h.registry
        .register("good.clientco.com", "tenant-b", NOW)
        .await
        .unwrap();

    h.task.tick(NOW).await; // verify and order both

    let bad = h.registry.get("bad.clientco.com").unwrap();
    assert_eq!(bad.status, DomainStatus::Verified);
    assert_eq!(bad.consecutive_failures, 1);
    assert_eq!(bad.next_attempt_unix, Some(NOW + 300));

    let good = h.registry.get("good.clientco.com").unwrap();
    assert_eq!(
        good.status,
        DomainStatus::Active,
        "one tenant's failure must not stop another's issuance"
    );
    assert!(h.cache.get("good.clientco.com").is_some());

    // The alert names the domain AND the tenant.
    let alerts = h.alerts.lock().unwrap().clone();
    assert!(
        alerts
            .iter()
            .any(|a| a.contains("bad.clientco.com") && a.contains("tenant-a")),
        "{alerts:?}"
    );

    // Inside its backoff the failed domain is not retried.
    let before = issuer.count();
    h.task.tick(NOW + 2).await;
    assert_eq!(issuer.count(), before, "a backed-off domain must not re-order");

    // Past the backoff it is.
    h.task.tick(NOW + 301).await;
    assert_eq!(issuer.count(), before + 1);
}

#[tokio::test]
async fn a_spent_issuance_budget_defers_the_order_without_contacting_the_ca() {
    let issuer = ScriptedIssuer::new(&[]);
    // Zero global headroom: the budget refuses before any order is placed.
    let limiter = IssuanceLimiter::new(5, 1, 300, 86_400);
    limiter.record_attempt("someone.else.com", NOW);
    let h = harness_with_limiter(
        TableVerifier::new(&[("app.clientco.com", points_here())]),
        Arc::clone(&issuer) as Arc<dyn DomainIssuer>,
        limiter,
    );
    h.registry
        .register("app.clientco.com", "tenant-a", NOW)
        .await
        .unwrap();

    h.task.tick(NOW).await;

    assert_eq!(issuer.count(), 0, "a spent budget must not reach the CA");
    let record = h.registry.get("app.clientco.com").unwrap();
    assert_eq!(record.status, DomainStatus::Verified);
    assert!(
        record.failure_reason.as_deref().unwrap().contains("budget"),
        "{record:?}"
    );
}

// ── AC5: renewal ─────────────────────────────────────────────────────────

#[tokio::test]
async fn only_domains_inside_the_renew_window_are_reissued() {
    /// A second issuer handing back the *renewed* fixture, so a re-issue is
    /// observable as a different served certificate.
    #[derive(Debug, Default)]
    struct RenewingIssuer(AtomicUsize);
    impl DomainIssuer for RenewingIssuer {
        fn issue<'a>(
            &'a self,
            _hostname: &'a str,
        ) -> BoxFuture<'a, Result<IssuedCertificate, String>> {
            Box::pin(async move {
                self.0.fetch_add(1, Ordering::SeqCst);
                Ok(IssuedCertificate {
                    chain_pem: RENEWED_CERT_PEM.to_owned(),
                    key_pem: RENEWED_KEY_PEM.to_owned(),
                })
            })
        }
    }

    let issuer = Arc::new(RenewingIssuer::default());
    let h = harness(
        TableVerifier::new(&[]),
        Arc::clone(&issuer) as Arc<dyn DomainIssuer>,
    );
    // Expires in 10 days: inside the 30-day window.
    h.registry
        .register("soon.clientco.com", "tenant-a", NOW)
        .await
        .unwrap();
    h.registry
        .record_active("soon.clientco.com", NOW, NOW + 10 * 86_400)
        .await
        .unwrap();
    // Expires in 80 days: outside it.
    h.registry
        .register("later.clientco.com", "tenant-b", NOW)
        .await
        .unwrap();
    h.registry
        .record_active("later.clientco.com", NOW, NOW + 80 * 86_400)
        .await
        .unwrap();

    h.task.tick(NOW).await;

    assert_eq!(issuer.0.load(Ordering::SeqCst), 1);
    assert!(h.cache.get("soon.clientco.com").is_some());
    assert!(
        h.cache.get("later.clientco.com").is_none(),
        "a certificate outside its renew window must not be re-ordered"
    );
}

// ── AC7: offboarding ─────────────────────────────────────────────────────

#[tokio::test]
async fn an_offboarded_domain_stops_being_served_and_renewed() {
    let issuer = ScriptedIssuer::new(&[]);
    let h = harness(
        TableVerifier::new(&[("app.clientco.com", points_here())]),
        Arc::clone(&issuer) as Arc<dyn DomainIssuer>,
    );
    h.registry
        .register("app.clientco.com", "tenant-a", NOW)
        .await
        .unwrap();
    h.task.tick(NOW).await;
    assert!(h.cache.get("app.clientco.com").is_some());

    h.task.offboard("app.clientco.com").await.unwrap();

    assert!(h.registry.get("app.clientco.com").is_none());
    assert!(h.cache.get("app.clientco.com").is_none());
    let stored = h
        .task
        .certs
        .load_cert(&CertId::from_domains(&["app.clientco.com".to_owned()]))
        .await
        .unwrap();
    assert!(
        stored.is_none(),
        "the stored certificate must be deleted, not orphaned"
    );

    let before = issuer.count();
    h.task.tick(NOW + 100_000).await;
    assert_eq!(issuer.count(), before, "an offboarded domain must not renew");
}

// ── AC6: incremental certificate loading ─────────────────────────────────

#[tokio::test]
async fn a_cold_domain_is_reloaded_from_the_store_rather_than_reordered() {
    let issuer = ScriptedIssuer::new(&[]);
    let h = harness(
        TableVerifier::new(&[("app.clientco.com", points_here())]),
        Arc::clone(&issuer) as Arc<dyn DomainIssuer>,
    );
    h.registry
        .register("app.clientco.com", "tenant-a", NOW)
        .await
        .unwrap();
    h.task.tick(NOW).await;

    // Simulate an eviction (or a restart with a cold cache).
    h.cache.remove("app.clientco.com");
    assert!(h.cache.get("app.clientco.com").is_none());

    assert!(
        h.task.warm("app.clientco.com").await,
        "the certificate must load back from the store"
    );
    assert!(h.cache.get("app.clientco.com").is_some());
    assert_eq!(issuer.count(), 1, "warming must not place a new order");
}
