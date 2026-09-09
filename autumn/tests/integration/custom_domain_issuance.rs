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

    // ...and it is RECORDED, not merely logged. The certificate is inside its
    // renewal window and will expire, so an operator has to act: the tenant
    // repointed the record or gave the hostname up. Skipping silently left
    // health `Up` and raised no alert until the expiry took the domain down.
    let record = h.registry.get("app.clientco.com").unwrap();
    assert_eq!(
        record.status,
        DomainStatus::Active,
        "the certificate is still valid and still served"
    );
    let reason = record
        .failure_reason
        .clone()
        .expect("the skip must be recorded");
    assert!(reason.contains("renewal skipped"), "{reason}");
    assert!(
        !record.is_due(NOW),
        "and must back off rather than re-checking every tick"
    );
    let report = h.registry.health_report(NOW);
    assert!(report.contains("app.clientco.com"), "{report}");
    assert!(report.contains("tenant-a"), "{report}");
    let alerts = h.alerts.lock().unwrap().clone();
    assert_eq!(alerts.len(), 1, "the operator is alerted once: {alerts:?}");
    assert!(alerts[0].contains("app.clientco.com"), "{alerts:?}");
}

// ── Codex round 5 ────────────────────────────────────────────────────────

#[tokio::test]
async fn offboarding_the_last_failing_domain_clears_the_operator_alert() {
    // The alert is raised per failing domain and retracted only when nothing
    // is failing. Offboarding removes the failure from the registry without
    // being a success, so without clearing here the `scheduled_task_failure`
    // alert stood forever — for a domain that no longer exists.
    let issuer = ScriptedIssuer::new(&["doomed.clientco.com"]);
    let h = harness(
        TableVerifier::new(&[("doomed.clientco.com", points_here())]),
        Arc::clone(&issuer) as Arc<dyn DomainIssuer>,
    );
    h.registry
        .register("doomed.clientco.com", "tenant-a", NOW)
        .await
        .unwrap();

    h.task.tick(NOW).await;
    assert_eq!(
        h.alerts.lock().unwrap().len(),
        1,
        "the failure must alert the operator"
    );
    assert!(
        h.recovered.lock().unwrap().is_empty(),
        "nothing has recovered yet"
    );

    assert!(h.task.offboard("doomed.clientco.com").await.unwrap());
    assert!(
        h.registry.health_report(NOW).is_empty(),
        "no domain is failing any more"
    );
    assert_eq!(
        h.recovered.lock().unwrap().len(),
        1,
        "the standing alert must be retracted"
    );
}

#[tokio::test]
async fn a_tenant_teardown_leaves_a_hostname_another_tenant_took_over() {
    // `offboard_tenant` walks a snapshot and each offboard awaits, so a
    // hostname freed early can be re-registered by SOMEONE ELSE before a later
    // one runs. Removing it unconditionally disconnects that tenant — a domain
    // and a certificate belonging to someone who was never being torn down.
    let dir = tempfile::tempdir().unwrap();
    let certs = Arc::new(FsAcmeStore::new(dir.path(), "staging"));
    let store = Arc::new(PausingDeleteStore::new());
    let registry = Arc::new(CustomDomainRegistry::new(
        Arc::clone(&store) as Arc<dyn autumn_web::custom_domain::CustomDomainStore>,
        100,
    ));
    registry.load().await.unwrap();
    let task = Arc::new(task_over(
        Arc::clone(&registry),
        Arc::new(CustomDomainCertCache::new(8)),
        Arc::clone(&certs),
        TableVerifier::new(&[]) as Arc<dyn DomainVerifier>,
        ScriptedIssuer::new(&[]) as Arc<dyn DomainIssuer>,
    ));

    // `list_for_tenant` sorts by hostname, so `a-` is torn down first and holds
    // the sweep inside its store delete.
    for host in ["a-first.clientco.com", "b-second.clientco.com"] {
        registry.register(host, "tenant-a", NOW).await.unwrap();
    }

    let teardown = tokio::spawn({
        let task = Arc::clone(&task);
        async move { task.offboard_tenant("tenant-a").await.unwrap() }
    });
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    // The second hostname changes hands while the teardown is suspended.
    assert!(registry.remove("b-second.clientco.com").await.unwrap());
    registry
        .register("b-second.clientco.com", "tenant-b", NOW + 1)
        .await
        .unwrap();
    let successor_cert = CertId::from_domains(&["b-second.clientco.com".to_owned()]);
    certs
        .save_cert(
            &successor_cert,
            &autumn_web::acme::store::StoredCert {
                chain_pem: CERT_PEM.to_owned(),
                key_pem: KEY_PEM.to_owned(),
            },
        )
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
    assert!(
        certs.load_cert(&successor_cert).await.unwrap().is_some(),
        "and must keep its certificate"
    );
}

