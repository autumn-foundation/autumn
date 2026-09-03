//! End-to-end coverage for wildcard certificates over DNS-01 (issue #1620).
//!
//! [`acme_end_to_end`](super::acme_end_to_end) proves the HTTP-01 path; this
//! module proves the wildcard one, against the same
//! [fake CA](super::acme_fake_ca) — now deriving the account key's RFC 7638
//! thumbprint from the client's own JWS and demanding the exact
//! `base64url(sha256("{token}.{thumbprint}"))` TXT value RFC 8555 §8.4 requires,
//! **visible** in a zone that models propagation delay.
//!
//! What only an end-to-end run can evidence, and what each test pins:
//!
//! - a fresh app obtains a `*.myapp.test` certificate on first boot and serves
//!   valid HTTPS for the apex **and** for a subdomain that did not exist at
//!   issuance time — verified by a real TLS handshake whose client performs full
//!   RFC 6125 name validation against the CA root, not a permissive stub;
//! - onboarding tenant N+1 costs **zero** certificate work: no order, no
//!   issuance, no restart, no config change;
//! - the apex and the wildcard publish two different values at the SAME
//!   `_acme-challenge` name, and both survive until validation;
//! - the CA is told to validate only AFTER every record has propagated;
//! - challenge records are cleaned up after success **and** after failure;
//! - a propagation timeout names the exact record, and lands in the health
//!   indicator and the operator-alert seam;
//! - a provider credential failure never puts the token in any output.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use autumn_web::acme::challenge::Http01Tokens;
use autumn_web::acme::dns::resolver::UdpTxtLookup;
use autumn_web::acme::dns::{DnsProvider, TxtRecord};
use autumn_web::acme::renewal::{
    AcmeHealthIndicator, AcmeRenewalTask, AcmeStatus, DnsChallenge, ReporterFn,
    self_signed_placeholder,
};
use autumn_web::acme::store::{AcmeStore, CertId, FsAcmeStore, StoredCert};
use autumn_web::actuator::HealthStatus;
use autumn_web::config::{AcmeConfig, AcmeDirectory, AcmeDnsConfig, AcmeDnsProvider};
use autumn_web::scheduler::{InProcessSchedulerCoordinator, SchedulerCoordinator};
use autumn_web::tls::{
    ReloadableCertResolver, TlsListener, build_server_config, certified_key_from_pem,
    crypto_provider, leaf_not_after_from_pem,
};
use axum::Router;
use axum::routing::get;
use futures::future::BoxFuture;
use rustls_pki_types::pem::PemObject as _;
use rustls_pki_types::{CertificateDer, ServerName};
use tokio_util::sync::CancellationToken;

use super::acme_fake_ca::{self, FakeCa, TxtZoneView};

/// The base domain the wildcard covers. A `.test` name so nothing here can
/// accidentally resolve against real DNS.
const BASE_DOMAIN: &str = "myapp.test";
/// A tenant that does not exist when the certificate is issued — the whole point
/// of the wildcard.
const LATE_TENANT: &str = "tenant42.myapp.test";
const DAY: i64 = 86_400;

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn now_unix() -> i64 {
    i64::try_from(
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("clock after epoch")
            .as_secs(),
    )
    .expect("unix time fits i64")
}

// ── A DNS zone that models propagation ───────────────────────────────────────

/// One published TXT value and the instant it becomes visible to the world.
struct ZoneValue {
    value: String,
    visible_at: Instant,
}

/// An in-process DNS zone: what the provider writes, what resolvers (and the CA)
/// can see, and how long the gap between the two is.
///
/// The propagation delay is what makes "wait before signalling ready" a tested
/// behaviour: with a delay set, an implementation that told the CA to validate
/// immediately would fail here exactly as it fails against Let's Encrypt.
pub struct FakeZone {
    records: Mutex<HashMap<String, Vec<ZoneValue>>>,
    propagation_delay: Duration,
}

impl FakeZone {
    fn new(propagation_delay: Duration) -> Arc<Self> {
        Arc::new(Self {
            records: Mutex::new(HashMap::new()),
            propagation_delay,
        })
    }

    fn upsert(&self, fqdn: &str, value: &str) {
        let mut records = lock(&self.records);
        let entries = records.entry(fqdn.to_ascii_lowercase()).or_default();
        if entries.iter().any(|e| e.value == value) {
            return;
        }
        entries.push(ZoneValue {
            value: value.to_owned(),
            visible_at: Instant::now() + self.propagation_delay,
        });
    }

    fn delete(&self, fqdn: &str, value: &str) {
        let mut records = lock(&self.records);
        if let Some(entries) = records.get_mut(&fqdn.to_ascii_lowercase()) {
            entries.retain(|e| e.value != value);
        }
    }

    /// Every value written, propagated or not — what the *provider* believes it
    /// has published.
    fn written(&self, fqdn: &str) -> Vec<String> {
        lock(&self.records)
            .get(&fqdn.to_ascii_lowercase())
            .map(|entries| entries.iter().map(|e| e.value.clone()).collect())
            .unwrap_or_default()
    }
}

