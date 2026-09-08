//! The custom-domain orchestrator: verify, issue, renew, offboard (#1635).
//!
//! Drives [`CustomDomainTask`] one tick at a time against fake
//! [`DomainVerifier`] / [`DomainIssuer`] seams, so the *policy* — what gets
//! ordered, when, and what happens after a failure — is asserted without a CA
//! and without wall-clock waits. The ACME wire protocol is covered by
//! `acme_end_to_end.rs`; what a real order does is not re-tested here.

use std::collections::{HashMap, HashSet};
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
///
/// With `heal_after` set, a named hostname fails only its first `heal_after`
/// attempts and succeeds afterwards — enough to drive a failure-then-recovery
/// sequence without a second issuer.
#[derive(Debug)]
struct ScriptedIssuer {
    calls: Mutex<Vec<String>>,
    failing: Vec<String>,
    heal_after: usize,
    count: AtomicUsize,
}

impl ScriptedIssuer {
    fn new(failing: &[&str]) -> Arc<Self> {
        Self::healing(failing, usize::MAX)
    }

    fn healing(failing: &[&str], heal_after: usize) -> Arc<Self> {
        Arc::new(Self {
            calls: Mutex::new(Vec::new()),
            failing: failing.iter().map(|h| (*h).to_owned()).collect(),
            heal_after,
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
            let attempts = {
                let mut calls = self.calls.lock().unwrap();
                calls.push(hostname.to_owned());
                calls.iter().filter(|h| *h == hostname).count()
            };
            self.count.fetch_add(1, Ordering::SeqCst);
            if self.failing.iter().any(|h| h == hostname) && attempts <= self.heal_after {
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
    store: Arc<FsAcmeStore>,
    registry: Arc<CustomDomainRegistry>,
    cache: Arc<CustomDomainCertCache>,
    alerts: Arc<Mutex<Vec<String>>>,
    recovered: Arc<Mutex<Vec<String>>>,
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
    let store = Arc::new(FsAcmeStore::new(dir.path(), "staging"));
    let store_out = Arc::clone(&store);
    let registry = Arc::new(CustomDomainRegistry::new(
        Arc::new(MemoryCustomDomainStore::new()),
        100,
    ));
    // `load()` marks the registry hydrated, which the retention prune requires
    // before it will delete anything.
    futures::executor::block_on(registry.load()).unwrap();
    let cache = Arc::new(CustomDomainCertCache::new(8));
    let alerts: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&alerts);
    let recovered: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let recovered_out = Arc::clone(&recovered);
    let task = CustomDomainTask {
        registry: Arc::clone(&registry),
        cache: Arc::clone(&cache),
        certs: Arc::clone(&store) as Arc<dyn autumn_web::acme::store::AcmeStore>,
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
        recovery: Some(Arc::new({
            let sink = Arc::clone(&recovered);
            move || sink.lock().unwrap().push("recovered".to_owned())
        })),
        coordinator: Arc::new(autumn_web::scheduler::InProcessSchedulerCoordinator::new(
            "test-replica",
        )),
        leadership_degraded: false,
        cert_store_paths: Some(store),
        retained_cert_ids: HashSet::new(),
    };
    Harness {
        task,
        store: store_out,
        registry,
        cache,
        alerts,
        recovered: recovered_out,
        _dir: dir,
    }
}

/// A task over caller-supplied stores, for the restart and scale tests.
fn task_over(
    registry: Arc<CustomDomainRegistry>,
    cache: Arc<CustomDomainCertCache>,
    certs: Arc<FsAcmeStore>,
    verifier: Arc<dyn DomainVerifier>,
    issuer: Arc<dyn DomainIssuer>,
) -> CustomDomainTask {
    CustomDomainTask {
        registry,
        cache,
        certs: Arc::clone(&certs) as Arc<dyn autumn_web::acme::store::AcmeStore>,
        provider: autumn_web::tls::crypto_provider(),
        verifier,
        issuer,
        limiter: Arc::new(IssuanceLimiter::new(5, 5000, 300, 86_400)),
        ingress: ExpectedIngress {
            hostname: Some("ingress.myapp.com".to_owned()),
            ipv4: vec!["203.0.113.10".parse().unwrap()],
            ipv6: vec![],
        },
        renew_before_days: 30,
        reporter: Arc::new(|_| {}),
        recovery: None,
        coordinator: Arc::new(autumn_web::scheduler::InProcessSchedulerCoordinator::new(
            "test-replica",
        )),
        leadership_degraded: false,
        cert_store_paths: Some(certs),
        retained_cert_ids: HashSet::new(),
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
    assert_eq!(
        issuer.count(),
        0,
        "zero ACME orders for an unverified domain"
    );
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
    assert!(
        record
            .failure_reason
            .as_deref()
            .unwrap()
            .contains("resolve")
    );
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
    assert_eq!(
        issuer.count(),
        before,
        "a backed-off domain must not re-order"
    );

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
    // Both already hold a certificate, so only the renew window decides.
    for host in ["soon.clientco.com", "later.clientco.com"] {
        h.task
            .certs
            .save_cert(
                &CertId::from_domains(&[host.to_owned()]),
                &autumn_web::acme::store::StoredCert {
                    chain_pem: CERT_PEM.to_owned(),
                    key_pem: KEY_PEM.to_owned(),
                },
            )
            .await
            .unwrap();
    }
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
    assert_eq!(
        issuer.count(),
        before,
        "an offboarded domain must not renew"
    );
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

// ── AC5: the alert clears only when nothing is failing ───────────────────

#[tokio::test]
async fn recovery_fires_only_once_every_domain_is_healthy() {
    // Both domains fail their first order, then heal. Two successes land in
    // one tick, and only the second — after which nothing is failing — may
    // clear the operator alert.
    let issuer = ScriptedIssuer::healing(&["a.clientco.com", "b.clientco.com"], 1);
    let h = harness(
        TableVerifier::new(&[
            ("a.clientco.com", points_here()),
            ("b.clientco.com", points_here()),
        ]),
        Arc::clone(&issuer) as Arc<dyn DomainIssuer>,
    );
    h.registry
        .register("a.clientco.com", "tenant-a", NOW)
        .await
        .unwrap();
    h.registry
        .register("b.clientco.com", "tenant-b", NOW)
        .await
        .unwrap();

    h.task.tick(NOW).await;
    assert!(
        h.recovered.lock().unwrap().is_empty(),
        "nothing has recovered while both domains are failing"
    );

    h.task.tick(NOW + 301).await;
    assert_eq!(
        h.registry.get("a.clientco.com").unwrap().status,
        DomainStatus::Active
    );
    assert_eq!(
        h.registry.get("b.clientco.com").unwrap().status,
        DomainStatus::Active
    );
    assert_eq!(
        h.recovered.lock().unwrap().len(),
        1,
        "the alert clears exactly once, when the LAST failing domain recovers"
    );
}

// ── AC7: retention prunes abandoned records and orphaned certificates ────

#[tokio::test]
async fn retention_prunes_abandoned_registrations_and_orphaned_certificates() {
    use autumn_web::custom_domain::CustomDomainPruner as _;

    let issuer = ScriptedIssuer::new(&[]);
    let h = harness(
        TableVerifier::new(&[("live.clientco.com", points_here())]),
        Arc::clone(&issuer) as Arc<dyn DomainIssuer>,
    );
    h.registry
        .register("live.clientco.com", "tenant-a", NOW)
        .await
        .unwrap();
    h.task.tick(NOW).await;
    // Never published DNS.
    h.registry
        .register("abandoned.clientco.com", "tenant-b", NOW)
        .await
        .unwrap();
    // A certificate whose registry record is already gone.
    h.task
        .certs
        .save_cert(
            &CertId::from_domains(&["orphan.clientco.com".to_owned()]),
            &autumn_web::acme::store::StoredCert {
                chain_pem: CERT_PEM.to_owned(),
                key_pem: KEY_PEM.to_owned(),
            },
        )
        .await
        .unwrap();

    let cutoff = NOW + 86_400;
    // A dry run reports without deleting.
    assert_eq!(h.task.prune(cutoff, true).await.unwrap(), 2);
    assert!(h.registry.get("abandoned.clientco.com").is_some());

    assert_eq!(h.task.prune(cutoff, false).await.unwrap(), 2);
    assert!(h.registry.get("abandoned.clientco.com").is_none());
    assert!(
        h.task
            .certs
            .load_cert(&CertId::from_domains(&["orphan.clientco.com".to_owned()]))
            .await
            .unwrap()
            .is_none()
    );
    // The live domain and its certificate are untouched.
    assert!(h.registry.get("live.clientco.com").is_some());
    assert!(
        h.task
            .certs
            .load_cert(&CertId::from_domains(&["live.clientco.com".to_owned()]))
            .await
            .unwrap()
            .is_some()
    );
}

// ── AC5: health names the domain and the tenant ──────────────────────────

#[tokio::test]
async fn health_is_up_while_a_failing_domain_still_serves_and_down_once_it_expires() {
    use autumn_web::actuator::HealthStatus;
    use autumn_web::custom_domain::CustomDomainHealthIndicator;

    let issuer = ScriptedIssuer::new(&[]);
    let h = harness(
        TableVerifier::new(&[]),
        Arc::clone(&issuer) as Arc<dyn DomainIssuer>,
    );
    h.registry
        .register("app.clientco.com", "tenant-a", NOW)
        .await
        .unwrap();
    h.registry
        .record_active("app.clientco.com", NOW, NOW + 86_400)
        .await
        .unwrap();
    h.registry
        .record_failure("app.clientco.com", NOW, "the CA rejected the order", 300)
        .await
        .unwrap();

    let indicator = CustomDomainHealthIndicator::new(Arc::clone(&h.registry));
    let graded = indicator.grade(NOW);
    assert_eq!(
        graded.status,
        HealthStatus::Up,
        "a failed renewal on a still-valid certificate is not an outage"
    );
    let failing = serde_json::to_string(&graded.details["failing"]).unwrap();
    assert!(failing.contains("app.clientco.com"), "{failing}");
    assert!(failing.contains("tenant-a"), "{failing}");

    // Once the certificate is actually dead the deployment is Down.
    assert_eq!(indicator.grade(NOW + 86_401).status, HealthStatus::Down);
}

// ── Regressions found by review ──────────────────────────────────────────

#[tokio::test]
async fn an_ingress_configured_only_as_a_hostname_still_verifies() {
    // `getaddrinfo` follows CNAMEs and reports addresses, never the CNAME, so
    // an operator who configures only `ingress_hostname` would see every
    // tenant domain sit at pending_dns forever unless the ingress hostname is
    // resolved to addresses and compared against those.
    let issuer = ScriptedIssuer::new(&[]);
    let mut h = harness(
        TableVerifier::new(&[
            ("app.clientco.com", points_here()),
            ("ingress.myapp.com", points_here()),
        ]),
        Arc::clone(&issuer) as Arc<dyn DomainIssuer>,
    );
    h.task.ingress = ExpectedIngress {
        hostname: Some("ingress.myapp.com".to_owned()),
        ipv4: vec![],
        ipv6: vec![],
    };
    h.registry
        .register("app.clientco.com", "tenant-a", NOW)
        .await
        .unwrap();

    h.task.tick(NOW).await;
    assert_eq!(
        h.registry.get("app.clientco.com").unwrap().status,
        DomainStatus::Active,
        "a hostname-only ingress must still verify"
    );
}

#[tokio::test]
async fn a_domain_pointing_elsewhere_still_fails_against_a_hostname_only_ingress() {
    let issuer = ScriptedIssuer::new(&[]);
    let mut h = harness(
        TableVerifier::new(&[
            ("app.clientco.com", points_elsewhere()),
            ("ingress.myapp.com", points_here()),
        ]),
        Arc::clone(&issuer) as Arc<dyn DomainIssuer>,
    );
    h.task.ingress = ExpectedIngress {
        hostname: Some("ingress.myapp.com".to_owned()),
        ipv4: vec![],
        ipv6: vec![],
    };
    h.registry
        .register("app.clientco.com", "tenant-a", NOW)
        .await
        .unwrap();

    h.task.tick(NOW).await;
    assert_eq!(
        h.registry.get("app.clientco.com").unwrap().status,
        DomainStatus::PendingDns
    );
    assert_eq!(issuer.count(), 0);
}

#[tokio::test]
async fn an_ingress_that_cannot_be_resolved_does_not_pass_everything() {
    // If the ingress hostname itself does not resolve, the expected set is
    // empty — which must NOT be read as "every address matches".
    let issuer = ScriptedIssuer::new(&[]);
    let mut h = harness(
        TableVerifier::new(&[("app.clientco.com", points_here())]),
        Arc::clone(&issuer) as Arc<dyn DomainIssuer>,
    );
    h.task.ingress = ExpectedIngress {
        hostname: Some("unresolvable.myapp.com".to_owned()),
        ipv4: vec![],
        ipv6: vec![],
    };
    h.registry
        .register("app.clientco.com", "tenant-a", NOW)
        .await
        .unwrap();

    h.task.tick(NOW).await;
    assert_eq!(
        h.registry.get("app.clientco.com").unwrap().status,
        DomainStatus::PendingDns
    );
    assert_eq!(issuer.count(), 0);
}

#[tokio::test]
async fn a_handshake_for_a_domain_past_the_cache_loads_its_certificate_from_disk() {
    use autumn_web::acme::tenant_domains::FsSniCertSource;
    use autumn_web::custom_domain::{CustomDomainCertCache, SniCertResolver};

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

    // A cache far smaller than the registry: the certificate is evicted, and
    // the handshake must still be served — otherwise a 1,000-domain
    // deployment with a 256-entry cache silently stops serving 744 tenants
    // after a restart.
    let cache = Arc::new(CustomDomainCertCache::new(1));
    let base = Arc::new(autumn_web::tls::ReloadableCertResolver::new(
        autumn_web::tls::certified_key_from_pem(
            CERT_PEM.as_bytes(),
            KEY_PEM.as_bytes(),
            &autumn_web::tls::crypto_provider(),
        )
        .unwrap(),
    ));
    let resolver = SniCertResolver::new(
        base,
        vec!["myapp.com".to_owned()],
        Arc::clone(&h.registry),
        Arc::clone(&cache),
    )
    .with_source(Arc::new(FsSniCertSource::new(
        Arc::clone(&h.store),
        autumn_web::tls::crypto_provider(),
    )));

    assert!(cache.get("app.clientco.com").is_none());
    assert!(
        resolver.certificate_for("app.clientco.com").is_some(),
        "a cold domain must load its certificate on the handshake path"
    );
    assert!(
        cache.get("app.clientco.com").is_some(),
        "and be cached after"
    );
    // An unregistered hostname is still refused without ever touching disk.
    assert!(resolver.certificate_for("attacker.example.net").is_none());
}

#[tokio::test]
async fn an_active_domain_whose_certificate_vanished_is_reordered() {
    // A torn write, a partial restore or a manual delete leaves the record
    // `Active` with a far-future `notAfter`. The domain is refused at the
    // handshake but is NOT due for renewal, so without a recovery pass it stays
    // hard down for as long as that window is away.
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
    assert_eq!(issuer.count(), 1);

    // Lose the certificate behind the task's back.
    h.task
        .certs
        .delete_cert(&CertId::from_domains(&["app.clientco.com".to_owned()]))
        .await
        .unwrap();
    h.cache.remove("app.clientco.com");
    assert_eq!(
        h.registry.get("app.clientco.com").unwrap().status,
        DomainStatus::Active,
        "the record still claims a certificate that is gone"
    );

    h.task.tick(NOW + 1).await;
    assert_eq!(
        issuer.count(),
        2,
        "the missing certificate must be re-ordered"
    );
    assert!(h.cache.get("app.clientco.com").is_some());
}

#[tokio::test]
async fn a_renewal_is_skipped_when_the_domain_no_longer_points_here() {
    // Verification gates the FIRST order. Without re-checking, a tenant who
    // repointed their domain is renewed forever — each cycle spending an order
    // plus a failed validation against the shared ACME account.
    let issuer = ScriptedIssuer::new(&[]);
    let h = harness(
        TableVerifier::new(&[("app.clientco.com", points_elsewhere())]),
        Arc::clone(&issuer) as Arc<dyn DomainIssuer>,
    );
    h.task
        .certs
        .save_cert(
            &CertId::from_domains(&["app.clientco.com".to_owned()]),
            &autumn_web::acme::store::StoredCert {
                chain_pem: CERT_PEM.to_owned(),
                key_pem: KEY_PEM.to_owned(),
            },
        )
        .await
        .unwrap();
    h.registry
        .register("app.clientco.com", "tenant-a", NOW)
        .await
        .unwrap();
    h.registry
        .record_active("app.clientco.com", NOW, NOW + 10 * 86_400)
        .await
        .unwrap();

    h.task.tick(NOW).await;
    assert_eq!(
        issuer.count(),
        0,
        "a domain that moved away must not be renewed"
    );
}

#[tokio::test]
async fn a_degraded_leadership_refuses_to_order_and_says_why() {
    let issuer = ScriptedIssuer::new(&[]);
    let mut h = harness(
        TableVerifier::new(&[("app.clientco.com", points_here())]),
        Arc::clone(&issuer) as Arc<dyn DomainIssuer>,
    );
    h.task.leadership_degraded = true;
    h.registry
        .register("app.clientco.com", "tenant-a", NOW)
        .await
        .unwrap();

    h.task.tick(NOW).await;
    assert_eq!(
        issuer.count(),
        0,
        "every replica ordering the same certificate would race the CA"
    );
    let record = h.registry.get("app.clientco.com").unwrap();
    assert!(
        record
            .failure_reason
            .as_deref()
            .unwrap()
            .contains("coordinator"),
        "{record:?}"
    );
}

#[tokio::test]
async fn a_certificate_ordered_for_an_offboarded_domain_is_discarded() {
    // The order takes a round trip. If the domain is offboarded meanwhile,
    // installing would resurrect the deleted certificate and promote a record
    // that was never verified.
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
    h.task.offboard("app.clientco.com").await.unwrap();

    // Re-registered by a DIFFERENT tenant while the old order was in flight.
    h.registry
        .register("app.clientco.com", "tenant-b", NOW)
        .await
        .unwrap();
    let installed = h
        .task
        .certs
        .load_cert(&CertId::from_domains(&["app.clientco.com".to_owned()]))
        .await
        .unwrap();
    assert!(installed.is_none(), "offboarding deleted the certificate");
    assert_eq!(
        h.registry.get("app.clientco.com").unwrap().status,
        DomainStatus::PendingDns,
        "the new tenant starts from pending, not from the old tenant's certificate"
    );
}

#[tokio::test]
async fn the_prune_refuses_to_run_when_the_registry_never_hydrated() {
    use autumn_web::custom_domain::CustomDomainPruner as _;

    let dir = tempfile::tempdir().unwrap();
    let store = Arc::new(FsAcmeStore::new(dir.path(), "staging"));
    // A registry that was never `load()`ed stands in for one whose load failed.
    let registry = Arc::new(CustomDomainRegistry::new(
        Arc::new(MemoryCustomDomainStore::new()),
        10,
    ));
    store
        .save_cert(
            &CertId::from_domains(&["app.clientco.com".to_owned()]),
            &autumn_web::acme::store::StoredCert {
                chain_pem: CERT_PEM.to_owned(),
                key_pem: KEY_PEM.to_owned(),
            },
        )
        .await
        .unwrap();

    let task = CustomDomainTask {
        registry,
        cache: Arc::new(CustomDomainCertCache::new(4)),
        certs: Arc::clone(&store) as Arc<dyn autumn_web::acme::store::AcmeStore>,
        provider: autumn_web::tls::crypto_provider(),
        verifier: TableVerifier::new(&[]),
        issuer: ScriptedIssuer::new(&[]),
        limiter: Arc::new(IssuanceLimiter::new(5, 50, 300, 86_400)),
        ingress: ExpectedIngress::default(),
        renew_before_days: 30,
        reporter: Arc::new(|_| {}),
        recovery: None,
        coordinator: Arc::new(autumn_web::scheduler::InProcessSchedulerCoordinator::new(
            "test-replica",
        )),
        leadership_degraded: false,
        cert_store_paths: Some(store.clone()),
        retained_cert_ids: HashSet::new(),
    };

    assert!(
        task.prune(NOW + 86_400, false).await.is_err(),
        "an index that never loaded would treat every certificate as an orphan"
    );
    assert!(
        store
            .load_cert(&CertId::from_domains(&["app.clientco.com".to_owned()]))
            .await
            .unwrap()
            .is_some(),
        "nothing may be deleted"
    );
}

// ── AC5/AC6: restart and scale, against the real on-disk stores ──────────

#[tokio::test]
async fn a_restart_serves_every_connected_domain_without_reordering() {
    use autumn_web::custom_domain::{CustomDomainStore as _, FsCustomDomainStore};

    let dir = tempfile::tempdir().unwrap();
    let registry_dir = dir.path().join("domains");
    let certs = Arc::new(FsAcmeStore::new(dir.path().join("acme"), "staging"));
    let issuer = ScriptedIssuer::new(&[]);

    // First boot: connect two domains and let them go active.
    {
        let store = Arc::new(FsCustomDomainStore::new(&registry_dir));
        let registry = Arc::new(CustomDomainRegistry::new(store, 100));
        registry.load().await.unwrap();
        let task = task_over(
            Arc::clone(&registry),
            Arc::new(CustomDomainCertCache::new(8)),
            Arc::clone(&certs),
            TableVerifier::new(&[
                ("a.clientco.com", points_here()),
                ("b.clientco.com", points_here()),
            ]),
            Arc::clone(&issuer) as Arc<dyn DomainIssuer>,
        );
        registry
            .register("a.clientco.com", "tenant-a", NOW)
            .await
            .unwrap();
        registry
            .register("b.clientco.com", "tenant-b", NOW)
            .await
            .unwrap();
        task.tick(NOW).await;
        assert_eq!(issuer.count(), 2);
    }

    // Second boot: fresh registry and cache over the SAME directories.
    let store = Arc::new(FsCustomDomainStore::new(&registry_dir));
    assert_eq!(
        store.load_all().await.unwrap().len(),
        2,
        "records must survive"
    );
    let registry = Arc::new(CustomDomainRegistry::new(store, 100));
    assert_eq!(registry.load().await.unwrap(), 2);
    let cache = Arc::new(CustomDomainCertCache::new(8));
    let task = task_over(
        Arc::clone(&registry),
        Arc::clone(&cache),
        Arc::clone(&certs),
        TableVerifier::new(&[]),
        Arc::clone(&issuer) as Arc<dyn DomainIssuer>,
    );

    // Routing survives.
    assert_eq!(
        registry.tenant_for_host("a.clientco.com").as_deref(),
        Some("tenant-a")
    );
    // Serving survives: the certificates load back from the store.
    for host in ["a.clientco.com", "b.clientco.com"] {
        assert!(task.warm(host).await, "{host} must reload from the store");
        assert!(cache.get(host).is_some());
    }
    // And nothing was re-ordered.
    task.tick(NOW + 1).await;
    assert_eq!(issuer.count(), 2, "a restart must not re-order anything");
}

#[tokio::test]
async fn a_thousand_domains_serve_the_right_certificate_through_a_cache_of_two_hundred() {
    use autumn_web::acme::tenant_domains::FsSniCertSource;
    use autumn_web::custom_domain::SniCertResolver;

    const DOMAINS: usize = 1000;
    const CACHE: usize = 200;

    let dir = tempfile::tempdir().unwrap();
    let certs = Arc::new(FsAcmeStore::new(dir.path(), "staging"));
    let registry = Arc::new(CustomDomainRegistry::new(
        Arc::new(MemoryCustomDomainStore::new()),
        DOMAINS,
    ));
    registry.load().await.unwrap();

    // Half the domains get the base fixture, half the renewed one, so "the
    // RIGHT certificate" is checkable rather than just "a certificate".
    for i in 0..DOMAINS {
        let host = format!("tenant{i}.clientco.com");
        let renewed = i % 2 == 1;
        let (chain, key) = if renewed {
            (RENEWED_CERT_PEM, RENEWED_KEY_PEM)
        } else {
            (CERT_PEM, KEY_PEM)
        };
        certs
            .save_cert(
                &CertId::from_domains(std::slice::from_ref(&host)),
                &autumn_web::acme::store::StoredCert {
                    chain_pem: chain.to_owned(),
                    key_pem: key.to_owned(),
                },
            )
            .await
            .unwrap();
        registry
            .register(&host, &format!("tenant-{i}"), NOW)
            .await
            .unwrap();
        registry
            .record_active(&host, NOW, NOW + 80 * 86_400)
            .await
            .unwrap();
    }

    let cache = Arc::new(CustomDomainCertCache::new(CACHE));
    let provider = autumn_web::tls::crypto_provider();
    let base = Arc::new(autumn_web::tls::ReloadableCertResolver::new(
        autumn_web::tls::certified_key_from_pem(CERT_PEM.as_bytes(), KEY_PEM.as_bytes(), &provider)
            .unwrap(),
    ));
    let resolver = SniCertResolver::new(
        base,
        vec!["myapp.com".to_owned()],
        Arc::clone(&registry),
        Arc::clone(&cache),
    )
    .with_source(Arc::new(FsSniCertSource::new(
        Arc::clone(&certs),
        Arc::clone(&provider),
    )));

    // The expected leaf DER for each half, to compare what SNI actually served.
    let plain =
        autumn_web::tls::certified_key_from_pem(CERT_PEM.as_bytes(), KEY_PEM.as_bytes(), &provider)
            .unwrap()
            .cert[0]
            .to_vec();
    let renewed = autumn_web::tls::certified_key_from_pem(
        RENEWED_CERT_PEM.as_bytes(),
        RENEWED_KEY_PEM.as_bytes(),
        &provider,
    )
    .unwrap()
    .cert[0]
        .to_vec();

    // Every domain resolves correctly, in an order that guarantees the cache
    // is thrashed — 1,000 lookups through 200 slots.
    for i in 0..DOMAINS {
        let host = format!("tenant{i}.clientco.com");
        let served = resolver
            .certificate_for(&host)
            .unwrap_or_else(|| panic!("{host} must be served"));
        let expected = if i % 2 == 1 { &renewed } else { &plain };
        assert_eq!(
            served.cert[0].as_ref(),
            expected.as_slice(),
            "{host} was served the wrong certificate"
        );
    }
    assert_eq!(cache.len(), CACHE, "the cache stayed bounded throughout");
    // An unregistered name is still refused, at scale.
    assert!(resolver.certificate_for("attacker.example.net").is_none());
}

// ── Codex round 1 ────────────────────────────────────────────────────────

#[tokio::test]
async fn a_certificate_cannot_activate_a_tenant_that_took_over_mid_order() {
    // The ownership check runs before `save_cert` awaits. If the owner is
    // offboarded and the hostname re-registered in that window, an
    // unconditional activation would carry the NEW tenant from pending_dns
    // straight to active on someone else's certificate, without its DNS ever
    // being verified.
    let h = harness(
        TableVerifier::new(&[("app.clientco.com", points_here())]),
        ScriptedIssuer::new(&[]) as Arc<dyn DomainIssuer>,
    );
    h.registry
        .register("app.clientco.com", "tenant-a", NOW)
        .await
        .unwrap();
    h.registry
        .record_verified("app.clientco.com", NOW)
        .await
        .unwrap();

    // Tenant B takes the hostname over while tenant A's order is in flight.
    h.registry.remove("app.clientco.com").await.unwrap();
    h.registry
        .register("app.clientco.com", "tenant-b", NOW)
        .await
        .unwrap();

    // Activating for the ORIGINAL owner must not apply.
    let applied = h
        .registry
        .record_active_for("app.clientco.com", "tenant-a", NOW, NOW + 86_400)
        .await
        .unwrap();
    assert!(!applied, "activation must not cross tenants");

    let record = h.registry.get("app.clientco.com").unwrap();
    assert_eq!(record.tenant, "tenant-b");
    assert_eq!(
        record.status,
        DomainStatus::PendingDns,
        "the new tenant must still prove its own DNS"
    );
    assert!(record.cert_not_after_unix.is_none());
    assert!(!h.registry.is_servable("app.clientco.com"));

    // And the same call for the CURRENT owner does apply.
    assert!(
        h.registry
            .record_active_for("app.clientco.com", "tenant-b", NOW, NOW + 86_400)
            .await
            .unwrap()
    );
    assert_eq!(
        h.registry.get("app.clientco.com").unwrap().status,
        DomainStatus::Active
    );
}

/// An issuer that hands the hostname to another tenant mid-order, reproducing
/// the window between `install`'s ownership check and its activation.
#[derive(Debug)]
struct TakeoverIssuer {
    registry: Arc<CustomDomainRegistry>,
}

impl DomainIssuer for TakeoverIssuer {
    fn issue<'a>(&'a self, hostname: &'a str) -> BoxFuture<'a, Result<IssuedCertificate, String>> {
        Box::pin(async move {
            self.registry.remove(hostname).await.unwrap();
            self.registry
                .register(hostname, "tenant-b", NOW)
                .await
                .unwrap();
            Ok(IssuedCertificate {
                chain_pem: CERT_PEM.to_owned(),
                key_pem: KEY_PEM.to_owned(),
            })
        })
    }
}

#[tokio::test]
async fn a_certificate_issued_for_a_hostname_that_changed_hands_is_discarded() {
    // End to end through `install`: the takeover happens between the ownership
    // check and activation, so the certificate belongs to nobody and must not
    // be left on disk or in the cache.
    let dir = tempfile::tempdir().unwrap();
    let certs = Arc::new(FsAcmeStore::new(dir.path(), "staging"));
    let registry = Arc::new(CustomDomainRegistry::new(
        Arc::new(MemoryCustomDomainStore::new()),
        10,
    ));
    registry.load().await.unwrap();
    let cache = Arc::new(CustomDomainCertCache::new(4));
    let task = task_over(
        Arc::clone(&registry),
        Arc::clone(&cache),
        Arc::clone(&certs),
        TableVerifier::new(&[("app.clientco.com", points_here())]),
        Arc::new(TakeoverIssuer {
            registry: Arc::clone(&registry),
        }) as Arc<dyn DomainIssuer>,
    );

    registry
        .register("app.clientco.com", "tenant-a", NOW)
        .await
        .unwrap();
    task.tick(NOW).await;

    let record = registry.get("app.clientco.com").unwrap();
    assert_eq!(record.tenant, "tenant-b");
    assert_eq!(
        record.status,
        DomainStatus::PendingDns,
        "the tenant that took the hostname over must still prove its own DNS"
    );
    assert!(cache.get("app.clientco.com").is_none());
    assert!(
        certs
            .load_cert(&CertId::from_domains(&["app.clientco.com".to_owned()]))
            .await
            .unwrap()
            .is_none(),
        "a certificate nobody owns must not be left on disk"
    );
}