/// A certificate store that completes the first `save_cert` and then blocks
/// until released, so a test can act in the window between an order writing
/// its certificate and activating it.
#[derive(Debug)]
struct PauseAfterSaveStore {
    inner: Arc<FsAcmeStore>,
    gate: Mutex<Option<futures::channel::oneshot::Receiver<()>>>,
    release: Mutex<Option<futures::channel::oneshot::Sender<()>>>,
}

impl PauseAfterSaveStore {
    fn new(inner: Arc<FsAcmeStore>) -> Self {
        let (tx, rx) = futures::channel::oneshot::channel();
        Self {
            inner,
            gate: Mutex::new(Some(rx)),
            release: Mutex::new(Some(tx)),
        }
    }

    fn release(&self) {
        let sender = self.release.lock().unwrap().take();
        if let Some(tx) = sender {
            let _ = tx.send(());
        }
    }
}

impl autumn_web::acme::store::AcmeStore for PauseAfterSaveStore {
    fn load_account(
        &self,
    ) -> autumn_web::acme::store::StoreFuture<'_, std::io::Result<Option<Vec<u8>>>> {
        self.inner.load_account()
    }

    fn save_account<'a>(
        &'a self,
        data: &'a [u8],
    ) -> autumn_web::acme::store::StoreFuture<'a, std::io::Result<()>> {
        self.inner.save_account(data)
    }

    fn load_cert<'a>(
        &'a self,
        id: &'a CertId,
    ) -> autumn_web::acme::store::StoreFuture<
        'a,
        std::io::Result<Option<autumn_web::acme::store::StoredCert>>,
    > {
        self.inner.load_cert(id)
    }

    fn save_cert<'a>(
        &'a self,
        id: &'a CertId,
        cert: &'a autumn_web::acme::store::StoredCert,
    ) -> autumn_web::acme::store::StoreFuture<'a, std::io::Result<()>> {
        let held = self.gate.lock().unwrap().take();
        Box::pin(async move {
            self.inner.save_cert(id, cert).await?;
            if let Some(rx) = held {
                let _ = rx.await;
            }
            Ok(())
        })
    }

    fn delete_cert<'a>(
        &'a self,
        id: &'a CertId,
    ) -> autumn_web::acme::store::StoreFuture<'a, std::io::Result<()>> {
        self.inner.delete_cert(id)
    }
}