impl TxtZoneView for FakeZone {
    fn visible_txt(&self, fqdn: &str) -> Vec<String> {
        let now = Instant::now();
        lock(&self.records)
            .get(&fqdn.to_ascii_lowercase())
            .map(|entries| {
                entries
                    .iter()
                    .filter(|e| e.visible_at <= now)
                    .map(|e| e.value.clone())
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// Serve `zone` over UDP as a DNS server, so the propagation wait exercises the
/// real wire path rather than an in-memory shortcut. Returns the bound address.
async fn serve_zone(zone: Arc<FakeZone>, shutdown: CancellationToken) -> SocketAddr {
    let socket = tokio::net::UdpSocket::bind("127.0.0.1:0")
        .await
        .expect("bind fake DNS server");
    let addr = socket.local_addr().expect("fake DNS local_addr");
    tokio::spawn(async move {
        let mut buf = vec![0_u8; 512];
        loop {
            let received = tokio::select! {
                r = socket.recv_from(&mut buf) => r,
                () = shutdown.cancelled() => break,
            };
            let Ok((read, peer)) = received else { break };
            let request = &buf[..read];
            if read < 12 {
                continue;
            }
            let id = u16::from_be_bytes([request[0], request[1]]);
            let Some(name) = question_name(request) else {
                continue;
            };
            let values = zone.visible_txt(&name);
            let response = txt_response(id, request, &values);
            let _ = socket.send_to(&response, peer).await;
        }
    });
    addr
}

/// The QNAME of a DNS query, as dotted labels.
fn question_name(msg: &[u8]) -> Option<String> {
    let mut offset = 12;
    let mut labels = Vec::new();
    loop {
        let len = usize::from(*msg.get(offset)?);
        if len == 0 {
            break;
        }
        let end = offset.checked_add(1 + len)?;
        labels.push(String::from_utf8_lossy(msg.get(offset + 1..end)?).into_owned());
        offset = end;
    }
    Some(labels.join("."))
}

/// A minimal TXT answer echoing the query's question section, with each value as
/// one answer record whose owner name is a compression pointer — the shape a
/// real resolver sends.
fn txt_response(id: u16, request: &[u8], values: &[String]) -> Vec<u8> {
    // Question section runs from byte 12 to the root label plus QTYPE/QCLASS.
    let mut question_end = 12;
    while let Some(&len) = request.get(question_end) {
        question_end += 1 + usize::from(len);
        if len == 0 {
            break;
        }
    }
    question_end += 4;
    let mut msg = request[..question_end.min(request.len())].to_vec();
    msg[2..4].copy_from_slice(&0x8180_u16.to_be_bytes()); // QR + RD + RA, NOERROR
    msg[0..2].copy_from_slice(&id.to_be_bytes());
    msg[6..8].copy_from_slice(&u16::try_from(values.len()).unwrap_or(0).to_be_bytes());
    for value in values {
        msg.extend_from_slice(&[0xC0, 12]); // pointer to the question's name
        msg.extend_from_slice(&16_u16.to_be_bytes()); // TXT
        msg.extend_from_slice(&1_u16.to_be_bytes()); // IN
        msg.extend_from_slice(&60_u32.to_be_bytes());
        let bytes = value.as_bytes();
        msg.extend_from_slice(&u16::try_from(bytes.len() + 1).unwrap_or(0).to_be_bytes());
        msg.push(u8::try_from(bytes.len()).unwrap_or(0));
        msg.extend_from_slice(bytes);
    }
    msg
}

// ── A DNS provider over the fake zone ────────────────────────────────────────

/// A [`DnsProvider`] writing into a [`FakeZone`], with the knobs the failure
/// paths need.
struct ZoneProvider {
    zone: Arc<FakeZone>,
    upserts: AtomicUsize,
    deletes: AtomicUsize,
    /// When set, `upsert_txt` fails — an invalid or expired provider token.
    fail_with: Mutex<Option<String>>,
}

impl ZoneProvider {
    fn new(zone: Arc<FakeZone>) -> Arc<Self> {
        Arc::new(Self {
            zone,
            upserts: AtomicUsize::new(0),
            deletes: AtomicUsize::new(0),
            fail_with: Mutex::new(None),
        })
    }
}

impl DnsProvider for ZoneProvider {
    fn name(&self) -> &'static str {
        "test-zone"
    }

    fn upsert_txt<'a>(&'a self, record: &'a TxtRecord) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.upserts.fetch_add(1, Ordering::SeqCst);
            if let Some(message) = lock(&self.fail_with).clone() {
                return Err(message);
            }
            self.zone.upsert(&record.fqdn, &record.value);
            Ok(())
        })
    }

    fn delete_txt<'a>(&'a self, record: &'a TxtRecord) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            self.deletes.fetch_add(1, Ordering::SeqCst);
            self.zone.delete(&record.fqdn, &record.value);
            Ok(())
        })
    }
}

// ── Harness ──────────────────────────────────────────────────────────────────

/// A running app-side wildcard-ACME deployment.
struct WildcardApp {
    https_addr: SocketAddr,
    challenge_addr: SocketAddr,
    resolver: Arc<ReloadableCertResolver>,
    tokens: Http01Tokens,
    cache_dir: tempfile::TempDir,
    shutdown: CancellationToken,
}