#[tokio::test]
async fn a_stale_order_does_not_delete_the_successors_certificate() {
    // The certificate id is a hash of the hostname, so a stale order and its
    // successor address the same file. Losing the activation race, the stale
    // order discards "its" certificate — but by then the successor has written
    // its own pair over that id, and deleting it leaves that tenant recorded
    // active with nothing on disk, failing every handshake until a repair
    // order completes.
    let dir = tempfile::tempdir().unwrap();
    let fs = Arc::new(FsAcmeStore::new(dir.path(), "staging"));
    let certs = Arc::new(PauseAfterSaveStore::new(Arc::clone(&fs)));
    let registry = Arc::new(CustomDomainRegistry::new(
        Arc::new(MemoryCustomDomainStore::new()),
        100,
    ));
    registry.load().await.unwrap();
    let task = Arc::new(CustomDomainTask {
        registry: Arc::clone(&registry),
        cache: Arc::new(CustomDomainCertCache::new(8)),
        certs: Arc::clone(&certs) as Arc<dyn autumn_web::acme::store::AcmeStore>,
        provider: autumn_web::tls::crypto_provider(),
        verifier: TableVerifier::new(&[("app.clientco.com", points_here())])
            as Arc<dyn DomainVerifier>,
        issuer: ScriptedIssuer::new(&[]) as Arc<dyn DomainIssuer>,
        limiter: Arc::new(IssuanceLimiter::new(5, 50, 300, 86_400)),
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
        cert_store_paths: Some(Arc::clone(&fs)),
        retained_cert_ids: HashSet::new(),
    });
    registry
        .register("app.clientco.com", "tenant-a", NOW)
        .await
        .unwrap();

    // tenant-a's order writes its certificate and suspends before activating.
    let tick = tokio::spawn({
        let task = Arc::clone(&task);
        async move { task.tick(NOW).await }
    });
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }

    // The hostname changes hands, and the successor installs its own pair over
    // the same certificate id and activates it.
    assert!(registry.remove("app.clientco.com").await.unwrap());
    registry
        .register("app.clientco.com", "tenant-b", NOW + 1)
        .await
        .unwrap();
    let cert_id = CertId::from_domains(&["app.clientco.com".to_owned()]);
    fs.save_cert(
        &cert_id,
        &autumn_web::acme::store::StoredCert {
            chain_pem: RENEWED_CERT_PEM.to_owned(),
            key_pem: RENEWED_KEY_PEM.to_owned(),
        },
    )
    .await
    .unwrap();
    assert!(
        registry
            .record_active_for("app.clientco.com", "tenant-b", NOW + 2, NOW + 90 * 86_400)
            .await
            .unwrap()
    );

    // The stale order now resumes and loses the activation race.
    certs.release();
    tick.await.unwrap();

    let stored = fs
        .load_cert(&cert_id)
        .await
        .unwrap()
        .expect("the successor's certificate must survive the stale order's cleanup");
    assert_eq!(
        stored.chain_pem, RENEWED_CERT_PEM,
        "and must still be the successor's own pair, not the stale order's"
    );
    let record = registry.get("app.clientco.com").unwrap();
    assert_eq!(record.tenant, "tenant-b");
    assert_eq!(record.status, DomainStatus::Active);
}

/// A certificate store that refuses to delete, so a test can assert what
/// offboarding does when the key cannot be removed.
#[derive(Debug)]
struct UndeletableCertStore(Arc<FsAcmeStore>);

impl autumn_web::acme::store::AcmeStore for UndeletableCertStore {
    fn load_account(
        &self,
    ) -> autumn_web::acme::store::StoreFuture<'_, std::io::Result<Option<Vec<u8>>>> {
        self.0.load_account()
    }

    fn save_account<'a>(
        &'a self,
        data: &'a [u8],
    ) -> autumn_web::acme::store::StoreFuture<'a, std::io::Result<()>> {
        self.0.save_account(data)
    }

    fn load_cert<'a>(
        &'a self,
        id: &'a CertId,
    ) -> autumn_web::acme::store::StoreFuture<
        'a,
        std::io::Result<Option<autumn_web::acme::store::StoredCert>>,
    > {
        self.0.load_cert(id)
    }

    fn save_cert<'a>(
        &'a self,
        id: &'a CertId,
        cert: &'a autumn_web::acme::store::StoredCert,
    ) -> autumn_web::acme::store::StoreFuture<'a, std::io::Result<()>> {
        self.0.save_cert(id, cert)
    }

    fn delete_cert<'a>(
        &'a self,
        _id: &'a CertId,
    ) -> autumn_web::acme::store::StoreFuture<'a, std::io::Result<()>> {
        Box::pin(async move { Err(std::io::Error::other("the certificate store is read-only")) })
    }
}

#[tokio::test]
async fn an_offboard_that_cannot_delete_the_certificate_alerts_the_operator() {
    // Offboarding still succeeds — the domain is already unroutable, and
    // reporting failure would suggest none of it happened — but an offboarded
    // tenant's private key left on disk must not be silent. Nothing else
    // retries it: the orphan prune runs only when a `[retention]
    // custom_domains` window is configured, and it is unset by default.
    let issuer = ScriptedIssuer::new(&[]);
    let mut h = harness(
        TableVerifier::new(&[("app.clientco.com", points_here())]),
        Arc::clone(&issuer) as Arc<dyn DomainIssuer>,
    );
    h.task.certs = Arc::new(UndeletableCertStore(Arc::clone(&h.store)))
        as Arc<dyn autumn_web::acme::store::AcmeStore>;
    h.registry
        .register("app.clientco.com", "tenant-a", NOW)
        .await
        .unwrap();

    assert!(
        h.task.offboard("app.clientco.com").await.unwrap(),
        "the routing removal still succeeds"
    );
    assert!(h.registry.get("app.clientco.com").is_none());
    let alerts = h.alerts.lock().unwrap().clone();
    assert_eq!(alerts.len(), 1, "{alerts:?}");
    assert!(alerts[0].contains("app.clientco.com"), "{alerts:?}");
    assert!(alerts[0].contains("private key"), "{alerts:?}");
}

/// A coordinator that holds `try_acquire` until released, so a test can change
/// the registry while an order is waiting for its lease.
#[derive(Debug)]
struct SlowCoordinator {
    inner: autumn_web::scheduler::InProcessSchedulerCoordinator,
    gate: Mutex<Option<futures::channel::oneshot::Receiver<()>>>,
    release: Mutex<Option<futures::channel::oneshot::Sender<()>>>,
}

impl SlowCoordinator {
    fn new() -> Self {
        let (tx, rx) = futures::channel::oneshot::channel();
        Self {
            inner: autumn_web::scheduler::InProcessSchedulerCoordinator::new("test-replica"),
            gate: Mutex::new(Some(rx)),
            release: Mutex::new(Some(tx)),
        }
    }

    fn release(&self) {
        let sender = self.release.lock().unwrap().take();
        if let Some(tx) = sender {
            let _ = tx.send(());
        }
    }
}

impl autumn_web::scheduler::SchedulerCoordinator for SlowCoordinator {
    fn backend(&self) -> &'static str {
        "in_process"
    }

    fn replica_id(&self) -> &str {
        autumn_web::scheduler::SchedulerCoordinator::replica_id(&self.inner)
    }

    fn try_acquire<'a>(
        &'a self,
        task_name: &'a str,
        tick_key: &'a str,
        coordination: autumn_web::task::TaskCoordination,
    ) -> autumn_web::scheduler::SchedulerFuture<
        'a,
        autumn_web::AutumnResult<Option<autumn_web::scheduler::SchedulerLease>>,
    > {
        let held = self.gate.lock().unwrap().take();
        Box::pin(async move {
            if let Some(rx) = held {
                let _ = rx.await;
            }
            self.inner
                .try_acquire(task_name, tick_key, coordination)
                .await
        })
    }
}

#[tokio::test]
async fn an_order_is_abandoned_when_the_hostname_stops_being_this_tenants() {
    // Acquiring the lease is an await, and the registry moves underneath it.
    // Ordering anyway spends a slot of the deployment's budget and the CA's
    // rate limit on a certificate `install` would only discard — and, for a
    // hostname re-registered by someone else, moves THAT tenant's record to
    // `issuing` for an order that was never theirs.
    let dir = tempfile::tempdir().unwrap();
    let certs = Arc::new(FsAcmeStore::new(dir.path(), "staging"));
    let registry = Arc::new(CustomDomainRegistry::new(
        Arc::new(MemoryCustomDomainStore::new()),
        100,
    ));
    registry.load().await.unwrap();
    let issuer = ScriptedIssuer::new(&[]);
    let coordinator = Arc::new(SlowCoordinator::new());
    let task = Arc::new(CustomDomainTask {
        registry: Arc::clone(&registry),
        cache: Arc::new(CustomDomainCertCache::new(8)),
        certs: Arc::clone(&certs) as Arc<dyn autumn_web::acme::store::AcmeStore>,
        provider: autumn_web::tls::crypto_provider(),
        verifier: TableVerifier::new(&[("app.clientco.com", points_here())])
            as Arc<dyn DomainVerifier>,
        issuer: Arc::clone(&issuer) as Arc<dyn DomainIssuer>,
        limiter: Arc::new(IssuanceLimiter::new(5, 50, 300, 86_400)),
        ingress: ExpectedIngress {
            hostname: Some("ingress.myapp.com".to_owned()),
            ipv4: vec!["203.0.113.10".parse().unwrap()],
            ipv6: vec![],
        },
        renew_before_days: 30,
        reporter: Arc::new(|_| {}),
        recovery: None,
        coordinator: Arc::clone(&coordinator)
            as Arc<dyn autumn_web::scheduler::SchedulerCoordinator>,
        leadership_degraded: false,
        cert_store_paths: Some(Arc::clone(&certs)),
        retained_cert_ids: HashSet::new(),
    });
    registry
        .register("app.clientco.com", "tenant-a", NOW)
        .await
        .unwrap();

    let tick = tokio::spawn({
        let task = Arc::clone(&task);
        async move { task.tick(NOW).await }
    });
    // Let the tick verify the domain and reach the held lease.
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    // The tenant offboards and another one connects the same hostname.
    assert!(registry.remove("app.clientco.com").await.unwrap());
    registry
        .register("app.clientco.com", "tenant-b", NOW + 1)
        .await
        .unwrap();
    coordinator.release();
    tick.await.unwrap();

    assert_eq!(
        issuer.count(),
        0,
        "the order belonged to a tenant that no longer owns the hostname"
    );
    let record = registry.get("app.clientco.com").unwrap();
    assert_eq!(record.tenant, "tenant-b");
    assert_eq!(
        record.status,
        DomainStatus::PendingDns,
        "the new tenant's record must not be dragged into someone else's order"
    );
}