impl Drop for WildcardApp {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

/// Boot the app side: an HTTPS listener seeded with `initial` (the placeholder),
/// plus the `:80` listener the DNS-01 path still binds for HTTP→HTTPS redirects.
async fn boot_app(initial: &StoredCert) -> WildcardApp {
    let provider = crypto_provider();
    let certified = certified_key_from_pem(
        initial.chain_pem.as_bytes(),
        initial.key_pem.as_bytes(),
        &provider,
    )
    .expect("initial cert loads");
    let resolver = Arc::new(ReloadableCertResolver::new(certified));
    let server_config =
        build_server_config(Arc::clone(&provider), Arc::clone(&resolver)).expect("server config");

    let shutdown = CancellationToken::new();
    let https_tcp = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind https");
    let https_addr = https_tcp.local_addr().expect("https local_addr");
    let listener = TlsListener::new(
        https_tcp,
        server_config,
        Duration::from_secs(10),
        shutdown.child_token(),
    );
    let app_router = Router::new().route("/health", get(|| async { "ok" }));
    let serve_shutdown = shutdown.clone();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app_router)
            .with_graceful_shutdown(async move { serve_shutdown.cancelled().await })
            .await;
    });

    let tokens = Http01Tokens::new();
    let challenge_tcp = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind challenge");
    let challenge_addr = challenge_tcp.local_addr().expect("challenge local_addr");
    let challenge = autumn_web::acme::challenge::challenge_router(tokens.clone(), https_addr.port());
    let serve_shutdown = shutdown.clone();
    tokio::spawn(async move {
        let _ = axum::serve(challenge_tcp, challenge)
            .with_graceful_shutdown(async move { serve_shutdown.cancelled().await })
            .await;
    });

    tokio::time::sleep(Duration::from_millis(50)).await;

    WildcardApp {
        https_addr,
        challenge_addr,
        resolver,
        tokens,
        cache_dir: tempfile::tempdir().expect("cache dir"),
        shutdown,
    }
}

/// The wildcard `[server.tls.acme]` config: the apex plus `*.myapp.test`.
fn wildcard_config(app: &WildcardApp, ca: &FakeCa, ca_root: std::path::PathBuf) -> AcmeConfig {
    AcmeConfig {
        domains: vec![BASE_DOMAIN.to_owned(), format!("*.{BASE_DOMAIN}")],
        contact_email: "ops@myapp.test".to_owned(),
        directory: AcmeDirectory::Custom {
            url: ca.directory_url.clone(),
        },
        cache_dir: app.cache_dir.path().to_path_buf(),
        http_challenge_port: app.challenge_addr.port(),
        renew_before_days: 30,
        ca_root_path: Some(ca_root),
        dns: Some(AcmeDnsConfig {
            provider: AcmeDnsProvider::Exec,
            credential: "acme_dns".to_owned(),
            propagation_timeout_secs: 30,
            poll_interval_secs: 1,
            // The live probe address is injected into `DnsChallenge` below; this
            // list only has to be a valid one, so the config as a whole would
            // pass `AcmeConfig::validate()`.
            resolvers: vec!["127.0.0.1:53".to_owned()],
            command: vec!["/bin/true".to_owned()],
        }),
    }
}

fn write_ca_root(app: &WildcardApp, ca: &FakeCa) -> std::path::PathBuf {
    let path = app.cache_dir.path().join("test-ca-root.pem");
    std::fs::write(&path, &ca.root_pem).expect("write CA root");
    path
}

fn fs_store(config: &AcmeConfig) -> Arc<dyn AcmeStore> {
    Arc::new(FsAcmeStore::new(
        config.cache_dir.clone(),
        autumn_web::acme::directory_label(&config.directory),
    ))
}

/// Build the renewal task over `app`'s live resolver, answering DNS-01 through
/// `provider` and confirming propagation against the zone served at `resolver`.
#[allow(clippy::too_many_arguments)]
fn renewal_task(
    app: &WildcardApp,
    config: AcmeConfig,
    store: Arc<dyn AcmeStore>,
    status: AcmeStatus,
    dns_provider: Arc<dyn DnsProvider>,
    dns_addr: SocketAddr,
    serving_stored_cert: bool,
    recovery: Option<autumn_web::acme::renewal::RecoveryFn>,
) -> AcmeRenewalTask {
    let propagation_timeout =
        Duration::from_secs(config.dns.as_ref().expect("dns configured").propagation_timeout_secs);
    AcmeRenewalTask {
        resolver: Arc::clone(&app.resolver),
        provider: crypto_provider(),
        store,
        cert_id: CertId::from_domains(&config.domains),
        tokens: app.tokens.clone(),
        status,
        config,
        serving_stored_cert,
        leadership_degraded: false,
        renew_window_misconfigured: AtomicBool::new(false),
        dns: Some(DnsChallenge {
            provider: dns_provider,
            lookup: Arc::new(UdpTxtLookup::new(Duration::from_secs(2))),
            resolvers: vec![dns_addr],
            propagation_timeout,
            poll_interval: Duration::from_millis(100),
        }),
        recovery,
    }
}

fn recording_reporter() -> (ReporterFn, Arc<Mutex<Vec<String>>>) {
    let seen = Arc::new(Mutex::new(Vec::new()));
    let sink = Arc::clone(&seen);
    let reporter: ReporterFn = Arc::new(move |msg: String| lock(&sink).push(msg));
    (reporter, seen)
}

/// Run one boot of the renewal task to quiescence.
async fn run_one_boot(task: AcmeRenewalTask, status: &AcmeStatus, reporter: ReporterFn) {
    let coordinator: Arc<dyn SchedulerCoordinator> =
        Arc::new(InProcessSchedulerCoordinator::new("test-replica"));
    let shutdown = CancellationToken::new();
    let handle = tokio::spawn(task.run(coordinator, reporter, shutdown.clone()));

    let deadline = Instant::now() + Duration::from_secs(60);
    loop {
        let snap = status.snapshot();
        if snap.last_success_unix.is_some() || snap.last_failure.is_some() {
            break;
        }
        if handle.is_finished() {
            match handle.await {
                Ok(()) => panic!("the ACME renewal task ended before recording any outcome"),
                Err(e) => panic!("the ACME renewal task panicked: {e}"),
            }
        }
        assert!(Instant::now() < deadline, "ACME boot attempt never settled");
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    shutdown.cancel();
    match tokio::time::timeout(Duration::from_secs(5), handle).await {
        Ok(Ok(())) | Err(_) => {}
        Ok(Err(e)) => panic!("the ACME renewal task panicked: {e}"),
    }
}

// ── A name-verifying TLS client ──────────────────────────────────────────────

/// Complete a real TLS handshake to `addr` with SNI `server_name`, performing
/// FULL RFC 6125 name validation against `ca_root_pem`.
///
/// This is the evidence for "every tenant's subdomain serves valid HTTPS": a
/// permissive verifier would pass whatever the server sent, so the client here
/// is the same webpki path a browser uses — a certificate that did not cover
/// `server_name` fails the handshake.
async fn https_get_verified(
    addr: SocketAddr,
    server_name: &str,
    ca_root_pem: &str,
) -> Result<String, String> {
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let mut roots = rustls::RootCertStore::empty();
    for cert in CertificateDer::pem_slice_iter(ca_root_pem.as_bytes()) {
        roots
            .add(cert.map_err(|e| format!("bad CA root PEM: {e}"))?)
            .map_err(|e| format!("could not trust the CA root: {e}"))?;
    }
    let config = rustls::ClientConfig::builder_with_provider(crypto_provider())
        .with_safe_default_protocol_versions()
        .map_err(|e| e.to_string())?
        .with_root_certificates(roots)
        .with_no_client_auth();
    let connector = tokio_rustls::TlsConnector::from(Arc::new(config));
    let name = ServerName::try_from(server_name.to_owned())
        .map_err(|e| format!("invalid server name {server_name}: {e}"))?;

    let tcp = tokio::net::TcpStream::connect(addr)
        .await
        .map_err(|e| format!("connect: {e}"))?;
    let mut stream = connector
        .connect(name, tcp)
        .await
        .map_err(|e| format!("TLS handshake for {server_name} failed: {e}"))?;
    stream
        .write_all(
            format!("GET /health HTTP/1.1\r\nHost: {server_name}\r\nConnection: close\r\n\r\n")
                .as_bytes(),
        )
        .await
        .map_err(|e| format!("write: {e}"))?;
    let mut body = Vec::new();
    stream
        .read_to_end(&mut body)
        .await
        .map_err(|e| format!("read: {e}"))?;
    Ok(String::from_utf8_lossy(&body).into_owned())
}

/// The SAN entries on the certificate the resolver is currently serving.
fn served_sans(resolver: &ReloadableCertResolver) -> Vec<String> {
    use x509_parser::prelude::FromDer as _;
    let certified = resolver.current();
    let leaf = certified.cert.first().expect("a leaf is served");
    let (_, parsed) = x509_parser::certificate::X509Certificate::from_der(leaf)
        .expect("the served leaf parses");
    parsed
        .subject_alternative_name()
        .ok()
        .flatten()
        .map(|san| {
            san.value
                .general_names
                .iter()
                .filter_map(|name| match name {
                    x509_parser::extensions::GeneralName::DNSName(dns) => Some((*dns).to_owned()),
                    _ => None,
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Stand up the whole rig: zone, DNS server, CA, app, config, store, status.
struct Rig {
    ca: FakeCa,
    app: WildcardApp,
    zone: Arc<FakeZone>,
    provider: Arc<ZoneProvider>,
    dns_addr: SocketAddr,
    config: AcmeConfig,
    store: Arc<dyn AcmeStore>,
    status: AcmeStatus,
    shutdown: CancellationToken,
}

impl Drop for Rig {
    fn drop(&mut self) {
        self.shutdown.cancel();
    }
}

async fn build_rig(propagation_delay: Duration) -> Rig {
    let placeholder = self_signed_placeholder(&[BASE_DOMAIN.to_owned()]).expect("placeholder");
    let app = boot_app(&placeholder).await;
    let ca = acme_fake_ca::start(app.challenge_addr).await;
    let zone = FakeZone::new(propagation_delay);
    ca.state.set_dns_zone(Arc::clone(&zone) as Arc<dyn TxtZoneView>);
    let shutdown = CancellationToken::new();
    let dns_addr = serve_zone(Arc::clone(&zone), shutdown.child_token()).await;
    let ca_root = write_ca_root(&app, &ca);
    let config = wildcard_config(&app, &ca, ca_root);
    let store = fs_store(&config);
    Rig {
        provider: ZoneProvider::new(Arc::clone(&zone)),
        ca,
        app,
        zone,
        dns_addr,
        config,
        store,
        status: AcmeStatus::new(),
        shutdown,
    }
}

impl Rig {
    fn task(&self, serving_stored_cert: bool) -> AcmeRenewalTask {
        renewal_task(
            &self.app,
            self.config.clone(),
            Arc::clone(&self.store),
            self.status.clone(),
            Arc::clone(&self.provider) as Arc<dyn DnsProvider>,
            self.dns_addr,
            serving_stored_cert,
            None,
        )
    }

    fn challenge_fqdn(&self) -> String {
        format!("_acme-challenge.{BASE_DOMAIN}")
    }
}

// ── Tests ────────────────────────────────────────────────────────────────────

/// AC1 + AC4: a fresh app obtains a wildcard certificate on first startup and
/// serves valid HTTPS for the apex **and** for any subdomain — including one
/// nobody had thought of at issuance time — with no per-subdomain configuration.
///
/// The handshake client performs full name validation against the CA root, so
/// this is the browser's own verdict, not an assertion about a string.
#[tokio::test]
async fn first_boot_issues_a_wildcard_and_serves_every_tenant_subdomain() {
    let rig = build_rig(Duration::ZERO).await;
    let (reporter, reported) = recording_reporter();

    run_one_boot(rig.task(false), &rig.status, reporter).await;

    let snap = rig.status.snapshot();
    assert!(
        snap.last_failure.is_none(),
        "issuance failed: {:?}",
        snap.last_failure
    );
    assert!(snap.last_success_unix.is_some(), "no success recorded");
    assert!(lock(&reported).is_empty(), "unexpected failure reports");

    // The CA really validated DNS-01 — twice, once per authorization.
    assert_eq!(
        rig.ca.state.dns_validations_ok.load(Ordering::SeqCst),
        2,
        "both the apex and the wildcard authorization must validate over DNS-01"
    );
    assert_eq!(rig.ca.state.dns_validations_failed.load(Ordering::SeqCst), 0);

    let sans = served_sans(&rig.app.resolver);
    assert!(
        sans.contains(&format!("*.{BASE_DOMAIN}")),
        "the served certificate must carry the wildcard SAN: {sans:?}"
    );

    // The apex, and a tenant subdomain that did not exist when the certificate
    // was ordered, both serve valid HTTPS — verified, not asserted.
    for host in [BASE_DOMAIN, LATE_TENANT, "another-tenant.myapp.test"] {
        let response = https_get_verified(rig.app.https_addr, host, &rig.ca.root_pem)
            .await
            .unwrap_or_else(|e| panic!("{host} must serve valid HTTPS: {e}"));
        assert!(response.contains("200 OK"), "{host}: {response}");
    }

    // …and a name the wildcard does NOT cover is still rejected, so the test
    // above is proving name validation rather than a permissive client.
    let err = https_get_verified(rig.app.https_addr, "evil.example.com", &rig.ca.root_pem)
        .await
        .expect_err("an uncovered name must fail the handshake");
    assert!(err.contains("handshake"), "got: {err}");
}

/// AC4 + Success Metric: onboarding tenant N+1 after initial issuance requires
/// **zero** certificate-related actions — no order, no issuance, no restart, no
/// config change — and the new subdomain serves valid HTTPS immediately.
#[tokio::test]
async fn onboarding_a_tenant_after_issuance_costs_no_certificate_work() {
    let rig = build_rig(Duration::ZERO).await;
    let (reporter, _) = recording_reporter();
    run_one_boot(rig.task(false), &rig.status, reporter).await;

    let orders_after_issuance = rig.ca.state.orders_created.load(Ordering::SeqCst);
    let upserts_after_issuance = rig.provider.upserts.load(Ordering::SeqCst);
    let cert_before = rig.app.resolver.current();

    // "Provision a new tenant": nothing whatsoever happens on the certificate
    // side. The tenant simply starts resolving to this host.
    let response = https_get_verified(rig.app.https_addr, LATE_TENANT, &rig.ca.root_pem)
        .await
        .expect("the new tenant serves valid HTTPS with no certificate work");
    assert!(response.contains("200 OK"), "{response}");

    assert_eq!(
        rig.ca.state.orders_created.load(Ordering::SeqCst),
        orders_after_issuance,
        "a new tenant must not cause a new ACME order"
    );
    assert_eq!(
        rig.provider.upserts.load(Ordering::SeqCst),
        upserts_after_issuance,
        "a new tenant must not cause a DNS write"
    );
    assert!(
        Arc::ptr_eq(&cert_before, &rig.app.resolver.current()),
        "a new tenant must not swap the served certificate"
    );
}

/// R3: the apex and the wildcard authorizations publish two DIFFERENT values at
/// the SAME `_acme-challenge` name. Both must be live when the CA validates —
/// a provider that replaced the record set instead of appending would break one.
#[tokio::test]
async fn apex_and_wildcard_publish_two_values_at_one_name() {
    let rig = build_rig(Duration::from_millis(150)).await;
    let (reporter, _) = recording_reporter();
    run_one_boot(rig.task(false), &rig.status, reporter).await;

    // The CA looked for two DISTINCT values at the one name.
    let lookups = rig.ca.state.dns_lookups();
    assert_eq!(lookups.len(), 2, "two authorizations, two lookups: {lookups:?}");
    assert!(
        lookups.iter().all(|(fqdn, _)| *fqdn == rig.challenge_fqdn()),
        "both records share the base domain's challenge name: {lookups:?}"
    );
    assert_ne!(
        lookups[0].1, lookups[1].1,
        "each authorization has its own key authorization digest: {lookups:?}"
    );
    assert_eq!(
        rig.ca.state.dns_validations_ok.load(Ordering::SeqCst),
        2,
        "both values were live at validation time"
    );
}

/// AC5 / R4: the CA is told to validate only AFTER the records have propagated.
///
/// With a propagation delay longer than a naive implementation would wait, an
/// order that signalled ready eagerly would be rejected by the CA (which reads
/// the zone's *visible* view). Issuance succeeding here is the proof.
#[tokio::test]
async fn propagation_is_awaited_before_the_ca_is_told_to_validate() {
    let rig = build_rig(Duration::from_millis(600)).await;
    let (reporter, reported) = recording_reporter();
    let started = Instant::now();

    run_one_boot(rig.task(false), &rig.status, reporter).await;

    assert!(
        rig.status.snapshot().last_failure.is_none(),
        "issuance must survive a slow zone: {:?}",
        rig.status.snapshot().last_failure
    );
    assert!(lock(&reported).is_empty());
    assert_eq!(rig.ca.state.dns_validations_failed.load(Ordering::SeqCst), 0);
    assert!(
        started.elapsed() >= Duration::from_millis(600),
        "the order cannot have completed before the records became visible"
    );
}

/// R2: after a successful order the challenge records are removed, so a zone
/// does not accumulate `_acme-challenge` litter across renewals.
#[tokio::test]
async fn challenge_records_are_removed_after_a_successful_order() {
    let rig = build_rig(Duration::ZERO).await;
    let (reporter, _) = recording_reporter();
    run_one_boot(rig.task(false), &rig.status, reporter).await;

    assert!(rig.status.snapshot().last_success_unix.is_some());
    assert_eq!(
        rig.zone.written(&rig.challenge_fqdn()),
        Vec::<String>::new(),
        "every published challenge record must be cleaned up"
    );
    assert_eq!(
        rig.provider.deletes.load(Ordering::SeqCst),
        rig.provider.upserts.load(Ordering::SeqCst),
        "one cleanup per publish"
    );
}

/// R2 again, on the path that actually matters: a FAILED order must clean up
/// too, or every retry leaves another pair of records behind until the zone is
/// full of dead `_acme-challenge` entries.
#[tokio::test]
async fn challenge_records_are_removed_after_a_failed_order() {
    // A zone that accepts writes but never makes them visible, so the records
    // are genuinely published and the propagation wait genuinely times out.
    let mut rig = build_rig(Duration::from_secs(3600)).await;
    rig.config
        .dns
        .as_mut()
        .expect("dns configured")
        .propagation_timeout_secs = 1;
    let (reporter, reported) = recording_reporter();

    run_one_boot(rig.task(false), &rig.status, reporter).await;

    let failure = rig
        .status
        .snapshot()
        .last_failure
        .expect("a propagation timeout must be recorded");
    assert!(failure.1.contains("propagation"), "got: {}", failure.1);
    assert!(!lock(&reported).is_empty(), "the failure must be reported");
    assert!(
        rig.provider.upserts.load(Ordering::SeqCst) > 0,
        "the records really were published before the wait timed out"
    );
    assert_eq!(
        rig.zone.written(&rig.challenge_fqdn()),
        Vec::<String>::new(),
        "a failed order must not leave challenge records behind"
    );
    assert_eq!(
        rig.provider.deletes.load(Ordering::SeqCst),
        rig.provider.upserts.load(Ordering::SeqCst),
        "one cleanup per publish, even on the failure path"
    );
}

/// Regression: the challenge records must survive until the ORDER settles, not
/// merely until `set_ready` returns.
///
/// RFC 8555 §7.5.1: `set_ready` only tells the CA to *queue* validation; the
/// client then polls. With the CA validating lazily (as a real one does), an
/// implementation that cleaned up as soon as `set_ready` returned would pull the
/// TXT records out from under a CA that had not looked yet — and every order
/// would fail with an opaque `unauthorized`.
#[tokio::test]
async fn challenge_records_survive_until_the_order_settles() {
    let rig = build_rig(Duration::ZERO).await;
    rig.ca.state.deferred_validation.store(true, Ordering::SeqCst);
    let (reporter, reported) = recording_reporter();

    run_one_boot(rig.task(false), &rig.status, reporter).await;

    assert!(
        rig.status.snapshot().last_failure.is_none(),
        "issuance must survive a CA that validates lazily: {:?}",
        rig.status.snapshot().last_failure
    );
    assert!(lock(&reported).is_empty());
    assert_eq!(
        rig.ca.state.dns_validations_ok.load(Ordering::SeqCst),
        2,
        "both records were still published when the CA finally looked"
    );
    assert_eq!(rig.ca.state.dns_validations_failed.load(Ordering::SeqCst), 0);
    // …and they are cleaned up once the order is done.
    assert_eq!(
        rig.zone.written(&rig.challenge_fqdn()),
        Vec::<String>::new()
    );
}

/// AC5: the propagation timeout error names the exact record that failed to
/// propagate — the whole point of a bounded wait.
#[tokio::test]
async fn a_propagation_timeout_names_the_exact_record() {
    let mut rig = build_rig(Duration::from_secs(3600)).await;
    rig.config
        .dns
        .as_mut()
        .expect("dns configured")
        .propagation_timeout_secs = 1;
    let (reporter, reported) = recording_reporter();

    run_one_boot(rig.task(false), &rig.status, reporter).await;

    let (_, message) = rig
        .status
        .snapshot()
        .last_failure
        .expect("the wait must time out");
    assert!(
        message.contains(&rig.challenge_fqdn()),
        "the message must name the record: {message}"
    );
    assert!(
        message.contains("propagation_timeout_secs"),
        "the message must name the knob that extends the wait: {message}"
    );
    let reports = lock(&reported).clone();
    assert!(
        reports.iter().any(|r| r.contains(&rig.challenge_fqdn())),
        "the same detail must reach the reporter/alert seam: {reports:?}"
    );
}

/// AC6: a failed DNS-01 issuance surfaces through health output. With no
/// certificate yet, a failure is `Down` — an operator's dashboard says so
/// immediately rather than after the placeholder silently expires.
#[tokio::test]
async fn a_dns01_failure_surfaces_in_health_and_names_the_provider() {
    let rig = build_rig(Duration::ZERO).await;
    *lock(&rig.provider.fail_with) = Some(
        "could not create the Cloudflare TXT record (HTTP 403 from \
         https://api.cloudflare.com/client/v4/zones/z/dns_records): Invalid access token"
            .to_owned(),
    );
    let (reporter, reported) = recording_reporter();

    run_one_boot(rig.task(false), &rig.status, reporter).await;

    let (_, message) = rig
        .status
        .snapshot()
        .last_failure
        .expect("a provider failure must be recorded");
    assert!(message.contains("Invalid access token"), "got: {message}");
    assert!(!lock(&reported).is_empty(), "the failure must be reported");

    let indicator = AcmeHealthIndicator::new(rig.status.clone(), 30)
        .with_dns_provider(Some("cloudflare"));
    let graded = indicator.grade(now_unix());
    assert_eq!(
        graded.status,
        HealthStatus::Down,
        "a failure with no usable certificate is Down"
    );
    assert_eq!(graded.details["challenge"], serde_json::json!("dns-01"));
    assert_eq!(
        graded.details["dns_provider"],
        serde_json::json!("cloudflare")
    );
    assert!(graded.details.contains_key("last_failure"));
}

/// AC3: a provider token never reaches the failure message, the recorded status,
/// the reporter payload, or the health details.
#[tokio::test]
async fn a_provider_failure_never_leaks_the_api_token() {
    const TOKEN: &str = "cf-live-token-DO-NOT-LEAK-9f3a";

    let rig = build_rig(Duration::ZERO).await;
    // A provider that fails the way a real one does: with the API's message, not
    // with its own credentials pasted in.
    *lock(&rig.provider.fail_with) =
        Some("could not create the Cloudflare TXT record (HTTP 403): Invalid access token".to_owned());
    let (reporter, reported) = recording_reporter();

    run_one_boot(rig.task(false), &rig.status, reporter).await;

    let snapshot = rig.status.snapshot();
    let indicator = AcmeHealthIndicator::new(rig.status.clone(), 30)
        .with_dns_provider(Some("cloudflare"));
    let surfaces = [
        format!("{:?}", snapshot.last_failure),
        format!("{:?}", lock(&reported).clone()),
        serde_json::to_string(&indicator.grade(now_unix()).details).expect("details serialize"),
    ];
    for surface in &surfaces {
        assert!(
            !surface.contains(TOKEN),
            "a DNS provider token must never reach operator-visible output: {surface}"
        );
    }
}

/// AC5: wildcard certificates inherit #1608's lifecycle — a near-expiry
/// certificate is renewed with no restart and no dropped connections, and the
/// fresh one hot-swaps into the SAME live listener.
#[tokio::test]
async fn a_near_expiry_wildcard_renews_in_place() {
    let rig = build_rig(Duration::ZERO).await;
    // The CA issues a certificate that is already inside the renew-before window.
    rig.ca.state.cert_lifetime_days.store(10, Ordering::SeqCst);
    let (reporter, _) = recording_reporter();
    run_one_boot(rig.task(false), &rig.status, reporter).await;

    let first = rig.app.resolver.current();
    let first_not_after = rig
        .status
        .snapshot()
        .cert_not_after_unix
        .expect("a certificate was issued");
    assert!(
        first_not_after - now_unix() < 30 * DAY,
        "the seeded certificate must be inside the renew-before window"
    );

    // A second boot over the same store renews, because the stored leaf is due.
    rig.ca.state.cert_lifetime_days.store(90, Ordering::SeqCst);
    let (reporter, reported) = recording_reporter();
    run_one_boot(rig.task(true), &rig.status, reporter).await;

    assert!(lock(&reported).is_empty(), "renewal must not report failures");
    let renewed = rig.app.resolver.current();
    assert!(
        !Arc::ptr_eq(&first, &renewed),
        "the renewed certificate must hot-swap into the live resolver"
    );
    let renewed_not_after = rig
        .status
        .snapshot()
        .cert_not_after_unix
        .expect("renewal recorded");
    assert!(
        renewed_not_after > first_not_after,
        "the renewed leaf must live longer than the one it replaced"
    );
    // Still a wildcard, and still serving the late tenant.
    let response = https_get_verified(rig.app.https_addr, LATE_TENANT, &rig.ca.root_pem)
        .await
        .expect("the renewed wildcard still covers every tenant");
    assert!(response.contains("200 OK"), "{response}");
}

/// AC5: the wildcard certificate persists across restarts — a reboot serves the
/// stored certificate and does NOT re-order (Let's Encrypt rate limits) or
/// re-publish challenge records.
#[tokio::test]
async fn a_restart_reuses_the_stored_wildcard_certificate() {
    let rig = build_rig(Duration::ZERO).await;
    let (reporter, _) = recording_reporter();
    run_one_boot(rig.task(false), &rig.status, reporter).await;

    let orders = rig.ca.state.orders_created.load(Ordering::SeqCst);
    let accounts = rig.ca.state.accounts_registered.load(Ordering::SeqCst);
    let upserts = rig.provider.upserts.load(Ordering::SeqCst);
    assert_eq!(orders, 1, "first boot orders exactly once");

    // "Restart": a fresh status over the same store and cache dir.
    let stored = rig
        .store
        .load_cert(&CertId::from_domains(&rig.config.domains))
        .await
        .expect("store readable")
        .expect("a certificate was persisted");
    let not_after = leaf_not_after_from_pem(stored.chain_pem.as_bytes()).expect("leaf parses");
    let restarted_status = AcmeStatus::new();
    restarted_status.set_cert_not_after(not_after);
    let task = renewal_task(
        &rig.app,
        rig.config.clone(),
        Arc::clone(&rig.store),
        restarted_status.clone(),
        Arc::clone(&rig.provider) as Arc<dyn DnsProvider>,
        rig.dns_addr,
        true,
        None,
    );
    let coordinator: Arc<dyn SchedulerCoordinator> =
        Arc::new(InProcessSchedulerCoordinator::new("test-replica"));
    let shutdown = CancellationToken::new();
    let (reporter, reported) = recording_reporter();
    let handle = tokio::spawn(task.run(coordinator, reporter, shutdown.clone()));
    tokio::time::sleep(Duration::from_millis(500)).await;
    shutdown.cancel();
    let _ = tokio::time::timeout(Duration::from_secs(5), handle).await;

    assert!(lock(&reported).is_empty(), "a restart must not fail");
    assert_eq!(
        rig.ca.state.orders_created.load(Ordering::SeqCst),
        orders,
        "a restart with a healthy stored certificate must not re-order"
    );
    assert_eq!(
        rig.ca.state.accounts_registered.load(Ordering::SeqCst),
        accounts,
        "a restart must reuse the stored ACME account"
    );
    assert_eq!(
        rig.provider.upserts.load(Ordering::SeqCst),
        upserts,
        "a restart must not touch DNS"
    );
}

/// AC6: a success that follows a failure clears the operator alert, so a
/// transient DNS outage does not leave a `scheduled_task_failure` alert standing
/// forever — while a steady-state renewal does not re-notify.
#[tokio::test]
async fn a_recovered_renewal_clears_the_operator_alert() {
    let recoveries = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&recoveries);
    let recovery: autumn_web::acme::renewal::RecoveryFn =
        Arc::new(move || { counter.fetch_add(1, Ordering::SeqCst); });

    let rig = build_rig(Duration::ZERO).await;
    *lock(&rig.provider.fail_with) = Some("provider outage".to_owned());
    let (reporter, _) = recording_reporter();
    let task = renewal_task(
        &rig.app,
        rig.config.clone(),
        Arc::clone(&rig.store),
        rig.status.clone(),
        Arc::clone(&rig.provider) as Arc<dyn DnsProvider>,
        rig.dns_addr,
        false,
        Some(Arc::clone(&recovery)),
    );
    run_one_boot(task, &rig.status, reporter).await;
    assert!(rig.status.snapshot().last_failure.is_some());
    assert_eq!(
        recoveries.load(Ordering::SeqCst),
        0,
        "a failure must not fire the recovery"
    );

    // The provider comes back; the next attempt succeeds and clears the alert.
    *lock(&rig.provider.fail_with) = None;
    let (reporter, _) = recording_reporter();
    let task = renewal_task(
        &rig.app,
        rig.config.clone(),
        Arc::clone(&rig.store),
        rig.status.clone(),
        Arc::clone(&rig.provider) as Arc<dyn DnsProvider>,
        rig.dns_addr,
        false,
        Some(recovery),
    );
    run_one_boot(task, &rig.status, reporter).await;
    assert!(rig.status.snapshot().last_failure.is_none());
    assert_eq!(
        recoveries.load(Ordering::SeqCst),
        1,
        "the recovery must fire exactly once, on the success that ended the failure"
    );
}