/// A registry store whose next `delete` blocks until released, so a test can
/// hold the retention sweep inside one offboard and change the registry
/// underneath it.
#[derive(Debug)]
struct PausingDeleteStore {
    inner: MemoryCustomDomainStore,
    gate: Mutex<Option<futures::channel::oneshot::Receiver<()>>>,
    release: Mutex<Option<futures::channel::oneshot::Sender<()>>>,
}

impl PausingDeleteStore {
    fn new() -> Self {
        let (tx, rx) = futures::channel::oneshot::channel();
        Self {
            inner: MemoryCustomDomainStore::new(),
            gate: Mutex::new(Some(rx)),
            release: Mutex::new(Some(tx)),
        }
    }

    fn release(&self) {
        let sender = self.release.lock().unwrap().take();
        if let Some(tx) = sender {
            let _ = tx.send(());
        }
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
        let held = self.gate.lock().unwrap().take();
        Box::pin(async move {
            if let Some(rx) = held {
                let _ = rx.await;
            }
            self.inner.delete(hostname).await
        })
    }
}

#[tokio::test]
async fn a_retention_sweep_leaves_a_domain_that_came_up_while_it_ran() {
    use autumn_web::custom_domain::CustomDomainPruner as _;

    // The sweep picks its candidates from a `list()` snapshot and then awaits
    // an offboard per candidate. The orchestrator ticks concurrently, so a
    // domain that was `pending_dns` when the snapshot was taken can be serving
    // by the time its turn comes — and deleting it then disconnects a tenant
    // seconds after their domain came up, taking the certificate with it.
    let dir = tempfile::tempdir().unwrap();
    let certs = Arc::new(FsAcmeStore::new(dir.path(), "staging"));
    let store = Arc::new(PausingDeleteStore::new());
    let registry = Arc::new(CustomDomainRegistry::new(
        Arc::clone(&store) as Arc<dyn autumn_web::custom_domain::CustomDomainStore>,
        100,
    ));
    registry.load().await.unwrap();
    let task = Arc::new(task_over(
        Arc::clone(&registry),
        Arc::new(CustomDomainCertCache::new(8)),
        Arc::clone(&certs),
        TableVerifier::new(&[]) as Arc<dyn DomainVerifier>,
        ScriptedIssuer::new(&[]) as Arc<dyn DomainIssuer>,
    ));

    // Both are abandoned when the sweep starts; `list()` sorts by hostname, so
    // `a-` is offboarded first and holds the sweep inside its store delete.
    for host in ["a-abandoned.clientco.com", "b-latecomer.clientco.com"] {
        registry.register(host, "tenant-a", NOW).await.unwrap();
    }
    let cert_id = CertId::from_domains(&["b-latecomer.clientco.com".to_owned()]);
    certs
        .save_cert(
            &cert_id,
            &autumn_web::acme::store::StoredCert {
                chain_pem: CERT_PEM.to_owned(),
                key_pem: KEY_PEM.to_owned(),
            },
        )
        .await
        .unwrap();

    let sweep = tokio::spawn({
        let task = Arc::clone(&task);
        async move { task.prune(NOW + 86_400, false).await.unwrap() }
    });
    // Let the sweep reach the held delete for the first candidate.
    for _ in 0..8 {
        tokio::task::yield_now().await;
    }
    // The second domain finishes setup while the sweep is suspended.
    registry
        .record_active("b-latecomer.clientco.com", NOW + 10, NOW + 90 * 86_400)
        .await
        .unwrap();
    store.release();

    assert_eq!(
        sweep.await.unwrap(),
        1,
        "only the still-abandoned domain may be counted"
    );
    assert!(
        registry.get("a-abandoned.clientco.com").is_none(),
        "the abandoned registration is still pruned"
    );
    let survivor = registry
        .get("b-latecomer.clientco.com")
        .expect("a domain that came up mid-sweep must survive it");
    assert_eq!(survivor.status, DomainStatus::Active);
    assert!(
        certs.load_cert(&cert_id).await.unwrap().is_some(),
        "and must keep the certificate it just installed"
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

/// An issuer that hands the hostname to another tenant and then fails, so the
/// error belongs to a tenant that no longer owns the domain.
#[derive(Debug)]
struct FailingTakeoverIssuer {
    registry: Arc<CustomDomainRegistry>,
}

impl DomainIssuer for FailingTakeoverIssuer {
    fn issue<'a>(&'a self, hostname: &'a str) -> BoxFuture<'a, Result<IssuedCertificate, String>> {
        Box::pin(async move {
            self.registry.remove(hostname).await.unwrap();
            self.registry
                .register(hostname, "tenant-b", NOW)
                .await
                .unwrap();
            Err("the CA rejected the order".to_owned())
        })
    }
}

#[tokio::test]
async fn a_failed_order_for_a_hostname_that_changed_hands_spares_the_new_tenant() {
    // The mirror of the discard case: the order fails after the takeover.
    // Charging the failure to whatever record holds the hostname would give
    // tenant B tenant A's error message and a backoff it never earned.
    let dir = tempfile::tempdir().unwrap();
    let certs = Arc::new(FsAcmeStore::new(dir.path(), "staging"));
    let registry = Arc::new(CustomDomainRegistry::new(
        Arc::new(MemoryCustomDomainStore::new()),
        10,
    ));
    registry.load().await.unwrap();
    let cache = Arc::new(CustomDomainCertCache::new(4));
    let alerts: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&alerts);
    let mut task = task_over(
        Arc::clone(&registry),
        cache,
        certs,
        TableVerifier::new(&[("app.clientco.com", points_here())]),
        Arc::new(FailingTakeoverIssuer {
            registry: Arc::clone(&registry),
        }) as Arc<dyn DomainIssuer>,
    );
    task.reporter = Arc::new(move |message: String| sink.lock().unwrap().push(message));

    registry
        .register("app.clientco.com", "tenant-a", NOW)
        .await
        .unwrap();
    task.tick(NOW).await;

    let record = registry.get("app.clientco.com").unwrap();
    assert_eq!(record.tenant, "tenant-b");
    assert_eq!(record.status, DomainStatus::PendingDns);
    assert!(
        record.failure_reason.is_none(),
        "the new tenant must not show the previous owner's error: {record:?}"
    );
    assert_eq!(record.consecutive_failures, 0);
    assert!(
        record.next_attempt_unix.is_none(),
        "the new tenant must not wait out a backoff it did not earn"
    );
    assert!(
        alerts.lock().unwrap().is_empty(),
        "a failure nobody owns must not page an operator"
    );
}

#[tokio::test]
async fn a_failure_is_recorded_only_for_the_tenant_that_still_owns_the_hostname() {
    let registry = CustomDomainRegistry::new(Arc::new(MemoryCustomDomainStore::new()), 10);
    registry.load().await.unwrap();
    registry
        .register("app.clientco.com", "tenant-a", NOW)
        .await
        .unwrap();
    registry.remove("app.clientco.com").await.unwrap();
    registry
        .register("app.clientco.com", "tenant-b", NOW)
        .await
        .unwrap();

    assert!(
        !registry
            .record_failure_for("app.clientco.com", "tenant-a", NOW, "stale", 300)
            .await
            .unwrap()
    );
    assert!(
        registry
            .get("app.clientco.com")
            .unwrap()
            .failure_reason
            .is_none()
    );

    assert!(
        registry
            .record_failure_for("app.clientco.com", "tenant-b", NOW, "mine", 300)
            .await
            .unwrap()
    );
    let record = registry.get("app.clientco.com").unwrap();
    assert_eq!(record.failure_reason.as_deref(), Some("mine"));
    assert_eq!(record.next_attempt_unix, Some(NOW + 300));
}
